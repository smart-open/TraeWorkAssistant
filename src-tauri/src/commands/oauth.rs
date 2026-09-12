use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::State;

use crate::fs_utils;
use crate::jwt;
use crate::models::RawAccount;
use crate::state::AppState;

/// 最近签发的 OAuth state（CSRF 防护）：oauth_get_login_url 签发时记录，
/// oauth_parse_callback 在回调携带 state 且本进程签发过时强校验一致性
static LAST_OAUTH_STATE: Mutex<Option<String>> = Mutex::new(None);

/// OAuth 常量
const OAUTH_CLIENT_ID: &str = "en1oxy7wnw8j9n";
const OAUTH_CLIENT_SECRET: &str = "-";
const OAUTH_APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:17388/authorize";

/// OAuth 回调解析结果
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OAuthCallbackInfo {
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub avatar: Option<String>,
}

/// OAuth 登录 URL 响应
#[derive(Serialize)]
pub struct OAuthLoginUrl {
    pub url: String,
    pub state: String,
    pub redirect_uri: String,
}

/// OAuth 登录完成后的账号信息
#[derive(Serialize)]
pub struct OAuthLoginResult {
    pub user_id: String,
    pub name: String,
    pub jwt: String,
    pub refresh_token: String,
    pub has_refresh_token: bool,
}

/// 短请求 Agent（项目未启用 ureq 的 proxy-from-env feature，Agent 默认直连）
fn short_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// 生成随机 hex 字符串。
/// 熵源：OS CSPRNG（Windows BCryptGenRandom 系统首选 RNG）。旧 LCG 以时间戳作种子，
/// 输出可预测，不适合 OAuth state / machine_id 等安全场景（审查 P2）；BCrypt 失败时
/// 保留 LCG 兜底（仅影响随机性，不中断流程）。
pub(crate) fn random_hex(len: usize) -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        let mut bytes = vec![0u8; len.div_ceil(2)];
        let halg: windows_sys::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE =
            unsafe { std::mem::zeroed() };
        // STATUS_SUCCESS == 0
        let status = unsafe {
            BCryptGenRandom(halg, bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status == 0 {
            let mut out = String::with_capacity(len);
            for b in bytes {
                if out.len() >= len {
                    break;
                }
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
                if out.len() >= len {
                    break;
                }
                out.push(char::from_digit((b & 0xF) as u32, 16).unwrap_or('0'));
            }
            return out;
        }
    }
    // 兜底：旧 LCG（仅非 Windows 或 BCrypt 调用失败时）
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42);
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        // 简单 LCG
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let nibble = ((seed >> 32) & 0xF) as u8;
        out.push(if nibble < 10 {
            (b'0' + nibble) as char
        } else {
            (b'a' + nibble - 10) as char
        });
    }
    out
}

/// 生成 OAuth 登录 URL
#[tauri::command]
pub fn oauth_get_login_url() -> OAuthLoginUrl {
    let state = random_hex(32);
    let machine_id = random_hex(32);
    let device_id: String = (0..15).map(|_| {
        let n = (random_hex(2).chars().next().unwrap() as u8).wrapping_rem(10);
        (b'0' + n) as char
    }).collect();

    let url = format!(
        "https://www.trae.cn/authorization?\
        client_id={client_id}\
        &client_secret={client_secret}\
        &app_id={app_id}\
        &auth_callback_url={redirect_uri}\
        &state={state}\
        &machine_id={machine_id}\
        &device_id={device_id}\
        &response_type=code",
        client_id = OAUTH_CLIENT_ID,
        client_secret = OAUTH_CLIENT_SECRET,
        app_id = OAUTH_APP_ID,
        redirect_uri = urlencoding::encode(OAUTH_REDIRECT_URI),
        state = state,
        machine_id = machine_id,
        device_id = device_id,
    );

    OAuthLoginUrl {
        url,
        // 记录最近签发的 state 供回调校验（CSRF）
        state: {
            *LAST_OAUTH_STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(state.clone());
            state
        },
        redirect_uri: OAUTH_REDIRECT_URI.to_string(),
    }
}

/// 解析 OAuth 回调 URL
#[tauri::command]
pub fn oauth_parse_callback(callback_url: String) -> Result<OAuthCallbackInfo, String> {
    // 回调 URL 格式：http://127.0.0.1:port/authorize?refreshToken=xxx&accessToken=xxx&userId=xxx&userName=xxx&avatar=xxx
    // 或可能带 code 参数需要交换
    let query_str = callback_url
        .split('?')
        .nth(1)
        .ok_or_else(|| "回调 URL 中缺少查询参数".to_string())?;

    let mut params: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for pair in query_str.split('&') {
        let mut kv = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("").to_string();
        let value = kv.next().unwrap_or("").to_string();
        // URL decode
        let decoded = urlencoding::decode(&value)
            .map(|c| c.to_string())
            .unwrap_or(value);
        params.insert(key, decoded);
    }

    // 优先从 refreshToken 参数获取
    let refresh_token = params
        .get("refreshToken")
        .or_else(|| params.get("refresh_token"))
        .cloned()
        .ok_or_else(|| "回调 URL 中缺少 refreshToken 参数".to_string())?;

    let access_token = params
        .get("accessToken")
        .or_else(|| params.get("access_token"))
        .cloned();

    let user_id = params
        .get("userId")
        .or_else(|| params.get("user_id"))
        .or_else(|| params.get("UserID"))
        .cloned();

    let user_name = params
        .get("userName")
        .or_else(|| params.get("user_name"))
        .or_else(|| params.get("nickname"))
        .cloned();

    let avatar = params.get("avatar").cloned();

    // CSRF 校验（审查 P2）：回调携带 state 且本进程签发过 state 时，两者必须一致；
    // 不一致的回调 URL 可能来自伪造/重放，直接拒绝。回调不带 state（旧流程/第三方拼 URL）
    // 或本进程从未签发过（如重启后直接粘贴回调）时保持宽容，不阻断正常登录
    if let Some(cb_state) = params.get("state") {
        let issued = LAST_OAUTH_STATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(expected) = issued {
            if !expected.is_empty() && cb_state != &expected {
                return Err("OAuth state 校验失败：回调 URL 与本机发起的登录请求不匹配（可能为伪造或重放），已拒绝".into());
            }
        }
    }

    Ok(OAuthCallbackInfo {
        refresh_token,
        access_token,
        user_id,
        user_name,
        avatar,
    })
}

