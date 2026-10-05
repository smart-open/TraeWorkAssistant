//! Qoder 账号域（F-80 M1 平移）：账号列表/改名/移除/PAT 导入/手动续期。
//!
//! 导入通道（§5.4）：M1 实装 **PAT 手工录入**（M0 R-3 侦察结论：CLI token 不明文落盘，
//! PAT 是最可靠且官方认可的通道）。
//!
//! Web 化改造：`#[tauri::command]` async 壳与 spawn_blocking 删除——cmd_bridge
//! handler 统一 spawn_blocking；热重载改 `reload_pools_after_change`。

use serde::Serialize;
use serde_json::Value;

use crate::fs_utils;
use crate::state::AppState;

use super::common::{account_id_of, load_pool, with_pool_mut, QoderAccount};
use crate::tasks::qoder_common::{self, QoderCreds};

#[derive(Serialize, Clone)]
pub struct QoderAccountView {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    pub phone_masked: String,
    pub plan: String,
    pub credential_source: String,
    pub token_expires_at: Option<i64>,
    pub needs_relogin: bool,
    pub relogin_reason: String,
    pub group_id: String,
    pub note: String,
    pub credits_balance: Option<f64>,
    pub credits_fetched_at: Option<String>,
    /// token store 中有可用凭证
    pub has_credential: bool,
    /// token 种类徽标：pat | client | unknown（脱敏，不含 token 本体）
    pub token_kind: String,
    /// 设备指纹徽标（§5.10：machine_id 前 8 位；None = 尚未回填）
    pub fingerprint: Option<String>,
    /// 完整设备指纹（§5.10；指纹查看弹框数据源）
    pub device_profile: Option<crate::tasks::qoder_device::QoderDeviceProfile>,
}

