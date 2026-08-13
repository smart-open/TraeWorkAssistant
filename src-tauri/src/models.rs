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
    pub device_id_masked: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RawAccount {
    pub name: String,
    #[serde(rename = "UserID", default)]
    pub user_id: Option<String>,
    pub jwt: String,
    #[serde(default)]
    pub added_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct AccountsFile {
    pub accounts: Vec<RawAccount>,
}

#[derive(Serialize, Deserialize, Clone)]
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
    #[serde(default = "default_true")]
    pub auto_start_proxy: bool,
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
    pub trae_path: Option<String>,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default = "default_retention")]
    pub log_retention_days: i32,
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
fn default_notify() -> String {
    "toast".into()
}
fn default_retention() -> i32 {
    30
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