/// ExchangeToken：用 refresh_token 换取 access_token
fn exchange_token(refresh_token: &str) -> Result<(String, Option<String>), String> {
    let resp = short_agent()
        .post("https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken")
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({
            "ClientID": OAUTH_CLIENT_ID,
            "RefreshToken": refresh_token,
            "ClientSecret": OAUTH_CLIENT_SECRET,
            "UserID": ""
        }))
        .map_err(|e| format!("ExchangeToken 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误");
        return Err(format!("ExchangeToken 失败 (code={}): {}", code, msg));
    }

    let data = body.get("data").ok_or("响应中缺少 data 字段")?;

    let access_token = data
        .get("access_token")
        .or_else(|| data.get("token"))
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 access_token")?;

    let new_refresh_token = data
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok((access_token.to_string(), new_refresh_token))
}

/// GetUserInfo：获取用户信息
fn get_user_info(access_token: &str) -> Result<(String, String), String> {
    let auth = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    let resp = short_agent()
        .post("https://api.trae.com.cn/cloudide/api/v3/trae/GetUserInfo")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({}))
        .map_err(|e| format!("GetUserInfo 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let data = body.get("data").or(body.get("result")).ok_or("响应中缺少 data 字段")?;

    let user_id = data
        .get("user_id")
        .or_else(|| data.get("UserID"))
        .or_else(|| data.get("userId"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let user_name = data
        .get("name")
        .or_else(|| data.get("user_name"))
        .or_else(|| data.get("userName"))
        .or_else(|| data.get("nickname"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((user_id, user_name))
}

/// OAuth 登录闭环：解析回调 → 换取 accessToken → 获取用户信息 → 保存账号
#[tauri::command]
pub fn oauth_login(
    state: State<AppState>,
    callback_url: String,
    account_name: Option<String>,
    group_id: Option<String>,
) -> Result<OAuthLoginResult, String> {
    // 1. 解析回调 URL
    let callback_info = oauth_parse_callback(callback_url)?;

    // 2. 如果回调中没有 accessToken，则用 refresh_token 换取
    let (access_token, new_refresh_token) = if let Some(ref at) = callback_info.access_token {
        (at.clone(), None)
    } else {
        exchange_token(&callback_info.refresh_token)?
    };

    // 3. 规范化 JWT 格式
    let jwt = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.clone()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    // 4. 解析 JWT 获取 user_id
    let jwt_info = jwt::parse(&jwt);
    let user_id = callback_info
        .user_id
        .clone()
        .or_else(|| jwt_info.user_id.clone())
        .ok_or_else(|| "无法从回调或 JWT 中获取 user_id".to_string())?;

    // 5. 尝试获取用户名
    let name = account_name
        .or(callback_info.user_name.clone())
        .or_else(|| {
            // 尝试调用 GetUserInfo
            get_user_info(&jwt)
                .map(|(uid, uname)| if uname.is_empty() { uid } else { uname })
                .ok()
        })
        .unwrap_or_else(|| {
            // 按字符截取（字节切片在多字节 UTF-8 边界处会 panic）
            let head: String = user_id.chars().take(8).collect();
            format!("账号_{head}")
        });

    // 6. 确定最终的 refresh_token（优先使用 ExchangeToken 返回的新 token）
    let final_refresh_token = new_refresh_token
        .unwrap_or_else(|| callback_info.refresh_token.clone());

    // 7. 检查账号是否已存在
    let mut accounts = crate::vault::load_accounts(&state);
    if accounts
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(&user_id))
    {
        // 已存在：更新 JWT 和 refresh_token
        let acct = accounts
            .accounts
            .iter_mut()
            .find(|a| a.user_id.as_deref() == Some(&user_id))
            .unwrap();
        acct.jwt = jwt.clone();
        acct.refresh_token = Some(final_refresh_token.clone());
        acct.updated_at = Some(fs_utils::now_iso());
        crate::vault::save_accounts(&state, &mut accounts)?;

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：更新已有账号 [{}] jwt + refresh_token", name),
        );
    } else {
        // 新账号
        accounts.accounts.push(RawAccount {
            name: name.clone(),
            user_id: Some(user_id.clone()),
            jwt: jwt.clone(),
            refresh_token: Some(final_refresh_token.clone()),
            added_at: Some(fs_utils::now_iso()),
            updated_at: Some(fs_utils::now_iso()),
            dc_id: None,
        });
        crate::vault::save_accounts(&state, &mut accounts)?;

        // 设置分组
        if let Some(g) = group_id {
            let mut groups: crate::models::GroupsFile =
                fs_utils::read_json(&state.path("groups.json"));
            groups.membership.insert(user_id.clone(), g);
            fs_utils::write_json(&state.path("groups.json"), &groups)?;
        }

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：新增账号 [{}] user_id={}", name, user_id),
        );
    }

    Ok(OAuthLoginResult {
        user_id: user_id.clone(),
        name,
        jwt,
        refresh_token: final_refresh_token,
        has_refresh_token: true,
    })
}