fn view_of(a: &QoderAccount, tokens: &Value) -> QoderAccountView {
    let rec = tokens
        .get("tokens")
        .and_then(|t| t.get(&a.id));
    let has_token = rec
        .and_then(|r| r.get("access_token").or_else(|| r.get("accessToken")))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let kind = rec
        .and_then(|r| r.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    QoderAccountView {
        id: a.id.clone(),
        uid: a.uid.clone(),
        nickname: a.nickname.clone(),
        phone_masked: a.phone_masked.clone(),
        plan: a.plan.clone(),
        credential_source: a.credential_source.clone(),
        token_expires_at: a.token_expires_at,
        needs_relogin: a.needs_relogin,
        relogin_reason: a.relogin_reason.clone(),
        group_id: a.group_id.clone(),
        note: a.note.clone(),
        credits_balance: a.credits_balance,
        credits_fetched_at: a.credits_fetched_at.clone(),
        has_credential: has_token,
        token_kind: kind,
        fingerprint: a
            .device_profile
            .as_ref()
            .filter(|p| !p.machine_id.is_empty())
            // I12：按字符截取，防非 ASCII machine_id 触发字节切片 panic
            .map(|p| p.machine_id.chars().take(8).collect::<String>()),
        device_profile: a.device_profile.clone(),
    }
}

/// 账号列表（含凭证状态；脱敏：只回 kind 徽标不回 token）。
/// 列表前惰性回填设备指纹（§5.10：存量账号幂等补齐，已有不覆盖）。
/// 回填失败仅记日志降级继续——只读列表不应被回填失败连带拖垮（I14 同款留痕惯例）
pub fn qoder_accounts_list(state: &AppState) -> Result<Vec<QoderAccountView>, String> {
    // P2 审查修复（main 版）：指纹回填为增强性操作，失败降级继续
    if let Err(e) = crate::tasks::qoder_device::ensure_pool_profiles(state) {
        fs_utils::app_log(&state.data_dir, &format!("Qoder 指纹回填失败（已降级，列表继续）: {e}"));
    }
    let accounts = load_pool(state);
    let tokens = qoder_common::load_token_store(state);
    Ok(accounts.iter().map(|a| view_of(a, &tokens)).collect())
}

/// 改名/备注（nickname 即展示名，可编辑覆盖 userinfo 值）。
/// 账号变更联动网关池热重载（与 Trae 侧 accounts.rs 同惯例；服务未运行时 no-op）。
/// 疑点③ 脏检查：值未实际变更时跳过落库后的网关池热重载——改名/改备注
/// 不影响网关调度（网关池消费的是凭证与分组，不消费 nickname/note），
/// 无变更重载会在调度进行中无谓打断并重建池连接
pub fn qoder_account_save(
    state: &AppState,
    user_id: String,
    name: Option<String>,
    note: Option<String>,
) -> Result<(), String> {
    // I09：持锁读-改-写，防并发整池覆盖丢更新
    let changed = with_pool_mut(state, |accounts| {
        let Some(a) = accounts.iter_mut().find(|a| a.id == user_id) else {
            return Err(format!("账号不在池中: {user_id}"));
        };
        let mut changed = false;
        if let Some(n) = name {
            let n = n.trim().to_string();
            if a.nickname != n {
                a.nickname = n;
                changed = true;
            }
        }
        if let Some(n) = note {
            if a.note != n {
                a.note = n;
                changed = true;
            }
        }
        Ok(changed)
    })?;
    if !changed {
        return Ok(());
    }
    crate::api_server::runtime::reload_pools_after_change(state);
    Ok(())
}

/// 移除账号（同步清理 token store 记录，防悬空凭证残留）。
/// ① 池中无条目但 token store 有记录（导入中断产生的孤儿凭证）时
/// 仍执行凭证清理——孤儿真实凭证在 vault 中无任何 UI 清理出口；
/// ② 凭证清理失败上抛 Err 透出（用户对凭证残留有感知）；
/// ③ 账号变更联动网关池热重载（删除的账号即时退出调度，服务未运行时 no-op）。
pub fn qoder_account_remove(state: &AppState, user_id: String) -> Result<(), String> {
    fs_utils::ensure_uid_safe(&user_id)?;
    // 池外孤儿判定：token store 有记录即可清理（池删除接口对孤儿凭证是唯一出口）
    let has_token = qoder_common::load_token_store(state)
        .get("tokens")
        .and_then(|t| t.get(&user_id))
        .is_some();
    // I09：持锁读-改-写
    let removed = with_pool_mut(state, |accounts| {
        let before = accounts.len();
        accounts.retain(|a| a.id != user_id);
        Ok(before != accounts.len())
    })?;
    if !removed && !has_token {
        return Err(format!("账号不在池中: {user_id}"));
    }
    // I14：清理失败上抛（悬空凭证难排查且用户无感知）
    if let Err(e) = qoder_common::remove_token(state, &user_id) {
        fs_utils::app_log(&state.data_dir, &format!("Qoder 账号 {user_id} 凭证清理失败: {e}"));
        let msg = if removed {
            format!("账号已从池中移除，但凭证清理失败：{e}（重新导入同一凭证后再次移除可重试）")
        } else {
            format!("孤儿凭证清理失败：{e}")
        };
        // 池移除已生效：无论凭证清理成败都必须热重载，网关侧即时剔除该账号
        //（否则残留账号凭 vault 旧凭证仍可被调度至下一次生命周期事件/重启）
        crate::api_server::runtime::reload_pools_after_change(state);
        return Err(msg);
    }
    // Q3：主动回收该账号的全局刷新锁条目（仅摘表项无 DB 读；并发持有者的 Arc 由
    // 引用计数自然释放，若与并发刷新竞争，新到的 ensure_fresh 会重建条目，语义不变）
    qoder_common::refresh_lock_remove(&user_id);
    fs_utils::app_log(&state.data_dir, &format!("Qoder 账号已移除: {user_id}"));
    crate::api_server::runtime::reload_pools_after_change(state);
    Ok(())
}

/// PAT 手工导入（M1 最可靠凭证通道；幂等：同 token 稳定同 id，重复导入=更新）。
/// 导入时尝试 /api/v1/userinfo 回填 uid/昵称（失败容错不阻塞——userinfo 对 PAT 的
/// 兼容性 R-6 待验证）。返回导入后的账号视图。
pub fn qoder_account_import_pat(
    state: &AppState,
    name: Option<String>,
    pat: String,
) -> Result<QoderAccountView, String> {
    let pat = pat.trim().to_string();
    if pat.is_empty() {
        return Err("PAT 不能为空".into());
    }
    if !pat.starts_with("pt-") && !pat.starts_with("jt-") {
        return Err("凭证格式不识别：应为 qoder.com.cn/account/integrations 创建的 PAT（pt-）或客户端抓包获取的 job token（jt-）".into());
    }
    let id = account_id_of(&pat);
    // 探测：fetch_userinfo/fetch_plan 为阻塞 ureq 网络请求（15s 超时），
    // cmd_bridge 已在工作线程执行
    let agent = crate::tasks::http_agent(15);
    let creds = QoderCreds {
        access_token: pat.clone(),
        kind: "pat".into(),
        ..Default::default()
    };
    let (uid, nickname) = qoder_common::fetch_userinfo(&agent, &creds);
    // 套餐回填（R-7 抓包固化：GET /api/v2/user/plan → plan_tier_name，如 "Pro Trial"；失败容错）
    let (tier, _user_type, _end) = qoder_common::fetch_plan(&agent, &creds);
    let uid = uid.unwrap_or_default();
    let nickname = nickname.unwrap_or_default();
    let plan = tier.unwrap_or_default();
    // 幂等入池：同 id 保留旧 uid/nickname（userinfo 失败时不覆盖既有信息）；持锁读-改-写（I09）
    let display = name
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());
    // P2 审查修复：入池匹配补 uid 兜底（对齐 oauth import_device_creds / data_io
    // merge_account 的 uid 优先语义）——同账号先经 OAuth 入池（token 派生
    // id 不同）后再导入 PAT 时，uid 命中保留原 id，防同 uid 重复账号
    let id2 = id.clone();
    let effective_id = with_pool_mut(state, |accounts| {
        if let Some(a) = accounts
            .iter_mut()
            .find(|a| a.id == id2 || (!uid.is_empty() && a.uid == uid))
        {
            if let Some(d) = display {
                a.nickname = d;
            } else if a.nickname.is_empty() && !nickname.is_empty() {
                // I13：显式传名仍覆盖；否则仅在池中昵称为空时补 userinfo 值，
                // 防重复导入把用户改过的名覆盖回去（对照 ide_store.rs 守卫）
                a.nickname = nickname.clone();
            }
            if a.uid.is_empty() && !uid.is_empty() {
                a.uid = uid.clone();
            }
            if !plan.is_empty() {
                a.plan = plan.clone();
            }
            // P2 审查修复：credential_source 保守更新——id 命中即同 token、凭证本体
            // 未变，仅来源字段为空时回填；uid 兜底命中且派生 id 不同 = 换了新凭证
            // （OAuth → PAT），徽标随之更新（对齐 oauth.rs 同款判据）
            if a.credential_source.is_empty() || a.id != id2 {
                a.credential_source = "pat".into();
            }
            a.needs_relogin = false;
            a.relogin_reason = String::new();
            // 指纹回填（幂等：已有稳定绑定不覆盖，§5.10）
            if a.device_profile.is_none() {
                a.device_profile = Some(crate::tasks::qoder_device::QoderDeviceProfile::generate());
            }
            Ok(a.id.clone())
        } else {
            accounts.push(QoderAccount {
                id: id2.clone(),
                uid: uid.clone(),
                nickname: display
                    .or_else(|| if nickname.is_empty() { None } else { Some(nickname.clone()) })
                    .unwrap_or_else(|| format!("Qoder {}", &id2[3..9])),
                plan,
                credential_source: "pat".into(),
                // 入池即生成稳定指纹（§5.10：一次生成永不轮换）
                device_profile: Some(crate::tasks::qoder_device::QoderDeviceProfile::generate()),
                ..Default::default()
            });
            Ok(id2.clone())
        }
    })?;
    // 凭证入 token store（M1 单源）。
    // jt- 前缀（审查 L-jt）：同时写入 pat 字段——ensure_fresh 的 PAT 重换通道以
    // has_pat（pat 字段非空）触发，仅落 access_token 时 jt- 走不进重换路径，
    // 24h 过期后直接 refresh_failed 需手工重导
    let creds = QoderCreds {
        pat: pat.clone(),
        access_token: pat,
        kind: "pat".into(),
        uid: uid.clone(),
        nickname: nickname.clone(),
        ..Default::default()
    };
    // 凭证按生效 id 落库（uid 兜底命中时为池内既有 id），与池条目对齐——
    // 若按派生 id 落库则凭证与池条目错位，后续扫描/导入按 id 查不到新凭证
    qoder_common::save_token_store(state, &effective_id, &creds)?;
    // 疑点②：清除旧设备流凭证残留（refresh_token 等）——该账号此前经
    // OAuth 入池时留下的 RT 对 PAT 通道完全无用，且 save_token_store
    // 非空字段合并不清除旧值；失败不阻断导入（仅残留未清），落日志可感知
    if let Err(e) = qoder_common::clear_device_flow_creds(state, &effective_id) {
        fs_utils::app_log(
            &state.data_dir,
            &format!("Qoder 账号 {effective_id} 旧设备流凭证残留清理失败: {e}"),
        );
    }
    fs_utils::app_log(&state.data_dir, &format!("Qoder PAT 账号已导入: {effective_id}"));
    // 新账号/凭证变更联动网关池热重载（fail-open 新账号即时入池调度；服务未运行时 no-op）
    crate::api_server::runtime::reload_pools_after_change(state);
    let accounts = load_pool(state);
    let tokens = qoder_common::load_token_store(state);
    accounts
        .iter()
        .find(|a| a.id == effective_id)
        .map(|a| view_of(a, &tokens))
        .ok_or_else(|| "导入后读取账号失败".into())
}

