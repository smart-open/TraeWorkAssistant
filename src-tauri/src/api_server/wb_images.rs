//! 生图双端点投影（T5.4/F-63）
//!
//! `/v1/images/generations`（文生图）+ `/v1/images/edits`（图生图）→ 上游
//! `POST {chat_base}/v2/images/generations`（沿用对话上游 headers 三铁律，
//! Accept 换 application/json，非流式单发）。
//!
//! F-63 验收红线：**上游不支持生图时明示报错不静默**——404 / 明确不支持
//! 消息 → 501 + 中文错误说明；模型目录声明不支持图片模态（supports_image
//! = false）→ 400。响应宽容解析：上游 data[].url / b64_json / image_url
//! 三种形态归一为 OpenAI images 格式。

use std::io::Read;

use serde_json::{json, Value};

use super::wb_upstream::{build_chat_headers, wb_agent, WbCreds};

/// 上游生图端点（未在调研中实测验证——投影层按此路径请求，
/// 上游 404/不支持时按 F-63 红线明示 501）
pub const UPSTREAM_PATH: &str = "/v2/images/generations";

/// 校验请求：模型存在且声明支持图片模态；prompt 非空；edits 必须带 image
pub fn validate(
    catalog: &[super::wb_catalog::WbModel],
    model: &str,
    prompt: &str,
    image_b64: Option<&str>,
) -> Result<(), (u16, String)> {
    let m = super::wb_catalog::find(catalog, model).ok_or_else(|| {
        (
            400u16,
            format!("model {model} 不在 WorkBuddy 模型目录（/v1/models），无法投影生图请求"),
        )
    })?;
    if !m.supports_image {
        return Err((
            400,
            format!("模型 {model} 目录声明不支持图片模态（supports_image=false），生图请求被拒绝"),
        ));
    }
    if prompt.trim().is_empty() {
        return Err((400, "prompt: field required（生图提示词不能为空）".into()));
    }
    if let Some(img) = image_b64 {
        if img.trim().is_empty() {
            return Err((400, "image: 生图编辑请求必须提供图像（base64 或 data URL）".into()));
        }
    }
    Ok(())
}

/// 发起上游生图请求并归一化为 OpenAI images 响应
pub fn generate(c: &WbCreds, body: &Value) -> Result<Value, (u16, String)> {
    let url = format!("{}{}", c.chat_base(), UPSTREAM_PATH);
    let payload = json!({
        "model": body.get("model").cloned().unwrap_or(json!("")),
        "prompt": body.get("prompt").cloned().unwrap_or(json!("")),
        "n": body.get("n").cloned().unwrap_or(json!(1)),
        "size": body.get("size").cloned().unwrap_or(json!("1024x1024")),
        "image": body.get("image").cloned().unwrap_or(Value::Null),
        "stream": false,
    });
    let mut req = wb_agent()
        .post(&url)
        .timeout(std::time::Duration::from_secs(180));
    for (k, v) in build_chat_headers(c) {
        // 生图非流式：accept 覆盖为 JSON（build_chat_headers 默认 text/event-stream）
        let v = if k.eq_ignore_ascii_case("accept") { "application/json".into() } else { v };
        req = req.set(k, &v);
    }
    let resp = match req.send_string(&payload.to_string()) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body_text = r.into_string().unwrap_or_default();
            return Err(upstream_unsupported(code, &body_text));
        }
        Err(e) => return Err((502, format!("上游生图请求失败: {e}"))),
    };
    let mut text = String::new();
    resp.into_reader()
        .read_to_string(&mut text)
        .map_err(|e| (502u16, format!("上游生图响应读取失败: {e}")))?;
    let up: Value = serde_json::from_str(&text)
        .map_err(|e| (502u16, format!("上游生图响应非 JSON: {e}")))?;
    if let Some(err) = up.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("上游生图错误");
        return Err(upstream_unsupported(200, msg));
    }
    normalize_images_response(&up)
}

