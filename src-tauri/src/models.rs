use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Clone, Default)]
pub struct AccountView {
    pub user_id: String,
    pub name: String,
    pub group_id: Option<String>,
    pub jwt: String,
    pub jwt_exp_hours: Option<f64>,
    pub jwt_exp_timestamp: Option<i64>,
    pub checked_today: Option<bool>,
    pub credits: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub device_id_masked: Option<String>,
    pub cooldown_type: Option<String>,
    pub cooldown_until: Option<i64>,
    pub cooldown_reason: Option<String>,
    pub has_refresh_token: bool,
    pub jwt_auto_refresh: bool,
    pub credits_expire_at: Option<i64>,
    /// 通用积分（product_id != 209）剩余
    #[serde(default)]
    pub general_credits: Option<f64>,
    /// Work 积分（product_id == 209）剩余
    #[serde(default)]
    pub work_credits: Option<f64>,
    /// 本周期积分包总额度（有效积分包 credits_limit 合计，到期日历「剩余 X / 总 Y」口径）
    #[serde(default)]
    pub total_credits: Option<f64>,
    /// 套餐身份（Free / Lite / Pro ...，来自 ide_user_pay_status 缓存）
    #[serde(default)]
    pub pay_identity: Option<String>,
    /// 会员套餐到期时间（Unix 秒，来自 ent_usage 会员包）
    #[serde(default)]
    pub membership_expire: Option<i64>,
    /// 会员套餐下次自动续费时间（Unix 秒，无自动续费为 None）
    #[serde(default)]
    pub membership_next_billing: Option<i64>,
    /// refresh_token 过期时间（Unix 秒；上游未下发则为 None）——F-78 批次 3 生命周期管理
    #[serde(default)]
    pub refresh_token_expires_at: Option<i64>,
    /// refresh_token 连续刷新失败次数（刷新成功清零）
    #[serde(default)]
    pub refresh_token_fails: i32,
    /// refresh_token 是否已判定失效（连续 3 次失败或服务端明确拒绝；重新 OAuth 登录重置）
    #[serde(default)]
    pub refresh_token_invalid: bool,
    /// 凭证（JWT/refresh_token）最近一次落盘时间（OAuth 登录/导入/刷新成功时更新；
    /// F-78 批次 3 收尾，对齐 Buddy 侧 auth_saved_at 先例）
    #[serde(default)]
    pub auth_saved_at: Option<String>,
    /// 客户端快照保存时间（sidecar savedAtMs；无快照为 None）
    #[serde(default)]
    pub snapshot_saved_at_ms: Option<i64>,
    /// 最近一次切换到该账号时客户端未确认登录的时间（切换超时/token 失效；
    /// 成功切换或重新保存登录态后清空）
    #[serde(default)]
    pub snapshot_verify_failed_at_ms: Option<i64>,
    /// JWT data.id 与 user_id 归属不一致（issue #55：此时扫描「已入池」可能经该账号
    /// JWT 命中其他 uid，列表却无从对账；显式暴露供列表警示。快照槽位按 user_id
    /// 命名，自动改写会孤儿化既有快照，故仅警示不强修）
    #[serde(default)]
    pub jwt_uid_mismatch: bool,
    /// JWT 解析出的 data.id（mismatch 时供 tooltip 展示）
    #[serde(default)]
    pub jwt_uid: Option<String>,
    /// 积分包明细（仅剩余 > 0 且未过期，按到期时间升序；来自 remaining_credits.json 缓存）：
    /// 到期日历与「7 天内到期」KPI 按包口径展示/计算（对齐 Buddy packages[]）；
    /// 刷新过积分即有值，无包/全用完为空数组，老缓存缺省 → 前端回退账号级汇总口径。
    /// 序列化时空数组同样缺省（前端 `!= null` 判定回退）：无可用包时 credits_expire_at
    /// 必为空（earliest_expire 仅统计剩余 > 0 的包），回退分支不会虚增展示，两条路径等价
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credit_packs: Vec<CreditPackDetail>,
    /// 共享标签 tag id 列表（从 account_tags 表联表填充，不落账号持久化结构，D3）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct RawAccount {
    pub name: String,
    #[serde(rename = "UserID", default)]
    pub user_id: Option<String>,
    pub jwt: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub added_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// 账户中心（icube-dc）id。实测为本机设备/数据中心级标识（跨账号恒定，不具备账号区分度），
    /// 仅记录预留供未来与外部数据源对账合并，不参与去重/合并/展示（用户确认 2026-09-07）
    #[serde(rename = "DcID", default)]
    pub dc_id: Option<String>,
    /// refresh_token 过期时间（Unix 秒；上游未下发则为 None）——F-78 批次 3 生命周期管理，
    /// 对齐 Buddy 侧 refresh_token_expires_at 先例；serde default 保证旧数据文件不破坏
    #[serde(default)]
    pub refresh_token_expires_at: Option<i64>,
    /// refresh_token 连续刷新失败次数（刷新成功清零）
    #[serde(default)]
    pub refresh_token_fails: i32,
    /// refresh_token 是否已判定失效（连续 3 次失败或服务端明确拒绝；重新 OAuth 登录重置）
    #[serde(default)]
    pub refresh_token_invalid: bool,
    /// 凭证最近一次落盘时间（OAuth 登录/导入/刷新成功时更新，仅展示用途）
    #[serde(default)]
    pub auth_saved_at: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct AccountsFile {
    pub accounts: Vec<RawAccount>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub order: i32,
}

#[derive(Serialize, Deserialize, Default)]
pub struct GroupsFile {
    pub groups: Vec<Group>,
    #[serde(default)]
    pub membership: HashMap<String, String>,
}

/// 共享标签定义（跨池一张表，与 Group 同构；id 前缀 tag_）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Tag {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub order: i32,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct DeviceEntry {
    pub device_id: String,
    #[serde(default)]
    pub market_user_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

pub type DeviceMap = HashMap<String, DeviceEntry>;

#[derive(Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default = "default_port")]
    pub proxy_port: u16,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub launch_minimized: bool,
    /// 启动静默签到（T11）：启动 60s 后对未签到账号自动执行一轮签到
    #[serde(default)]
    pub silent_checkin: bool,
    #[serde(default = "default_true")]
    pub auto_start_proxy: bool,
    /// 启动时自动开启 API 网关（7864，监听 0.0.0.0）：默认关，避免升级后自动向局域网暴露服务
    #[serde(default)]
    pub auto_start_api: bool,
    #[serde(default = "default_true")]
    pub tray: bool,
    #[serde(default = "default_lang")]
    pub language: String,
    #[serde(default = "default_true")]
    pub checkin_skip_checked: bool,
    #[serde(default = "default_true")]
    pub checkin_skip_expired: bool,
    #[serde(default = "default_retry")]
    pub retry: i32,
    #[serde(default = "default_notify")]
    pub notify: String,
    #[serde(default)]
    pub trae_cn_path: Option<String>,
    /// 豆包桌面版 exe 手动路径（环境配置页持久化；app_locate doubao 档案读取）
    #[serde(default)]
    pub doubao_path: Option<String>,
    /// 豆包会话续期保活端点（探活巡检用；KeepAlive 不依赖此项；空值由 state 迁移回填默认）
    #[serde(default)]
    pub doubao_renew_url: Option<String>,
    /// 豆包会员额度接口（会员额度汇总 XHR；空 = 额度查询不可用，由 state 迁移回填默认）
    #[serde(default)]
    pub doubao_quota_url: Option<String>,
    /// 豆包快照可选纳入 Default/IndexedDB（C4：对话历史等完整状态随账号迁移；体积代价大，默认排除）
    #[serde(default)]
    pub doubao_snapshot_include_idb: bool,
    /// Trae 每日签到调度触发时刻 HH:MM（默认 09:00，环境配置页可改；
    /// Windows 计划任务注册时间复用该值）
    #[serde(default = "default_trae_checkin_hhmm")]
    pub trae_checkin_hhmm: String,
    /// Trae 签到多账号间隔秒（默认 3s，防上游频控；环境配置页可改，0 = 关闭间隔）。
    /// None = 未配置按默认（Settings::default() 路径同款，state 不做零值回填）
    #[serde(default)]
    pub trae_checkin_gap_secs: Option<u64>,
    /// WorkBuddy 签到/成长多账号间隔秒（默认 3s；任务配置页可改，0 = 关闭；
    /// 签到与成长两轮共用同一配置）
    #[serde(default)]
    pub wb_checkin_gap_secs: Option<u64>,
    /// Qoder 签到多账号间隔秒（默认 3s；任务配置页可改，0 = 关闭）
    #[serde(default)]
    pub qoder_checkin_gap_secs: Option<u64>,
    /// Trae JWT 定时调度续期（issue #27）：调度器 trae-renew 开关（默认开，每日兜底
    /// 续期临期账号；state::settings 对零值统一回填默认，防止 Settings::default 路径漏开）
    #[serde(default = "default_true")]
    pub jwt_renew_enabled: bool,
    /// Trae JWT 续期调度触发时刻 HH:MM（默认 09:00，环境配置页可改）
    #[serde(default = "default_jwt_renew_hhmm")]
    pub jwt_renew_hhmm: String,
    /// WorkBuddy 每日成长任务（任务配置页）：调度器 wb-growth 开关（默认开；
    /// 成长三开关全关时轮次空转无副作用，防 Settings::default 路径漏开同款回填）
    #[serde(default = "default_true")]
    pub wb_growth_enabled: bool,
    /// WorkBuddy 成长任务调度触发时刻 HH:MM（默认 09:00，任务配置页可改）
    #[serde(default = "default_wb_growth_hhmm")]
    pub wb_growth_hhmm: String,
    /// WorkBuddy 每日签到调度触发时刻 HH:MM（默认 09:10，任务配置页可改）
    #[serde(default = "default_wb_checkin_hhmm")]
    pub wb_checkin_hhmm: String,
    // ── Buddy 国际版（buddy-ai）每日活跃打卡（§4.5 补篇，阶段 3b） ──
    /// buddy-ai 打卡调度开关（默认关——国际档显式开启制，与 buddy_ai_enabled 独立；
    /// BuddyAI 页打卡卡可开）
    #[serde(default)]
    pub buddy_ai_checkin_enabled: bool,
    /// buddy-ai 打卡调度触发时刻 HH:MM（默认 09:20，对齐外部项目口径）
    #[serde(default = "default_buddy_ai_checkin_hhmm")]
    pub buddy_ai_checkin_hhmm: String,
    /// buddy-ai 打卡多账号间隔秒（默认 3s，0 = 关闭；复用 effective_checkin_gap）
    #[serde(default)]
    pub buddy_ai_checkin_gap_secs: Option<u64>,
    // ── Buddy 国际版定时同步/续期（2026-10-16 六页对齐批 3，对齐国内任务配置口径） ──
    /// buddy-ai 积分数据每日定时同步开关（默认开——对齐国内看板同步口径；
    /// 仅同步 buddy-ai 池内账号，空池静默跳过）
    #[serde(default = "default_true")]
    pub buddy_ai_credits_sync_enabled: bool,
    /// buddy-ai 积分同步触发时刻 HH:MM（默认 23:45，错峰 wb 23:30 / trae 23:40）
    #[serde(default = "default_buddy_ai_credits_sync_hhmm")]
    pub buddy_ai_credits_sync_hhmm: String,
    /// buddy-ai 积分同步模式：off | hourly（每小时）| every_hours（每N小时）|
    /// every_days（每N天）| daily（每日 HH:MM）。空 = 旧数据（只有 enabled 布尔），
    /// state 归一化按旧开关派生 daily/off
    #[serde(default)]
    pub buddy_ai_credits_sync_mode: String,
    /// buddy-ai 积分同步间隔小时数（every_hours 模式生效，默认 2，1..=24）
    #[serde(default = "default_credits_sync_hours")]
    pub buddy_ai_credits_sync_hours: String,
    /// buddy-ai 积分同步间隔天数（every_days 模式生效，默认 1，1..=30）
    #[serde(default = "default_credits_sync_days")]
    pub buddy_ai_credits_sync_days: String,
    /// buddy-ai 凭证兜底续期开关（默认开；lazy 48h——AT 到期前 48h 内才真正刷新）
    #[serde(default = "default_true")]
    pub buddy_ai_renew_enabled: bool,
    /// buddy-ai 模型目录每日同步开关（默认开；/v3/config 直连拉取，无账号静默跳过）
    #[serde(default = "default_true")]
    pub buddy_ai_catalog_sync_enabled: bool,
    /// buddy-ai 凭证兜底续期触发时刻 HH:MM（默认 10:35，错峰打卡 09:20；环境配置页可改）
    #[serde(default = "default_buddy_ai_renew_hhmm")]
    pub buddy_ai_renew_hhmm: String,
    /// buddy-ai 模型目录同步触发时刻 HH:MM（默认 05:55；资源调度页可改）
    #[serde(default = "default_buddy_ai_catalog_sync_hhmm")]
    pub buddy_ai_catalog_sync_hhmm: String,
    // ── 看板数据定时同步（积分/Token/模型；调度器 credits/models 两类任务） ──
    /// Buddy 积分与 Token 看板同步模式：off（关闭）| hourly（每小时）| daily（每日 HH:MM）| every_hours（每N小时，默认）
    #[serde(default = "default_credits_sync_mode")]
    pub wb_credits_sync_mode: String,
    /// Buddy 积分与 Token 同步触发时刻 HH:MM（daily 模式生效，默认 23:30 对齐原快照时刻）
    #[serde(default = "default_wb_credits_sync_hhmm")]
    pub wb_credits_sync_hhmm: String,
    /// Buddy 积分与 Token 同步间隔小时数（every_hours 模式生效，默认 2，合法范围 1..=24）
    #[serde(default = "default_credits_sync_hours")]
    pub wb_credits_sync_hours: String,
    /// Trae 积分数据同步模式：off | hourly | daily | every_hours（每N小时，默认）
    #[serde(default = "default_trae_credits_sync_mode")]
    pub trae_credits_sync_mode: String,
    /// Trae 积分同步触发时刻 HH:MM（daily 模式生效，默认 23:40 对齐原快照时刻）
    #[serde(default = "default_trae_credits_sync_hhmm")]
    pub trae_credits_sync_hhmm: String,
    /// Trae 积分同步间隔小时数（every_hours 模式生效，默认 2，合法范围 1..=24）
    #[serde(default = "default_credits_sync_hours")]
    pub trae_credits_sync_hours: String,
    /// Buddy 上游模型目录每日同步开关（资源调度页，默认开；无账号时调度静默跳过）
    #[serde(default = "default_true")]
    pub wb_catalog_sync_enabled: bool,
    /// Buddy 模型目录同步触发时刻 HH:MM（默认 05:45）
    #[serde(default = "default_wb_catalog_sync_hhmm")]
    pub wb_catalog_sync_hhmm: String,
    /// Qoder 上游模型目录每日同步开关（资源调度页，默认开；无账号时调度静默跳过）
    #[serde(default = "default_true")]
    pub qoder_catalog_sync_enabled: bool,
    /// Qoder 模型目录同步触发时刻 HH:MM（默认 05:50，错峰 Buddy 05:45）
    #[serde(default = "default_qoder_catalog_sync_hhmm")]
    pub qoder_catalog_sync_hhmm: String,
    /// Trae 官网模型列表每日同步开关（API 服务页，默认开；无账号时调度静默跳过）
    #[serde(default = "default_true")]
    pub trae_models_sync_enabled: bool,
    /// Trae 模型列表同步触发时刻 HH:MM（默认 05:40）
    #[serde(default = "default_trae_models_sync_hhmm")]
    pub trae_models_sync_hhmm: String,
    // ── 通知渠道（F-19；Trae/Buddy 全平台共用，系统设置页通知渠道面板维护） ──
    /// 通知总开关（默认开；关闭后所有事件渠道静默）
    #[serde(default = "default_true")]
    pub notify_enabled: bool,
    /// 签到完成时推送（默认开）
    #[serde(default = "default_true")]
    pub notify_on_checkin: bool,
    /// 调度任务失败时推送（默认开）
    #[serde(default = "default_true")]
    pub notify_on_task_fail: bool,
    /// Bark 推送地址（https://api.day.app/<key>；空 = 关闭该渠道）
    #[serde(default)]
    pub notify_bark_url: Option<String>,
    /// 通用 Webhook 地址（POST JSON，兼容企业微信群机器人等自建端；空 = 关闭）
    #[serde(default)]
    pub notify_webhook_url: Option<String>,
    /// Server酱 SendKey（空 = 关闭）
    #[serde(default)]
    pub notify_serverchan_sendkey: Option<String>,
    /// 会话明细概述显示开关（issue #82 T6.1，默认开；关闭仅隐藏 UI 展示，不删存量缓存）
    #[serde(default = "default_true")]
    pub sessions_preview_enabled: bool,
    /// WorkBuddy 桌面版 exe 手动路径（切换桥 workbuddy 档案 settings_key；环境配置页持久化）
    #[serde(default)]
    pub workbuddy_path: Option<String>,
    /// CodeBuddy 桌面版 exe 手动路径（app_locate codebuddy 档案 settings_key）
    #[serde(default)]
    pub codebuddy_path: Option<String>,
    /// WorkBuddy AI 国际版 exe 手动路径（app_locate workbuddy_ai 档案 settings_key；BuddyAI 环境配置页）
    #[serde(default)]
    pub workbuddy_ai_path: Option<String>,
    /// CodeBuddy AI 国际客户端 exe 手动路径（app_locate codebuddy_ai 档案 settings_key）
    #[serde(default)]
    pub codebuddy_ai_path: Option<String>,
    /// WorkBuddy auth 文件人工路径（默认 %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info；
    /// 非默认安装布局时在环境配置页指定）
    #[serde(default)]
    pub wb_auth_file_path: Option<String>,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default = "default_retention")]
    pub log_retention_days: i32,
    #[serde(default = "default_proxy_domains")]
    pub proxy_domains: String,
    #[serde(default)]
    pub proxy_log_path: Option<String>,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default = "default_api_model")]
    pub api_default_model: String,
    /// F-74：Buddy（WorkBuddy/CodeBuddy）切换账号时自动把当前账号会话迁移到目标账号
    ///（默认关；开启后切换前自动备份当前账号三件套并复制到目标账号名下）
    #[serde(default)]
    pub buddy_switch_migrate_chats: bool,
    // ── Qoder（F-80；M1 签到 MVP 调度键 + 客户端路径）──
    /// Qoder CN IDE 桌面版 exe 手动路径（M3 切换档案预留；默认 %LOCALAPPDATA%\Programs\Qoder CN\Qoder CN.exe）
    #[serde(default)]
    pub qoder_ide_path: Option<String>,
    /// QoderWork（= Qoder CN，经 Launcher 启动；v1.4.1 澄清同一产品）exe 手动路径；留空走默认安装布局
    #[serde(default)]
    pub qoderwork_path: Option<String>,
    /// Qoder 每日签到调度触发时刻 HH:MM（默认 10:15：单次覆盖 0 点签到与 10:00 登录奖励双活动，§2.2）
    #[serde(default = "default_qoder_checkin_hhmm")]
    pub qoder_checkin_hhmm: String,
    /// Qoder 积分数据同步触发时刻 HH:MM（daily 模式生效，默认 23:40）
    #[serde(default = "default_qoder_credits_sync_hhmm")]
    pub qoder_credits_sync_hhmm: String,
    /// Qoder 积分数据同步开关（旧两态字段，保留兼容持久化；调度器改按 mode 判定，
    /// mode 缺失时 state 归一按本布尔派生：true→every_hours、false→off）
    #[serde(default = "default_true")]
    pub qoder_credits_sync_enabled: bool,
    /// Qoder 积分数据同步模式：off（关闭）| hourly（每小时）| daily（每日 HH:MM）|
    /// every_hours（每N小时，默认）——serde default 空串，state 归一按旧 enabled 派生
    #[serde(default)]
    pub qoder_credits_sync_mode: String,
    /// Qoder 积分数据同步间隔小时数（every_hours 模式生效，默认 2，合法范围 1..=24）
    #[serde(default = "default_credits_sync_hours")]
    pub qoder_credits_sync_hours: String,
    /// Qoder 凭证定时续期开关（默认开）：qoder-refresh 任务每 6 小时为全部含
    /// refresh_token 的账号兜底续期 dt- 凭证（客户端 token 惰性窗 7h > 6h 调度
    /// 间隔，任一 tick 必落窗内）；关闭后凭证仅在使用时惰性刷新
    #[serde(default = "default_true")]
    pub qoder_token_renew_enabled: bool,
    /// Qoder 会话流水定时合并开关（默认开）：qoder-sessions-compact 任务每 6 小时将
    /// 早于保留窗的请求级流水按 (date, uid, preview) 合并为单行（幂等，积分合计不变；
    /// 看板展示由查询层读时聚合即时呈现，无需等待合并）
    #[serde(default = "default_true")]
    pub qoder_sessions_compact_enabled: bool,
    /// Qoder 会话流水合并保留窗天数（默认 3，合法 1..=90）：近 N 天保留请求级明细，
    /// 更早的合并为会话行；非法/越界由读取侧回退默认并 clamp
    #[serde(default = "default_qoder_sessions_compact_days")]
    pub qoder_sessions_compact_days: String,
    /// Qoder 会话流水合并触发时刻 HH:MM（保留兼容字段；调度已改 EveryHours(6) 档，
    /// 时刻不参与判定，字段仅保历史持久化可读）
    #[serde(default = "default_qoder_sessions_compact_hhmm")]
    pub qoder_sessions_compact_hhmm: String,
    // ── 侧边栏应用显示（仅 UI 偏好：隐藏不停功能、不删数据、不影响计划任务） ──
    /// 固定应用：始终显示在侧边栏且不可隐藏（radio 单选，默认 trae）
    #[serde(default = "default_pinned_app")]
    pub pinned_app: String,
    /// 侧边栏隐藏的应用 key 列表（空 = 全部显示；固定应用即使列入也强制显示）
    #[serde(default)]
    pub hidden_apps: Vec<String>,
    /// 侧边栏应用自定义图标（应用 key → 图标名，前端 lib/appIcons.ts 候选表；
    /// 缺失/非法回退 APP_TABS 内置默认图标）
    #[serde(default)]
    pub app_icons: HashMap<String, String>,
    /// 侧边栏应用显示顺序（AppKey 列表，按用户拖拽后的顺序存储；空/缺省 = 内置默认
    /// 顺序；未提及的应用由前端按默认顺序补在末尾，新版本新增应用对老配置天然兼容）
    #[serde(default)]
    pub app_order: Vec<String>,
    /// BuddyAI（buddy-ai 国际版）应用 Tab 显示开关（默认 false = 不展示）：
    /// 独立于 hidden_apps——存量用户 hidden_apps 已持久化为 []，若借道隐藏列表
    /// 无法区分「默认隐藏」与「用户显式取消隐藏」，故用独立 opt-in 字段；
    /// 系统设置-应用显示面板勾选后置 true
    #[serde(default)]
    pub buddy_ai_visible: bool,
}

