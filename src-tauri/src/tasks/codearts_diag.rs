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
    for (k, v) in identity.client_headers() { req = req.set(k, v.as_str()); }
    match req.call() {
        Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(e) => (0, format!("NET_ERR: {e}")),
    }
}

/// 无签名 GET（IAM token 直查类端点用 `X-Auth-Token`，不走 AK/SK 签名）。
fn plain_get_with_token(url: &str, token: &str) -> (u16, String) {
    let agent = crate::tasks::http_agent(20);
    match agent.get(url).set("X-Auth-Token", token).call() {
        Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(e) => (0, format!("NET_ERR: {e}")),
    }
}

/// 从任意响应体里尽力抽取 user_id / domain_id（多候选键，宽容解析）。
///
/// 为什么不在探针阶段定死字段名：三个候选端点的信封各不相同（STS 是
/// `{user:{...}}` / snap-manager 是业务信封），先取证再定口径，避免凭猜测
/// 写死一个错的键名。
fn extract_identity(body: &str) -> (String, String) {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return (String::new(), String::new());
    };
    // 递归找第一个字符串值：候选键按优先级
    fn find(v: &Value, keys: &[&str], depth: usize) -> String {
        if depth > 4 {
            return String::new();
        }
        if let Some(o) = v.as_object() {
            for k in keys {
                if let Some(s) = o.get(*k).and_then(Value::as_str) {
                    if !s.trim().is_empty() {
                        return s.to_string();
                    }
                }
            }
            for (_, child) in o {
                let hit = find(child, keys, depth + 1);
                if !hit.is_empty() {
                    return hit;
                }
            }
        } else if let Some(a) = v.as_array() {
            for child in a {
                let hit = find(child, keys, depth + 1);
                if !hit.is_empty() {
                    return hit;
                }
            }
        }
        String::new()
    }
    let uid = find(&v, &["user_id", "userId", "userid", "user_account_id", "account_id", "id"], 0);
    let dom = find(&v, &["domain_id", "domainId", "domainid", "domain"], 0);
    (uid, dom)
}

/// 身份探针：只用只读 GET，不改任何请求路径。
///
/// 三个候选来源（按「与真实客户端同源」优先级）：
/// ① snap-manager/v1/user/project —— 真实 IDE 启动时必调，AK/SK 签名；
/// ② snap-manager/v1/member/roles —— 同域业务信封；
/// ③ STS v5/caller-identity —— IAM token 直查（走 X-Auth-Token，不签名）。
///
/// 只要任一来源给出非空 user_id/domain_id 即可判定账号归属主账号。
fn probe_identity(identity: &codearts_sign::CodeartsIdentity, creds: &codearts_common::CodeartsCreds) -> Value {
    let base = "https://snap-access.cn-north-4.myhuaweicloud.com";
    let mut out = json!({});

    // ① 真实客户端同源：user/project（无 agent-type，只需 X-Language）
    let (st1, b1) = signed_get(identity, &format!("{base}/snap-manager/v1/user/project"), &[]);
    let (u1, d1) = extract_identity(&b1);
    out["user_project"] = json!({
        "status": st1, "user_id": u1, "domain_id": d1,
        "body": b1.chars().take(1500).collect::<String>(),
    });

    // ② member/roles
    let (st2, b2) = signed_get(identity, &format!("{base}/snap-manager/v1/member/roles"), &[]);
    let (u2, d2) = extract_identity(&b2);
    out["member_roles"] = json!({
        "status": st2, "user_id": u2, "domain_id": d2,
        "body": b2.chars().take(1500).collect::<String>(),
    });

    // ③ STS caller-identity：只认 IAM token，绝不发 Authorization
    let (st3, b3) = plain_get_with_token(
        "https://sts.cn-north-4.myhuaweicloud.com/v5/caller-identity",
        &creds.security_token,
    );
    let (u3, d3) = extract_identity(&b3);
    out["sts_caller_identity"] = json!({
        "status": st3, "user_id": u3, "domain_id": d3,
        "body": b3.chars().take(1500).collect::<String>(),
    });

    // 汇总：按优先级取第一个非空组合
    let (uid, dom, src) = if !u1.is_empty() || !d1.is_empty() {
        (u1, d1, "user_project")
    } else if !u2.is_empty() || !d2.is_empty() {
        (u2, d2, "member_roles")
    } else if !u3.is_empty() || !d3.is_empty() {
        (u3, d3, "sts_caller_identity")
    } else {
        (String::new(), String::new(), "")
    };
    out["resolved"] = json!({ "user_id": uid, "domain_id": dom, "source": src });
    out
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
    // 账号级客户端指纹头：与生产路径 `send_signed_request` 同口径，
    // 让诊断验证的是完整生产头集合而不是精简子集
    for (k, v) in identity.client_headers() {
        req = req.set(k, v.as_str());
    }
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
        let identity = codearts_sign::identity_from_creds(&creds, id);
        let base = "https://snap-access.cn-north-4.myhuaweicloud.com";
        let mut rec = json!({ "id": id, "is_oauth": creds.is_oauth(), "expires_at": creds.expires_at });

        // 指纹设备段：验证「不同账号 = 不同调用来源」（不含凭据明文）
        rec["fingerprint"] = json!({
            "device_segment": identity.fingerprint.device_segment(),
            "ide_name": codearts_sign::IDE_NAME,
            "ide_version": codearts_sign::IDE_VERSION,
            "plugin_name": codearts_sign::PLUGIN_NAME,
            "plugin_version": codearts_sign::PLUGIN_VERSION,
        });

        // 身份探针（只读 GET）：判定账号归属主账号 / 子账号
        rec["identity"] = probe_identity(&identity, &creds);

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
