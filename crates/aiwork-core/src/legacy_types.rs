//! 桌面域数据类型（store 层引用的类型，其归属命令模块已随桌面壳退役）。
//! 自 commands/trae_apps.rs、commands/doubao.rs 原样平移，仅提升可见性；
//! 字段/serde 契约不变（SQLite 存量数据兼容）。

use std::collections::HashMap;

// ── Trae 账号级套餐（原 commands/trae_apps.rs）─────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct PayStatusEntry {
    /// 套餐名（Free / Lite / Pro ...）
    pub identity_str: String,
    /// 套餐数值
    pub identity: i64,
    /// 是否新客（未付费过）
    pub is_pay_freshman: bool,
    /// 是否积分计费
    pub is_credits_billing: bool,
    /// 查询时间（Unix 秒）
    pub fetched_at: i64,
}

/// pay_status.json 结构：{ statuses: {uid: entry}, updated_at }
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct PayStatusFile {
    #[serde(default)]
    pub statuses: HashMap<String, PayStatusEntry>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

// ── 豆包应用账号池（原 commands/doubao.rs；仅 store 层持久化所需）──────────

/// doubao_accounts 表单条账号记录（P2 元数据 + P3 会话续期字段，均 serde default 向后兼容）
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct DoubaoAccount {
    /// 豆包 user_id（与快照槽目录名一致）
    pub user_id: String,
    /// 别名（展示名，默认 = user_id）
    #[serde(default)]
    pub name: String,
    /// 备注
    #[serde(default)]
    pub note: String,
    /// 入池时间（YYYY-MM-DD HH:MM:SS）
    #[serde(default)]
    pub added_at: String,
    /// 最近一次切换/保存登录态时间
    #[serde(default)]
    pub last_active_at: Option<String>,
    // ── P3 会话续期字段 ──
    /// 明文 sessionid（凭证等同密码：仅存本地文件，前端全程掩码展示）
    #[serde(default)]
    pub session_id: Option<String>,
    /// sid_guard 原文（'sid|create_ts|duration|...'，滑动续期载体）
    #[serde(default)]
    pub sid_guard: Option<String>,
    /// 会话到期时间（由 sid_guard 解析）
    #[serde(default)]
    pub session_expire_at: Option<String>,
    /// 巡检判定：true=过期 / false=有效 / None=未知
    #[serde(default)]
    pub expired: Option<bool>,
    /// 最近一次 cookie 解密同步时间
    #[serde(default)]
    pub cookies_synced_at: Option<String>,
    /// 最近一次续期探活时间
    #[serde(default)]
    pub last_renew_at: Option<String>,
    /// 会话来源：live=当前 User Data / snapshot=快照槽解密
    #[serde(default)]
    pub session_source: Option<String>,
    /// ttwid 设备 Cookie（对话历史 API 登录校验必需；代理抓包或手动录入）
    #[serde(default)]
    pub ttwid: Option<String>,
    // ── P4 会员额度缓存 ──
    /// 会员等级（None = 免费或未识别）
    #[serde(default)]
    pub quota_level: Option<String>,
    /// 会员到期时间
    #[serde(default)]
    pub quota_expire_at: Option<String>,
    /// 额度状态一句话（如 "图片 80/100 · 视频 3/10"）
    #[serde(default)]
    pub quota_summary: Option<String>,
    /// 最近一次额度查询时间（Some = 已查询过，据此展示免费/会员标识）
    #[serde(default)]
    pub quota_checked_at: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct DoubaoAccountPool {
    #[serde(default)]
    pub accounts: Vec<DoubaoAccount>,
    /// 最近一次 KeepAlive 保活时间（池级）
    #[serde(default)]
    pub last_keepalive_at: Option<String>,
}