/// 侧边栏应用合法 key 白名单（单一来源）：app_order / app_icons / hidden_apps
/// 的写入侧（settings_set C4 校验）与读取侧（state.rs 归一）共用，杜绝多处
/// 硬编码漂移——新增应用时仅需改此处与前端 APP_TABS
pub fn is_valid_app_key(k: &str) -> bool {
    matches!(k, "trae" | "buddy" | "qoder" | "doubao" | "buddy-ai")
}

fn default_api_port() -> u16 {
    7864
}
fn default_api_model() -> String {
    "deepseek-v4-flash".into()
}
/// 侧边栏固定应用默认 trae（主应用与回退项）
fn default_pinned_app() -> String {
    "trae".into()
}

fn default_port() -> u16 {
    8899
}
fn default_theme() -> String {
    "system".into()
}
fn default_true() -> bool {
    true
}
fn default_lang() -> String {
    "zh-CN".into()
}
fn default_retry() -> i32 {
    1
}
/// Trae 每日签到调度默认每日 09:00（环境配置页可改，schtasks 注册时间复用）
fn default_trae_checkin_hhmm() -> String {
    "09:00".into()
}
/// 签到多账号间隔默认 3s（Trae/Buddy/Qoder 共用默认；防上游频控）
fn default_checkin_gap_secs() -> u64 {
    3
}
/// 签到间隔生效值：未配置按默认 3s，上限 clamp 600s（误填超大值防呆，0 合法 = 关闭）
pub fn effective_checkin_gap(v: Option<u64>) -> u64 {
    v.unwrap_or_else(default_checkin_gap_secs).min(600)
}
/// Trae JWT 续期调度默认每日 09:00（issue #27：剩余 <48h 惰性续期，每日一次为安全超集）
fn default_jwt_renew_hhmm() -> String {
    "09:00".into()
}
fn default_wb_growth_hhmm() -> String {
    "09:00".into()
}
fn default_wb_checkin_hhmm() -> String {
    "09:10".into()
}
/// buddy-ai 打卡调度默认 09:20（§4.5 补篇：对齐外部项目 scheduler 口径）
fn default_buddy_ai_checkin_hhmm() -> String {
    "09:20".into()
}
/// buddy-ai 积分同步默认 23:45（错峰 wb 23:30 / trae 23:40 / qoder 23:40）
fn default_buddy_ai_credits_sync_hhmm() -> String {
    "23:45".into()
}
/// buddy-ai 积分同步 every_days 模式默认间隔天数
fn default_credits_sync_days() -> String {
    "1".into()
}
/// buddy-ai 凭证兜底续期默认 10:35（错峰打卡 09:20 / wb-renew）
fn default_buddy_ai_renew_hhmm() -> String {
    "10:35".into()
}
/// buddy-ai 模型目录同步默认 05:55（错峰 wb-catalog 05:50 / 打卡 09:20）
fn default_buddy_ai_catalog_sync_hhmm() -> String {
    "05:55".into()
}
/// Qoder 每日签到调度默认 10:15（F-80 §2.2：10:15 时 0 点签到与 10:00 登录奖励均可领）
fn default_qoder_checkin_hhmm() -> String {
    "10:15".into()
}
fn default_qoder_credits_sync_hhmm() -> String {
    "23:40".into()
}
/// 看板同步默认每 2 小时（会话明细需随账号定时落盘；issue #82）
fn default_credits_sync_mode() -> String {
    "every_hours".into()
}
fn default_trae_credits_sync_mode() -> String {
    "every_hours".into()
}
/// every_hours 模式默认间隔（小时）
fn default_credits_sync_hours() -> String {
    "2".into()
}
fn default_wb_credits_sync_hhmm() -> String {
    "23:30".into()
}
fn default_trae_credits_sync_hhmm() -> String {
    "23:40".into()
}
fn default_wb_catalog_sync_hhmm() -> String {
    "05:45".to_string()
}