/// 单账号凭证续期（手动按钮，对照 workbuddy_refresh_token 同语义）：
/// force 恒刷（lazy_hours=i64::MAX 走 ensure_fresh 无条件重换路径，PAT 通道重换
/// 作业令牌、客户端通道 deviceToken/refresh 续期）；每账号互斥由 refresh_lock_for
/// 兜底（并发触发同账号串行排队，后到者命中「他人已刷新落库」二次检查直接复用）。
/// 成功回读视图（到期时间/登录态即时刷新）；失败按 ensure_fresh note 分类中文归因。
pub fn qoder_account_refresh_token(
    state: &AppState,
    account_id: String,
) -> Result<QoderAccountView, String> {
    if load_pool(state).iter().all(|a| a.id != account_id) {
        return Err(format!("账号不在池中: {account_id}"));
    }
    let agent = crate::tasks::http_agent(30);
    let (creds, _did, note) = qoder_common::ensure_fresh(state, &agent, &account_id, i64::MAX);
    match note {
        // 成功路径（本次刷新 / 本已新鲜被复用）→ 回读最新视图
        "refreshed" | "fresh" => {
            // 续期后回写池到期时间/登录态再回读——ensure_fresh 自身不回写池
            // （高频惰性路径的开销考量，回写由签到/余额刷新/手动续期等调用方
            // 负责），缺这步则视图 token_expires_at 停留旧值（PAT 账号恒
            // 「长期有效」），前端到期时间点续期后不刷新
            qoder_common::sync_pool_expiry(state, &account_id, &creds);
            let accounts = load_pool(state);
            let tokens = qoder_common::load_token_store(state);
            accounts
                .iter()
                .find(|a| a.id == account_id)
                .map(|a| view_of(a, &tokens))
                .ok_or_else(|| "续期后回读失败".to_string())
        }
        "no_credential" => Err("该账号无可用凭证，请先导入 PAT 或重新登录".into()),
        // 暂态失败（网络/服务端抖动）：可重试
        "refresh_failed" => Err("续期失败：网络或服务端异常，请稍后重试".into()),
        // 刷新成功但落库失败（refresh_token 一次性轮换，旧 RT 已作废）：
        // 新凭证仅本轮内存有效，重启后须重新导入/续期——如实告知不谎报成功
        "refreshed_unsaved" => {
            Err("续期已执行但落库失败（重启后需重新续期），请检查磁盘空间后重试".into())
        }
        "pat_rejected" => Err("续期失败：PAT 已被拒绝，请更新 PAT 后重试".into()),
        // 永久失败：ensure_fresh 内已回写池 needs_relogin，视图随之展示
        "expired_needs_relogin" | "auth_dead" => {
            Err("凭证已失效且无法自动续期，请重新登录客户端或更新 PAT".into())
        }
        other => Err(format!("续期未执行（{other}）")),
    }
}
