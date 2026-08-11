//! JWT 解析（不校验签名，仅本地展示用途）。
//! 支持 `Cloud-IDE-JWT <token>` 前缀；payload 取 data.id 与 exp。

pub struct JwtInfo {
    pub user_id: Option<String>,
    pub exp_hours: Option<f64>,
}

pub fn parse(jwt_full: &str) -> JwtInfo {
    let token = jwt_full
        .strip_prefix("Cloud-IDE-JWT ")
        .unwrap_or(jwt_full);
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return JwtInfo {
            user_id: None,
            exp_hours: None,
        };
    }
    let pad = format!("{}{}", parts[1], "=".repeat(parts[1].len() % 4));
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(pad) else {
        return JwtInfo {
            user_id: None,
            exp_hours: None,
        };
    };
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return JwtInfo {
            user_id: None,
            exp_hours: None,
        };
    };
    let user_id = payload
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| payload.get("auth_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .or_else(|| payload.get("sub").and_then(|v| v.as_str()).map(|s| s.to_string()));

    let exp_hours = payload.get("exp").and_then(|v| v.as_i64()).map(|exp| {
        let now = chrono::Utc::now().timestamp();
        (exp - now) as f64 / 3600.0
    });

    JwtInfo { user_id, exp_hours }
}

/// 由 exp 剩余小时数推导状态：>24 ok / <=24 && >0 warn / <=0 expired
pub fn status_of(exp_hours: Option<f64>) -> &'static str {
    match exp_hours {
        Some(h) if h > 24.0 => "ok",
        Some(h) if h > 0.0 => "warn",
        Some(_) => "expired",
        None => "unknown",
    }
}