/// Qoder 模型目录同步默认时刻（错峰 Buddy 05:45 / Trae 05:40）
fn default_qoder_catalog_sync_hhmm() -> String {
    "05:50".to_string()
}
/// Qoder 会话流水合并默认时刻（低峰时段，错峰积分同步 23:40 / 凭证刷新 06:00）
fn default_qoder_sessions_compact_hhmm() -> String {
    "04:30".to_string()
}
/// Qoder 会话流水合并保留窗默认天数（与 docs::QODER_SESSIONS_COMPACT_AFTER_DAYS 对齐）
fn default_qoder_sessions_compact_days() -> String {
    "3".to_string()
}
fn default_trae_models_sync_hhmm() -> String {
    "05:40".into()
}
fn default_notify() -> String {
    "toast".into()
}
fn default_retention() -> i32 {
    30
}

/// 池开关「默认开」的 serde 兜底（Trae 主池历史行为恒可用，字段缺省视为开启）
fn default_pool_enabled() -> bool {
    true
}
/// 解密域名白名单默认值（Charles SSL Proxying Locations 语义：列表内 MITM 解密，
/// 其余透明直通）。完整覆盖字节系九组域 + Qoder 全系四域（F-80 M0 抓包支持 +
/// p3-2 chat 抓包：Qoder Work agent worker 的模型端点为 api2-v2.qoder.sh，
/// 管理面为 qoder.com.cn；宽后缀语义下各域覆盖全部子域）；带证书锁定的
/// 客户端域（豆包 ttnet 原生栈）由自适应降级兜底：连续 3 次握手被客户端中止
/// 自动转透明直通（重启代理复位）。
pub fn default_proxy_domains() -> String {
    "trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com,qoder.com.cn,qoder.sh,qoder.cn,qoder.ai".into()
}