/// 上游不支持生图的明示判定（F-63 红线：不静默）
fn upstream_unsupported(status: u16, body: &str) -> (u16, String) {
    let lower = body.to_lowercase();
    if status == 404
        || lower.contains("not found")
        || lower.contains("不支持")
        || lower.contains("unsupported")
        || lower.contains("no such")
        || lower.contains("invalid url")
    {
        return (
            501,
            "上游 WorkBuddy/CodeBuddy 当前未提供生图端点（F-63 投影层已就绪，等待上游能力开放）".into(),
        );
    }
    (
        if status == 200 { 502 } else { status },
        format!("上游生图错误（HTTP {status}）: {}", &body.chars().take(200).collect::<String>()),
    )
}

/// 上游响应 → OpenAI images 归一：兼容 data[].url / b64_json / image_url
fn normalize_images_response(up: &Value) -> Result<Value, (u16, String)> {
    let arr = up
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .or_else(|| up.get("images").and_then(|d| d.as_array()).cloned())
        .ok_or_else(|| {
            (
                502u16,
                "上游生图响应缺少 data/images 数组，无法归一为 OpenAI 格式".into(),
            )
        })?;
    let data: Vec<Value> = arr
        .iter()
        .map(|item| {
            let url = item
                .get("url")
                .and_then(|v| v.as_str())
                .or_else(|| item.pointer("/image_url/url").and_then(|v| v.as_str()))
                .or_else(|| item.pointer("/image_url").and_then(|v| v.as_str()));
            let b64 = item
                .get("b64_json")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("image_base64").and_then(|v| v.as_str()));
            let mut o = json!({});
            if let Some(u) = url {
                o["url"] = json!(u);
            }
            if let Some(b) = b64 {
                o["b64_json"] = json!(b);
            }
            if let Some(rp) = item.get("revised_prompt") {
                o["revised_prompt"] = rp.clone();
            }
            o
        })
        .collect();
    Ok(json!({
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "data": data,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::wb_catalog;

    #[test]
    fn validate_rejects_non_image_model() {
        let c = wb_catalog::builtin();
        // hy3-x 声明不支持图片
        let e = validate(&c, "hy3-x", "画一只猫", None).unwrap_err();
        assert_eq!(e.0, 400);
        assert!(e.1.contains("supports_image=false"));
    }

    #[test]
    fn validate_rejects_unknown_model_and_empty_prompt() {
        let c = wb_catalog::builtin();
        assert_eq!(validate(&c, "not-a-model", "x", None).unwrap_err().0, 400);
        assert_eq!(validate(&c, "hy4", "  ", None).unwrap_err().0, 400);
        assert_eq!(validate(&c, "hy4", "ok", Some(" ")).unwrap_err().0, 400);
        // 支持图片的模型 + 正常 prompt 通过
        assert!(validate(&c, "hy4", "画一只猫", None).is_ok());
        assert!(validate(&c, "kimi-k3-1", "画一只猫", Some("data:image/png;base64,AAA")).is_ok());
    }

    #[test]
    fn normalize_maps_three_upstream_shapes() {
        let up = json!({"data":[{"url":"https://x/a.png"},{"b64_json":"AAA"},{"image_url":{"url":"https://y/b.png"}}]});
        let v = normalize_images_response(&up).unwrap();
        let d = v["data"].as_array().unwrap();
        assert_eq!(d[0]["url"], json!("https://x/a.png"));
        assert_eq!(d[1]["b64_json"], json!("AAA"));
        assert_eq!(d[2]["url"], json!("https://y/b.png"));
        // images 数组形态
        let up2 = json!({"images":[{"url":"https://z/c.png"}]});
        assert_eq!(normalize_images_response(&up2).unwrap()["data"][0]["url"], json!("https://z/c.png"));
        // 缺数组 → 明示错误
        assert!(normalize_images_response(&json!({"foo":1})).is_err());
    }

    #[test]
    fn unsupported_detection_covers_404_and_phrases() {
        assert_eq!(upstream_unsupported(404, "").0, 501);
        assert_eq!(upstream_unsupported(200, "endpoint not found").0, 501);
        assert_eq!(upstream_unsupported(200, "该功能不支持").0, 501);
        assert_eq!(upstream_unsupported(400, "invalid prompt").0, 400);
    }
}
