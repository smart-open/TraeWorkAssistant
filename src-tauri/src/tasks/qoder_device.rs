//! Qoder 设备指纹域（F-80 M3，§5.10 多账号并发；v1.2 用户决策：伪造是正式需求，
//! 以「每账号稳定绑定」而非随机轮换作为风险控制手段）。
//!
//! 移植自已验证的社区实现模型（docs/tmp/f80-qoder-recon/external-projects-recon.md）：
//! - QoderGateway（§2.3）：machine_id 账号级持久稳定入库；machine_token/machine_type
//!   每会话现场随机；服务端无强绑定校验
//! - QoderPatcher（§1.3/§6.1）：machine_id = 32 位小写 hex 无分隔符；device_id/umid
//!   = uuid4 标准格式。原实现为 md5(rand)，按归档 §6.1 建议改 uuid4 种子增熵——
//!   本实现用 sha256(uuid4)[..32] 产同格式 hex，不引入 md5 crate
//!
//! 注入优先级（tasks::qoder_common::effective_creds 合并）：
//! ① token store 有真实捕获 machine_id（client/mitm/cli 来源）→ 透传真实值；
//! ② 否则注入 device_profile.machine_id + 现场随机 machine_token；
//! ③ 全无 → 不带设备头（build_auth_headers 缺失即不带）。
//!
//! §5.10.2 本地存储覆写已实现：switcher/machine.rs apply_qoder_fingerprint 在切换/恢复
//! 成功后覆写本地 machineid 文件 + storage.json 遥测三键 + state.vscdb 服务端机器码
//! （挂点 switcher::apply_fingerprint_override）；本模块仍负责 API 请求侧指纹。

use sha2::{Digest, Sha256};

use crate::state::AppState;

/// 每账号稳定设备指纹（§5.10.1；入库 qoder_pool 账号记录，一次生成永不轮换）
#[derive(serde::Serialize, serde::Deserialize, Clone, Default, Debug)]
pub struct QoderDeviceProfile {
    /// 32 位小写 hex（Cosy-MachineId 注入源）
    #[serde(default)]
    pub machine_id: String,
    /// uuid4 标准格式（客户端设备 id 形态；ms_deviceid 同源，§1.4）
    #[serde(default)]
    pub device_id: String,
    /// uuid4 标准格式（umid 仅 macOS 强制，Windows 预留对齐字段）
    #[serde(default)]
    pub umid: String,
}