/// 旧版默认域名列表（未含 doubao.com）：用于把升级前已持久化的旧默认无缝迁移到新默认
pub fn legacy_proxy_domains() -> String {
    "trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com".into()
}

/// 上一版默认域名列表（含 doubao.com 宽后缀）：该版本会让豆包客户端 ttnet 原生栈
/// 的证书锁定域全部被 MITM 拒绝（页面空白/接口挂），迁移到新默认（用户自定义过则不动）
pub fn legacy_proxy_domains_with_doubao() -> String {
    "trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com".into()
}

/// 中间版默认域名列表（过度收窄版：移出了 zijieapi.com）：zijieapi 实测可正常
/// MITM 且 Trae 抓包需要，迁移回新默认（用户自定义过则不动）
pub fn legacy_proxy_domains_narrow() -> String {
    "trae.cn,trae.com.cn,mchost.guru,www.doubao.com,accounts.doubao.com".into()
}

/// F-80 上一版默认域名列表（未含 qoder.com.cn）：已持久化该旧默认的存量用户
/// 升级后无缝补上 Qoder 域（用户自定义过则不动）
pub fn legacy_proxy_domains_without_qoder() -> String {
    "trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com".into()
}

/// p3-2 上一版默认域名列表（仅含 qoder.com.cn 单 Qoder 域）：Qoder Work agent
/// worker 的 chat 流量走 api2-v2.qoder.sh（/model/v1/chat/completions），MCP 走
/// mcp.qoder.cn——已持久化旧 10 域默认的存量用户升级后无缝补齐 qoder.sh/qoder.cn/
/// qoder.ai 三域（用户自定义过则不动）
pub fn legacy_proxy_domains_with_qoder_cn() -> String {
    "trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com,qoder.com.cn".into()
}

