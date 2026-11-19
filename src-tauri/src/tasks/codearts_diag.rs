//! CodeArts 诊断任务（--task-run codearts-diag）：用真实凭据请求关键端点，
//! 把原始响应写入 %APPDATA%\AIWorkAssistant\data\codearts_diag.json。
//! 仅用于排查积分解析 / 模型目录 / 反代签名，不参与正常业务。

use serde_json::{json, Value};
use std::io::Write;

use crate::state::AppState;
use crate::tasks::codearts_common;
use crate::tasks::codearts_sign;

/// 签名 GET，返回 (status, body 文本)。
fn signed_get(identity: &codearts_sign::CodeartsIdentity, url: &str, extra: &[(&str, &str)]) -> (u16, String) {
    let signed = match codearts_sign::sign(identity, "GET", url, b"", extra) {
        Ok(s) => s,
        Err(e) => return (0, format!("SIGN_ERR: {e}")),
    };
    let agent = crate::tasks::http_agent(20);
    let mut req = agent.get(url)
        .set("Authorization", &signed.authorization)
        .set("x-sdk-date", &signed.x_sdk_date)
        .set("content-type", &signed.content_type);
    if let Some(st) = &signed.x_security_token { req = req.set("x-security-token", st.as_str()); }
    if let Some(sha) = &signed.x_sdk_content_sha256 { req = req.set("x-sdk-content-sha256", sha.as_str()); }
    for (k, v) in &signed.extra { req = req.set(k.as_str(), v.as_str()); }
    match req.call() {
        Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(e) => (0, format!("NET_ERR: {e}")),
    }
}

/// 签名 POST（body 字节级一致），返回 (status, body)。
fn signed_post(identity: &codearts_sign::CodeartsIdentity, url: &str, body: &[u8]) -> (u16, String) {
    let signed = match codearts_sign::sign(identity, "POST", url, body, &[]) {
        Ok(s) => s,
        Err(e) => return (0, format!("SIGN_ERR: {e}")),
    };
    let agent = crate::tasks::http_agent(60);
    let mut req = agent.post(url)
        .set("Authorization", &signed.authorization)
        .set("x-sdk-date", &signed.x_sdk_date)
        .set("content-type", &signed.content_type);
    if let Some(st) = &signed.x_security_token { req = req.set("x-security-token", st.as_str()); }
    if let Some(sha) = &signed.x_sdk_content_sha256 { req = req.set("x-sdk-content-sha256", sha.as_str()); }
    // 社区参考：Chat-Id/Session-Id/lang 头不参与签名
    req = req.set("Chat-Id", "diag-chat-001").set("Session-Id", "diag-session-001").set("lang", "zh-cn");
    match req.send_bytes(body) {
        Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(e) => (0, format!("NET_ERR: {e}")),
    }
}

/// 诊断入口：对所有有凭据的 CodeArts 账号抓三个端点原始响应。
pub fn run(state: &AppState) -> i32 {
    // 让 codearts_catalog::resolve 能读到真实目录缓存
    crate::tasks::codearts_catalog::set_data_dir(&state.data_dir);
    let pool = crate::store::docs::codearts_pool_load(&crate::store::db(&state.data_dir));
    let accounts = pool.get("accounts").and_then(Value::as_array).cloned().unwrap_or_default();
    if accounts.is_empty() {
        println!("{}", json!({"ok":false,"error":"无 CodeArts 账号"}));
        return 1;
    }
    let mut out = json!({ "generated_at": chrono::Local::now().to_rfc3339(), "accounts": [] });
    for a in &accounts {
        let id = a.get("id").and_then(Value::as_str).unwrap_or("");
        let creds = match codearts_common::ensure_fresh_creds(&state.data_dir, id) {
            Ok(c) => c,
            Err(e) => {
                out["accounts"].as_array_mut().unwrap().push(json!({"id":id,"error":format!("creds: {e}")}));
                continue;
            }
        };
        if !creds.is_valid() {
            out["accounts"].as_array_mut().unwrap().push(json!({"id":id,"error":"无有效凭据"}));
            continue;
        }
        let identity = codearts_sign::identity_from_creds(&creds);
        let base = "https://snap-access.cn-north-4.myhuaweicloud.com";
        let mut rec = json!({ "id": id, "is_oauth": creds.is_oauth(), "expires_at": creds.expires_at });

        // 积分统计
        let (st, body) = signed_get(&identity, &format!("{base}/snap-manager/v1/statistics/plugin"), &[("agent-type","PromptCenter")]);
        rec["stats_status"] = json!(st);
        rec["stats_body"] = json!(body.chars().take(8000).collect::<String>());

        // 模型目录（builtin）：必须带小写 agent-type 头，否则 400 TM.00001001
        let (st2, body2) = signed_get(&identity, &format!("{base}/v1/model/builtin"), &[("agent-type","PromptCenter")]);
        rec["builtin_status"] = json!(st2);
        rec["builtin_body"] = json!(body2.chars().take(12000).collect::<String>());

        // ops delivery（签到活动，顺带验证 ops 签名）
        let (st3, body3) = signed_get(&identity, &format!("{base}/v1/ops/delivery?channel=IDE"), &[("agent-type","PromptCenter")]);
        rec["ops_status"] = json!(st3);
        rec["ops_body"] = json!(body3.chars().take(6000).collect::<String>());

        // 网关路径完整模拟：dispatch 用客户端模型名 resolve → 拿 upstream model_id → 发 chat。
        // 这验证「客户端发 codearts 模型名 → 网关能路由并真实调用」的整条链路。
        let resolved = crate::tasks::codearts_catalog::resolve("GLM-5.2");
        rec["gateway_resolve"] = json!(resolved);
        let model_id = resolved.as_ref().map(|(_, m)| m.as_str()).unwrap_or("GLM-5.2");
        let chat_body = serde_json::json!({
            "model": model_id,
            "messages": [{"role":"user","content":"hi"}],
            "max_tokens": 8,
            "stream": false
        }).to_string();
        let (st4, body4) = signed_post(&identity, &format!("{base}/api/v2/chat/completions"), chat_body.as_bytes());
        rec["chat_status"] = json!(st4);
        rec["chat_body"] = json!(body4.chars().take(4000).collect::<String>());
        // 判断是否是有效 OpenAI chat 响应
        let valid = body4.contains("chat.completion") && body4.contains("choices");
        rec["gateway_chat_valid"] = json!(valid);

        out["accounts"].as_array_mut().unwrap().push(rec);
    }
    let path = state.data_dir.join("data").join("codearts_diag.json");
    let text = serde_json::to_string_pretty(&out).unwrap_or_default();
    match std::fs::File::create(&path).and_then(|mut f| f.write_all(text.as_bytes())) {
        Ok(()) => {
            println!("{}", json!({"ok":true,"path":path.to_string_lossy()}));
            0
        }
        Err(e) => {
            println!("{}", json!({"ok":false,"error":format!("写诊断文件失败: {e}")}));
            1
        }
    }
}
