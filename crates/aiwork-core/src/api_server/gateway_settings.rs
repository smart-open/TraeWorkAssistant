//! 网关设置独立归属（unified-api-gateway-design §8.1）
//!
//! `data/api_gateway_settings.json`：`port / default_model`——网关设置从
//! app_settings.json 抽离，归属与"公共网关"定位一致。
//!
//! 一次性迁移：新文件缺失时从 `conf/app_settings.json` 的旧字段
//! （api_port / api_default_model）抽取并落盘新文件；**旧字段保留不删**
//! （防回滚），但网关启动不再读取。

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySettings {
    /// 网关监听端口（默认 7864，与既有约定一致）
    #[serde(default = "default_port")]
    pub port: u16,
    /// 默认模型（请求未指定 model 时使用；统一目录内模型）
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub updated_at: i64,
}

fn default_port() -> u16 {
    7864
}

fn default_model() -> String {
    crate::api_server::DEFAULT_MODEL.to_string()
}

impl Default for GatewaySettings {
    fn default() -> Self {
        GatewaySettings {
            port: default_port(),
            default_model: default_model(),
            updated_at: 0,
        }
    }
}

/// 规范化：default_model 空 → 内置默认
fn normalized(mut s: GatewaySettings) -> GatewaySettings {
    if s.default_model.trim().is_empty() {
        s.default_model = default_model();
    }
    s
}

/// 读取网关设置；kv 文档缺失时从 kv `app_settings` 旧字段一次性迁移
/// （迁移即落盘 kv；旧字段保留不删，防回滚，但不再读取）。
/// SQLite 化（P2）：原 data/api_gateway_settings.json → kv 键 `api_gateway_settings`。
pub fn load(data_dir: &Path) -> GatewaySettings {
    let store = crate::store::db(data_dir);
    if let Some(text) = store.kv_get_raw("api_gateway_settings") {
        if let Ok(s) = serde_json::from_str::<GatewaySettings>(&text) {
            return normalized(s);
        }
    }
    // 一次性迁移：旧字段缺失/损坏均回退默认值（read_json 语义一致）
    let legacy: serde_json::Value = store.kv_get("app_settings");
    let port = legacy
        .get("api_port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0 && *p <= u64::from(u16::MAX))
        .map(|p| p as u16)
        .unwrap_or_else(default_port);
    let model = legacy
        .get("api_default_model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let s = normalized(GatewaySettings {
        port,
        default_model: model,
        updated_at: 0,
    });
    // 迁移落盘失败不阻塞启动（下次启动重试），内存值仍生效
    let _ = store.kv_set("api_gateway_settings", &s);
    s
}

/// 保存网关设置（端口/模型合法性由调用方校验后传入；这里兜底端口范围）
pub fn save(data_dir: &Path, s: GatewaySettings) -> Result<(), String> {
    if s.port == 0 {
        return Err("端口无效（1-65535）".into());
    }
    let mut s = normalized(s);
    s.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::store::db(data_dir).kv_set("api_gateway_settings", &s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(app_settings: Option<serde_json::Value>) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "twa_gwset_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        if let Some(v) = app_settings {
            // SQLite 化（P2）：旧字段来源 = kv `app_settings`
            crate::store::db(&dir).kv_set_raw("app_settings", &v.to_string()).unwrap();
        }
        Fixture { dir }
    }

    /// 缺失迁移：app_settings 旧字段 → kv `api_gateway_settings`；旧键保留不删
    #[test]
    fn t01_migrates_from_app_settings_once() {
        let f = fixture(Some(json!({"api_port": 9000, "api_default_model": "glm-5.3"})));
        let s = load(&f.dir);
        assert_eq!(s.port, 9000);
        assert_eq!(s.default_model, "glm-5.3");
        // 迁移即落盘（kv）
        assert!(crate::store::db(&f.dir).kv_get_raw("api_gateway_settings").is_some());
        // 旧字段保留不删
        let legacy = crate::store::db(&f.dir).kv_get_raw("app_settings").unwrap();
        assert!(legacy.contains("9000"));
        // 二次读取走新键（且改旧键不再生效）
        let mut legacy: serde_json::Value = serde_json::from_str(&legacy).unwrap();
        legacy["api_port"] = json!(7777);
        crate::store::db(&f.dir).kv_set_raw("app_settings", &legacy.to_string()).unwrap();
        assert_eq!(load(&f.dir).port, 9000);
    }

    /// 无旧配置 → 默认值（7864 / deepseek-v4-flash），同样落盘
    #[test]
    fn t02_defaults_when_no_legacy() {
        let f = fixture(None);
        let s = load(&f.dir);
        assert_eq!(s.port, 7864);
        assert_eq!(s.default_model, "deepseek-v4-flash");
        assert!(crate::store::db(&f.dir).kv_get_raw("api_gateway_settings").is_some());
    }

    /// 已有配置 → 直接读 kv；save 往返 + updated_at
    #[test]
    fn t03_save_roundtrip() {
        let f = fixture(None);
        let _ = load(&f.dir); // 先迁移落盘
        save(
            &f.dir,
            GatewaySettings {
                port: 8000,
                default_model: "kimi-k3".into(),
                updated_at: 0,
            },
        )
        .unwrap();
        let s = load(&f.dir);
        assert_eq!(s.port, 8000);
        assert_eq!(s.default_model, "kimi-k3");
        assert!(s.updated_at > 0);
        // 空模型名兜底默认
        save(
            &f.dir,
            GatewaySettings {
                port: 8001,
                default_model: "  ".into(),
                updated_at: 0,
            },
        )
        .unwrap();
        assert_eq!(load(&f.dir).default_model, "deepseek-v4-flash");
        // 端口 0 拒绝
        assert!(save(
            &f.dir,
            GatewaySettings {
                port: 0,
                default_model: "kimi-k3".into(),
                updated_at: 0
            },
        )
        .is_err());
    }
}