/// F-80 扩展版默认域名列表（qoder.com.cn + qoder.cn + qoder.com 三 Qoder 域）：
/// p3-2b 之前某版本的持久化默认。p3-2b 迁移列表漏了它，存量用户被误判为
/// 「自定义过」导致 qoder.sh/qoder.ai 未进解密白名单（2026-09-30 运行时横幅实测）
pub fn legacy_proxy_domains_qoder_com() -> String {
    "trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com,qoder.com.cn,qoder.cn,qoder.com".into()
}

/// 豆包保活端点默认值：GET /info/v2/（通知未读数，轻量、必须登录，200=有效 / 302=过期）。
/// 实测字节 passport 为 30 天滑动续期（服务端按会话活跃内部刷新，不回发新 cookie），
/// 探活只需携带凭证访问一个"必须登录"的轻量端点即可判定有效性。
pub fn default_doubao_renew_url() -> String {
    "https://www.doubao.com/info/v2/".into()
}

/// 豆包会员额度接口默认值：POST /alice/commerce/sale/subscription/quota/summary/
/// （请求体 {"product_line":"membership"}，200 JSON 会员额度汇总；代理日志实测确认）
pub fn default_doubao_quota_url() -> String {
    "https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/".into()
}

/// 旧版保活端点默认值（doubao.com 首页）：升级时迁移到新默认
pub fn legacy_doubao_renew_url() -> String {
    "https://www.doubao.com/".into()
}