impl QoderDeviceProfile {
    /// 生成全新指纹（仅在账号入池/回填且无既有指纹时调用一次，此后稳定不变）
    pub fn generate() -> Self {
        Self {
            machine_id: machine_id_generate(),
            device_id: uuid::Uuid::new_v4().to_string(),
            umid: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// machine_id 生成：sha256(uuid4)[..32] —— 32 位小写 hex，与 QoderPatcher 的
/// md5(随机数) 同格式且熵更高（归档 §6.1 翻译建议）
pub fn machine_id_generate() -> String {
    let seed = uuid::Uuid::new_v4().to_string();
    let mut h = Sha256::new();
    h.update(seed.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    hex[..32].to_string()
}

/// Cosy-MachineToken 现场随机值（QoderGateway 取值模型：每会话随机，非持久绑定）
pub fn random_machine_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 指纹合并（纯函数便于测试；effective_creds 唯一调用）：
/// 真实捕获优先透传；缺失时注入账号绑定 machine_id + 现场随机 machine_token。
/// profile 缺失或 machine_id 为空时不注入（请求头自然不带设备头）。
pub fn merge_device_profile(creds: &mut crate::tasks::qoder_common::QoderCreds, profile: Option<&QoderDeviceProfile>) {
    if !creds.machine_id.is_empty() {
        return; // 真实捕获（client/mitm/cli 来源）优先
    }
    if let Some(p) = profile.filter(|p| !p.machine_id.is_empty()) {
        creds.machine_id = p.machine_id.clone();
        creds.machine_token = random_machine_token();
    }
}

/// 账号池惰性回填（幂等）：为缺指纹的存量账号生成 profile；已有不覆盖。
/// 直操 qoder_pool 原始 JSON（保留未知字段），所有签到/列表路径共用。
pub fn ensure_pool_profiles(state: &AppState) -> Result<(), String> {
    // I09：直操原始 JSON 保留未知字段（不可走 with_pool_mut），持池锁防并发整池覆盖丢写
    let _guard = state
        .qoder_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let db = crate::store::db(&state.data_dir);
    let mut pool = crate::store::docs::qoder_pool_load(&db);
    let Some(accounts) = pool.get_mut("accounts").and_then(|a| a.as_array_mut()) else {
        return Ok(());
    };
    let mut changed = 0usize;
    for a in accounts.iter_mut() {
        let has = a
            .get("device_profile")
            .and_then(|p| p.get("machine_id"))
            .and_then(|m| m.as_str())
            .is_some_and(|s| !s.is_empty());
        if !has {
            if let Some(o) = a.as_object_mut() {
                let profile = serde_json::to_value(QoderDeviceProfile::generate())
                    .map_err(|e| e.to_string())?;
                o.insert("device_profile".into(), profile);
                changed += 1;
            }
        }
    }
    if changed > 0 {
        crate::store::docs::qoder_pool_save(&db, &pool)?;
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("Qoder 设备指纹回填完成（本轮补齐 {changed} 个账号）"),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 格式基线：machine_id 32 位小写 hex；device_id/umid 为 uuid4 标准形态
    #[test]
    fn generated_profile_formats() {
        let p = QoderDeviceProfile::generate();
        assert_eq!(p.machine_id.len(), 32, "32 位 hex");
        assert!(p.machine_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(p.machine_id.chars().all(|c| !c.is_ascii_uppercase()), "小写");
        for u in [&p.device_id, &p.umid] {
            let parts: Vec<&str> = u.split('-').collect();
            assert_eq!(parts.iter().map(|s| s.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
            assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        }
    }

    /// 随机性：两次生成互异（防写死）；machine_token 现场随机
    #[test]
    fn generation_is_random() {
        let a = QoderDeviceProfile::generate();
        let b = QoderDeviceProfile::generate();
        assert_ne!(a.machine_id, b.machine_id);
        assert_ne!(a.device_id, b.device_id);
        assert_ne!(random_machine_token(), random_machine_token());
    }

    /// 注入优先级：真实捕获 wins（profile 不覆盖已有 machine_id）
    #[test]
    fn merge_prefers_captured_machine_id() {
        let mut creds = crate::tasks::qoder_common::QoderCreds {
            machine_id: "captured-real".into(),
            machine_token: "captured-token".into(),
            ..Default::default()
        };
        let p = QoderDeviceProfile::generate();
        merge_device_profile(&mut creds, Some(&p));
        assert_eq!(creds.machine_id, "captured-real");
        assert_eq!(creds.machine_token, "captured-token");
    }

    /// 注入：缺失时注入 profile.machine_id + 现场随机 machine_token
    #[test]
    fn merge_injects_profile_with_fresh_token() {
        let mut creds = crate::tasks::qoder_common::QoderCreds::default();
        let p = QoderDeviceProfile::generate();
        merge_device_profile(&mut creds, Some(&p));
        assert_eq!(creds.machine_id, p.machine_id);
        assert!(!creds.machine_token.is_empty());
        // 每次调用现场随机：同 profile 两次合并 token 互异（QoderGateway 模型）
        let mut creds2 = crate::tasks::qoder_common::QoderCreds::default();
        merge_device_profile(&mut creds2, Some(&p));
        assert_ne!(creds.machine_token, creds2.machine_token);
    }

    /// 全无：profile 缺失/空 machine_id 时不注入（请求头自然不带设备头）
    #[test]
    fn merge_without_profile_is_noop() {
        let mut creds = crate::tasks::qoder_common::QoderCreds::default();
        merge_device_profile(&mut creds, None);
        assert!(creds.machine_id.is_empty());
        assert!(creds.machine_token.is_empty());
        let empty = QoderDeviceProfile::default();
        merge_device_profile(&mut creds, Some(&empty));
        assert!(creds.machine_id.is_empty());
    }
}