#[derive(Serialize, Deserialize, Default)]
pub struct CreditRecord {
    pub date: String,
    pub user_id: String,
    pub credits: i64,
    #[serde(default)]
    pub delta: i64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct CreditsFile {
    pub records: Vec<CreditRecord>,
}

/// 每日积分快照：记录当天所有账号的积分总数、获得数、消耗数
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct CreditsDailySnapshot {
    pub date: String,
    pub total: f64,
    pub earned: f64,
    pub consumed: f64,
    /// 逐账号 earned 维度（uid → 当日获得积分；issue #82 问题4-1：
    /// 看板「区间总获得」按分组/账号筛选时用此维度求和）。
    /// 空 map = 老快照行无账号明细（前端筛选时该日不计入过滤后合计并如实标注）。
    #[serde(default)]
    pub accounts: std::collections::BTreeMap<String, f64>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct CreditsDailyFile {
    #[serde(default)]
    pub snapshots: Vec<CreditsDailySnapshot>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct CheckinSummary {
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub results: Vec<serde_json::Value>,
    #[serde(default)]
    pub total_ok: i32,
    #[serde(default)]
    pub already: i32,
    #[serde(default)]
    pub failed: i32,
}

/// 剩余积分缓存文件：user_id -> 剩余积分
#[derive(Serialize, Deserialize, Default)]
pub struct RemainingCreditsFile {
    #[serde(default)]
    pub credits: HashMap<String, f64>,
    #[serde(default)]
    pub expire_times: HashMap<String, i64>,
    /// 通用积分（product_id != 209）剩余缓存
    #[serde(default)]
    pub general: HashMap<String, f64>,
    /// 通用积分（product_id != 209）最早到期时间缓存（Unix 秒）：
    /// API 网关调度口径（网关只扣通用积分，Work 包到期不参与，issue #28）；
    /// None 语义 = 账号通用包长期有效/已耗尽，键缺失即无到期约束
    #[serde(default)]
    pub general_expire_times: HashMap<String, i64>,
    /// Work 积分（product_id == 209）剩余缓存
    #[serde(default)]
    pub work: HashMap<String, f64>,
    /// 本周期积分包总额度缓存（credits_limit 合计；到期日历「剩余 X / 总 Y」数据源）
    #[serde(default)]
    pub total_limit: HashMap<String, f64>,
    /// 会员套餐到期时间缓存（Unix 秒，来自 ent_usage 会员包 end_time）
    #[serde(default)]
    pub membership_expire: HashMap<String, i64>,
    /// 会员套餐下次自动续费时间缓存（Unix 秒，next_billing_time）
    #[serde(default)]
    pub membership_next_billing: HashMap<String, i64>,
    /// 积分包明细缓存（仅剩余 > 0 且未过期的包，按到期时间升序）：
    /// 到期日历/7 天内到期 KPI 按包口径计算（对齐 Buddy packages[]），替代账号级汇总
    #[serde(default)]
    pub packs: HashMap<String, Vec<CreditPackDetail>>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// 积分明细条目（仅剩余 > 0 且未过期的积分包）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreditPackDetail {
    /// "通用" | "Work"
    pub kind: String,
    /// 来源名称（如「每日签到」「每月登录积分」）
    pub source: String,
    /// 该包剩余积分 = credits_limit - usage.credits_amount
    pub remaining: f64,
    /// 该包本周期总额度（credits_limit；到期日历「剩余 X / 总 Y」口径，对齐 Buddy packages[].total）
    pub total: f64,
    /// 过期时间（Unix 秒）
    pub expire_time: i64,
}

/// 单账号积分明细（悬浮展示用）
#[derive(Serialize, Clone)]
pub struct CreditDetail {
    pub general: f64,
    pub work: f64,
    pub total: f64,
    /// 按过期时间升序
    pub packs: Vec<CreditPackDetail>,
}

/// 单个账号的冷却状态
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CooldownEntry {
    #[serde(rename = "type", default)]
    pub error_type: String,
    #[serde(default)]
    pub until: i64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub error_count: i32,
}

/// 冷却状态文件：account_cooldowns.json
#[derive(Serialize, Deserialize, Default)]
pub struct AccountCooldownsFile {
    #[serde(default)]
    pub cooldowns: HashMap<String, CooldownEntry>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// API 池配置文件：api_pool.json
#[derive(Serialize, Deserialize)]
pub struct ApiPoolFile {
    /// Trae 池参与调度开关：默认开（主池历史行为恒可用）；关闭后 Trae 目录模型
    /// 不路由 Trae 池，仅 Trae 源模型显式报错（WbDisabled/QoderDisabled 同族语义）
    #[serde(default = "default_pool_enabled")]
    pub trae_enabled: bool,
    #[serde(default)]
    pub enabled_uids: Vec<String>,
    /// 调度策略：expire_first（默认）/ credit_first / random / weighted / p2c
    #[serde(default)]
    pub strategy: String,
    /// Buddy 池调度策略（取值同上）；空 = 沿用 strategy（兼容旧数据两池同策略）
    #[serde(default)]
    pub wb_strategy: String,
    /// Qoder 池调度策略（取值同上）；空 = 沿用 strategy（跟随 Trae 池，同 Buddy 语义）
    #[serde(default)]
    pub qoder_strategy: String,
    /// 参与调度的分组 id 列表；空 = 不限分组
    #[serde(default)]
    pub group_ids: Vec<String>,
    /// Qoder 池入池白名单（qd- 前缀账号 id）；空 = fail-open 全部含凭证账号入池
    #[serde(default)]
    pub qoder_enabled_uids: Vec<String>,
    /// Qoder 池分组筛选（Qoder 账号 group_id，共享分组体系）；空 = 不限分组
    #[serde(default)]
    pub qoder_group_ids: Vec<String>,
    /// 各池标签筛选（共享标签 tag id；空 = 不限）。与分组筛选取交集（D5：
    /// 均空 = 不限；仅一方非空 = 该维度成员集）
    #[serde(default)]
    pub trae_tag_ids: Vec<String>,
    #[serde(default)]
    pub wb_tag_ids: Vec<String>,
    #[serde(default)]
    pub qoder_tag_ids: Vec<String>,
    /// WorkBuddy 上游开关（T2.1）：开启后 WB 目录模型路由到 WB 账号池
    #[serde(default)]
    pub wb_enabled: bool,
    /// 默认深度思考（T5.3/F-62）：客户端未显式请求 reasoning_effort 时默认 high
    #[serde(default)]
    pub wb_default_thinking: bool,
    /// 工具代执行（T5.5/F-64）：客户端声明 web_search 类工具时代理侧代执行
    #[serde(default = "default_true")]
    pub wb_tool_exec: bool,
    /// 后台任务降级（T5.6③/F-65）：标题/摘要类短请求路由到目录最低倍率模型
    #[serde(default)]
    pub wb_bg_downgrade: bool,
    /// 长上下文降档（F-76④）：输入粗估 ≥100k token 的请求自动换 flash 档模型
    #[serde(default)]
    pub wb_longctx_downgrade: bool,
    /// 慢请求竞速对冲阈值毫秒（F-76③）：流式首字节超阈值且有其他健康账号时
    /// 向第二账号发对冲请求；0 = 关闭
    #[serde(default = "default_hedge_threshold_ms")]
    pub wb_hedge_threshold_ms: u64,
    /// Trae 池账号并发上限（F-77，per-pool 三参数之一）：inflight ≥ 上限的账号
    /// 视为 busy 不参与候选，全部 busy 时降级取 inflight 最小者；0 = 不限。
    /// 旧版三池共用字段 account_concurrency_limit 已退役（serde 忽略旧 JSON 键），
    /// Trae 池按默认值 1 落地、与 Buddy/Qoder 池互不共享
    #[serde(default = "default_account_concurrency_limit")]
    pub trae_account_concurrency_limit: u32,
    /// Trae 池粘性 TTL 秒（F-76②，per-pool 三参数之一）：TTL 内同会话必落
    /// 同一池同账号（上游 KV cache 复用）；旧共用字段 pool_sticky_ttl_secs 已退役，
    /// Trae 池按默认值 300 落地
    #[serde(default = "default_pool_sticky_ttl_secs")]
    pub trae_pool_sticky_ttl_secs: u64,
    /// Trae 池显式会话粘性 TTL 秒（per-pool 三参数之一）：显式 conversationId
    /// 绑定账号的有效期（滚动续期），绑定落 sticky_bindings 表 "t:" 命名空间。
    /// 旧版 Trae 池无显式绑定机制，新增对齐 Buddy/Qoder 能力
    #[serde(default = "default_sticky_ttl_secs")]
    pub trae_sticky_ttl_secs: u64,
    /// Trae 池慢请求竞速对冲阈值毫秒（F-76③ 同构）：流式首字节超阈值且有其他
    /// 健康账号时向第二账号发对冲请求，先出首字者胜；0 = 关闭。
    /// 默认 8000 与 Buddy/Qoder 对冲阈值同默认（运行时 clamp 1s–8s）
    #[serde(default = "default_hedge_threshold_ms")]
    pub trae_hedge_threshold_ms: u64,
    /// WB 显式绑定 TTL 秒（F-76②；wb_sticky 会话粘性，per-pool 三参数之一）：
    /// 显式 conversationId 绑定 Buddy 账号的有效期，覆盖原 1800s 常量
    #[serde(default = "default_wb_sticky_ttl_secs")]
    pub wb_sticky_ttl_secs: u64,
    /// Buddy 池账号并发上限（per-pool 三参数之一，默认 1；0 = 不限）
    #[serde(default = "default_account_concurrency_limit")]
    pub wb_account_concurrency_limit: u32,
    /// Buddy 池粘性 TTL 秒（per-pool 三参数之一，默认 300）
    #[serde(default = "default_pool_sticky_ttl_secs")]
    pub wb_pool_sticky_ttl_secs: u64,
    /// Buddy 池入池白名单（wb- 前缀账号 id）：空 = 全部含凭证账号自动入池
    /// （fail-open，对齐 Buddy 页「含凭证账号参与 WB 上游调度」语义；修复 WB 池
    /// 因误用 Trae 共享白名单而恒空、Buddy 源永远 503 no_healthy_account 的问题）；
    /// 非空 = 仅列表内账号参与调度
    #[serde(default)]
    pub wb_enabled_uids: Vec<String>,
    /// Buddy 池分组筛选（wb_group_ids）：非空时仅纳入所选分组的 WB 账号
    /// （未分组账号不参与，对齐 Trae 池 group_ids 的 T10 语义）；空 = 不限分组
    #[serde(default)]
    pub wb_group_ids: Vec<String>,
    /// Qoder 上游开关（p3-3）：开启后 Qoder 目录模型路由到 Qoder 账号池。
    /// 默认 false——未部署 Qoder 的环境不产生空池噪音（健康告警/目录徽章）
    #[serde(default)]
    pub qoder_enabled: bool,
    /// Qoder 慢请求竞速对冲阈值毫秒（F-80-余 v2）：流式首字节超阈值且有其他
    /// 健康账号时向第二账号发对冲请求，先出首字者胜；0 = 关闭。
    /// 默认 8000 与 WB 对冲阈值同默认（运行时 clamp 1s–8s）
    #[serde(default = "default_hedge_threshold_ms")]
    pub qoder_hedge_threshold_ms: u64,
    /// Qoder 会话粘性开关（F-80-余 v2）：开启后同会话（显式 conversationId /
    /// 消息指纹）在 TTL 内绑定同一 Qoder 账号——同账号 + 同种子派生同一上游
    /// session_id，保住上游会话侧复用；默认 false（v1 轮换行为）
    #[serde(default)]
    pub qoder_sticky_enabled: bool,
    /// Qoder 池账号并发上限（per-pool 三参数之一，默认 1；0 = 不限）
    #[serde(default = "default_account_concurrency_limit")]
    pub qoder_account_concurrency_limit: u32,
    /// Qoder 池粘性 TTL 秒（per-pool 三参数之一，默认 300）
    #[serde(default = "default_pool_sticky_ttl_secs")]
    pub qoder_pool_sticky_ttl_secs: u64,
    /// Qoder 池显式会话粘性 TTL 秒（per-pool 三参数之一，默认 1800）：
    /// 覆盖 qoder_sticky store 的 EXPLICIT_TTL_SECS 常量（旧版硬编码不可配）
    #[serde(default = "default_sticky_ttl_secs")]
    pub qoder_sticky_ttl_secs: u64,
    /// Buddy 国际版（buddy-ai）池上游开关：开启后 buddy-ai 目录模型路由到国际池。
    /// 默认 false——dispatch「buddy-ai」档显式开启制（§4.2：避免模型路由歧义，
    /// 用户在 API 服务页显式开启国际档），未部署国际账号时不产生空池噪音
    #[serde(default)]
    pub buddy_ai_enabled: bool,
    /// buddy-ai 显式会话粘性开关（§4.2 同 qoder_sticky_enabled 口径，默认关）
    #[serde(default)]
    pub buddy_ai_sticky_enabled: bool,
    /// buddy-ai 池粘性 TTL 秒（per-pool 参数，默认 300）
    #[serde(default = "default_pool_sticky_ttl_secs")]
    pub buddy_ai_pool_sticky_ttl_secs: u64,
    /// buddy-ai 池账号并发上限（per-pool 三参数之一，默认 1；0 = 不限，
    /// 对齐 Qoder 池 qoder_account_concurrency_limit 口径）
    #[serde(default = "default_account_concurrency_limit")]
    pub buddy_ai_account_concurrency_limit: u32,
    /// buddy-ai 显式会话粘性 TTL 秒（per-pool 三参数之一，默认 1800）：
    /// 覆盖 buddy_ai_sticky store 的 EXPLICIT_TTL_SECS 常量（set_explicit_ttl 热应用）
    #[serde(default = "default_wb_sticky_ttl_secs")]
    pub buddy_ai_sticky_ttl_secs: u64,
    /// buddy-ai 竞速对冲阈值毫秒（per-pool 三参数之一，0 = 关闭；默认 8000，
    /// 运行时 clamp 1s–8s 同 WB/Qoder）
    #[serde(default = "default_hedge_threshold_ms")]
    pub buddy_ai_hedge_threshold_ms: u64,
    /// buddy-ai 池分组筛选（buddy_ai_group_ids）：非空时仅纳入所选分组的国际账号
    ///（未分组账号不参与，对齐 WB/Qoder 池 T10 语义）；空 = 不限分组
    #[serde(default)]
    pub buddy_ai_group_ids: Vec<String>,
    /// buddy-ai 池标签筛选（tag_ 前缀 id）：与分组筛选取交集（D5 池语义）；空 = 不限
    #[serde(default)]
    pub buddy_ai_tag_ids: Vec<String>,
    /// buddy-ai 池内调度策略（取值同 strategy；空串 = 跟随 Trae 池，与
    /// wb_strategy / qoder_strategy 同语义）。启动（do_start）与热应用（pool_set）
    /// 共用 PoolStrategy::resolve_buddy_ai 解析，保证两处行为一致
    #[serde(default)]
    pub buddy_ai_strategy: String,
    /// Qwen3.8-Flash 免费活动倍率覆盖（资源调度页模型目录开关，默认开）：
    /// 开启时该模型目录/兜底倍率按 0 处理——/v1/models 展示 0× + 取号豁免
    /// 零积分过滤（issue #111：上游免费活动期 billable=false 不扣分，但
    /// model/list 目录价随低峰折扣漂移 0.1/0.08，不反映免费，需此覆盖）。
    /// 上游活动结束后关闭即恢复官方同步倍率
    #[serde(default = "default_true")]
    pub qoder_flash_free_rate: bool,
}

fn default_hedge_threshold_ms() -> u64 {
    // 与运行时 clamp(1000, 8000) 上限对齐（P1 缺陷5）：原 15s 默认超出上限，
    // 实际等效 8s，配置展示失真
    8_000
}

fn default_account_concurrency_limit() -> u32 {
    1
}

fn default_pool_sticky_ttl_secs() -> u64 {
    300
}

fn default_wb_sticky_ttl_secs() -> u64 {
    1800
}

/// 显式会话粘性 TTL 默认值（Trae/Qoder 池 per-pool 三参数之一，与
/// wb_sticky::EXPLICIT_TTL_SECS 30m 常量对齐）
fn default_sticky_ttl_secs() -> u64 {
    1800
}

/// Default 与 serde default 对齐（derive Default 的数值字段会落 0，与旧版
/// api_pool.json 缺字段的语义不一致：F-76③ 对冲默认 15s、F-77 并发默认 1、
/// F-76② 池粘性 300s / 会话粘性 1800s）
impl Default for ApiPoolFile {
    fn default() -> Self {
        Self {
            trae_enabled: true,
            enabled_uids: Vec::new(),
            strategy: String::new(),
            wb_strategy: String::new(),
            group_ids: Vec::new(),
            wb_enabled: false,
            wb_default_thinking: false,
            wb_tool_exec: false,
            wb_bg_downgrade: false,
            wb_longctx_downgrade: false,
            wb_hedge_threshold_ms: default_hedge_threshold_ms(),
            trae_account_concurrency_limit: default_account_concurrency_limit(),
            trae_pool_sticky_ttl_secs: default_pool_sticky_ttl_secs(),
            trae_sticky_ttl_secs: default_sticky_ttl_secs(),
            trae_hedge_threshold_ms: default_hedge_threshold_ms(),
            wb_sticky_ttl_secs: default_wb_sticky_ttl_secs(),
            wb_account_concurrency_limit: default_account_concurrency_limit(),
            wb_pool_sticky_ttl_secs: default_pool_sticky_ttl_secs(),
            wb_enabled_uids: Vec::new(),
            wb_group_ids: Vec::new(),
            qoder_enabled: false,
            qoder_hedge_threshold_ms: default_hedge_threshold_ms(),
            qoder_sticky_enabled: false,
            qoder_account_concurrency_limit: default_account_concurrency_limit(),
            qoder_pool_sticky_ttl_secs: default_pool_sticky_ttl_secs(),
            qoder_sticky_ttl_secs: default_sticky_ttl_secs(),
            qoder_flash_free_rate: true,
            qoder_strategy: String::new(),
            qoder_enabled_uids: Vec::new(),
            qoder_group_ids: Vec::new(),
            trae_tag_ids: Vec::new(),
            wb_tag_ids: Vec::new(),
            qoder_tag_ids: Vec::new(),
            buddy_ai_enabled: false,
            buddy_ai_sticky_enabled: false,
            buddy_ai_pool_sticky_ttl_secs: default_pool_sticky_ttl_secs(),
            buddy_ai_account_concurrency_limit: default_account_concurrency_limit(),
            buddy_ai_sticky_ttl_secs: default_wb_sticky_ttl_secs(),
            buddy_ai_hedge_threshold_ms: default_hedge_threshold_ms(),
            buddy_ai_group_ids: Vec::new(),
            buddy_ai_tag_ids: Vec::new(),
            buddy_ai_strategy: String::new(),
        }
    }
}

/// 池中单个账号的运行时状态（给 /status 和前端使用）
#[derive(Serialize, Clone)]
pub struct PoolStatus {
    pub uid: String,
    pub name: String,
    pub credits: Option<f64>,
    pub credits_expire_at: Option<i64>,
    pub cooling: bool,
    pub cooldown_until: Option<i64>,
    pub cooldown_reason: Option<String>,
    /// hard_credit（积分耗尽）冷却中：until 次日 04:00 自动恢复探测（F-29 v1.2）。
    /// cooling 仅覆盖 until 软冷却，此字段补齐硬冷却——否则积分耗尽账号在
    /// 前端显示「就绪」却不可选（可观测盲点）
    #[serde(default)]
    pub hard_credit: bool,
    pub disabled: bool,
    pub err_count: i32,
    /// 账号五态机（T2.2/F-29 v1.2）：Available/QuotaProtection/RateLimited/Forbidden/ProxyDisabled
    #[serde(default)]
    pub state: String,
    /// 账号实时在途并发数（F-77：per-account inflight，前端池状态页展示）
    #[serde(default)]
    pub inflight: u32,
    /// refresh_token 是否已判定失效（F-78 批次 3：前端展示「Token 失效」徽标）
    #[serde(default)]
    pub refresh_invalid: bool,
}

/// API 服务整体状态（给前端用）
#[derive(Serialize, Clone)]
pub struct ApiServiceStatus {
    pub running: bool,
    pub port: u16,
    pub total_requests: u64,
    pub active_uid: Option<String>,
    pub last_error: Option<String>,
    pub started_at: Option<u64>,
}
