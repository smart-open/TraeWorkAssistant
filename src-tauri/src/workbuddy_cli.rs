//! CodeBuddy CLI 切号桥 + 五重防护自动轮换（F-06/F-59，批次3 T3.4）。
//! 设计依据 docs/workbuddy-product-design.md §3.10 / §5.7④（learn-the-design，自写实现）。
//!
//! 分层：
//! - 本模块 = 纯逻辑层（decide_target 纯函数 / settings.json env token JSON 操作 / CLI 会话活动扫描），
//!   不依赖 AppState，全部可单测；
//! - 有状态粘合（账号池 / token store / 轮换状态文件）在 commands/workbuddy.rs。
//!
//! Windows 路线：直接维护 `~/.codebuddy/settings.json` 的 `env.CODEBUDDY_AUTH_TOKEN`
//!（绕过 apiKeyHelper 的 Git Bash/Node 路径兼容坑）；凭证红线：token 不进日志/错误消息。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// CLI settings.json 中的认证环境变量键
pub const AUTH_ENV_KEY: &str = "CODEBUDDY_AUTH_TOKEN";

// ── 候选与决策（纯函数核心）────────────────────────────────────────────────

/// 轮换候选账号（从积分缓存提取，不携带凭证）。
#[derive(Debug, Clone)]
pub struct CliCandidate {
    pub account_id: String,
    pub display_name: String,
    /// 剩余积分中最早到期时间（Unix 毫秒）；无剩余积分资源时为 None。
    pub soonest_expire_at_ms: Option<i64>,
    /// 未过期积分包剩余合计
    pub total_remaining: f64,
    /// 有凭证、查询成功、未过期且有剩余 → 可被选为目标
    pub valid: bool,
    pub error: Option<String>,
}

impl CliCandidate {
    /// 紧迫度排序键：到期越早越紧迫；无到期时间排最后。
    fn urgency_key(&self) -> (i64, i64) {
        match self.soonest_expire_at_ms {
            Some(ts) => (0, ts),
            None => (1, 0),
        }
    }
}

/// 轮换决策结果。
#[derive(Debug, PartialEq)]
pub enum RotateDecision {
    /// 不切，附原因（写入轮换日志）。
    Skip(String),
    /// 切到目标账号 id。
    Switch(String),
}

/// 剥离 "Bearer " 前缀与首尾空白。
pub fn clean_bearer(token: &str) -> &str {
    let token = token.trim();
    if token == "Bearer" {
        return "";
    }
    token.strip_prefix("Bearer ").unwrap_or(token).trim()
}

/// 五重防护轮换决策（§5.7④，纯函数、无 IO，now_ms 由调用方注入保证可测）。
///
/// 防护顺序：
/// 1. 有效候选过滤（查询失败/已过期/无剩余剔除）；
/// 2. 按 urgency_key 升序取最紧迫者为目标；
/// 3. 紧迫阈值 `min_urgency_ms`：目标到期剩余超过该值 → 都还早，不切；
/// 4. 当前即目标 → 不切；
/// 5. 冷却期：上次切换 + cooldown_ms 未到 → 不切；
/// 6. 活跃保护：CLI 最近会话写入距今 < active_guard_ms → 不切（防打断）；
/// 7. 最小剩余 `min_remaining`（0 = 关闭）：目标低于该值不值得切；
/// 8. 到期差异 `min_gap_ms`：目标仅比当前早到期不足该值 → 不切（防横跳）。
#[allow(clippy::too_many_arguments)]
pub fn decide_target(
    candidates: &[CliCandidate],
    current_account_id: Option<&str>,
    now_ms: i64,
    last_switch_at_ms: Option<i64>,
    cooldown_ms: i64,
    min_gap_ms: i64,
    min_urgency_ms: i64,
    cli_recent_activity_ms: Option<i64>,
    active_guard_ms: i64,
    min_remaining: f64,
) -> RotateDecision {
    // 1) 有效候选
    let mut valid: Vec<&CliCandidate> = candidates.iter().filter(|c| c.valid).collect();
    if valid.is_empty() {
        return RotateDecision::Skip("没有可用账号（查询失败/已过期/无剩余积分）".to_string());
    }
    // 2) 目标 = 紧迫度最高（到期最早；无到期排最后）
    valid.sort_by_key(|c| c.urgency_key());
    let target = valid[0];
    // 3) 紧迫度检查：目标到期还早 → 无需切换
    if let Some(target_ts) = target.soonest_expire_at_ms {
        let remaining_ms = target_ts - now_ms;
        if remaining_ms > min_urgency_ms {
            return RotateDecision::Skip(format!(
                "所有账号到期都还早（最紧迫的还剩 {} 天），无需切换",
                remaining_ms / (24 * 3600_000)
            ));
        }
    }
    // 4) 已是目标 → 不切
    if current_account_id == Some(target.account_id.as_str()) {
        return RotateDecision::Skip("当前账号已是最紧迫账号".to_string());
    }
    // 5) 冷却期
    if let Some(ts) = last_switch_at_ms {
        if ts + cooldown_ms > now_ms {
            return RotateDecision::Skip("处于切换冷却期".to_string());
        }
    }
    // 6) 活跃保护：CLI 会话最近有写入 → 不切（正在用）
    if let Some(activity) = cli_recent_activity_ms {
        if activity + active_guard_ms > now_ms {
            return RotateDecision::Skip("CLI 正在使用中，暂不切换".to_string());
        }
    }
    // 7) 价值过滤：目标剩余积分太少，切过去不值得
    if min_remaining > 0.0 && target.total_remaining < min_remaining {
        return RotateDecision::Skip(format!(
            "目标账号剩余积分不足（{}，阈值 {}），不值得切换",
            target.total_remaining, min_remaining
        ));
    }
    // 8) 防横跳：目标比当前早到期，但差异 < 阈值 → 不切
    if let Some(current) = candidates
        .iter()
        .find(|c| Some(c.account_id.as_str()) == current_account_id)
    {
        if let (Some(cur_ts), Some(tgt_ts)) = (current.soonest_expire_at_ms, target.soonest_expire_at_ms) {
            if cur_ts > 0 && tgt_ts < cur_ts && cur_ts - tgt_ts < min_gap_ms {
                return RotateDecision::Skip(format!(
                    "目标到期仅早 {} 小时，未达切换阈值（{} 小时）",
                    (cur_ts - tgt_ts) / 3600_000,
                    min_gap_ms / 3600_000
                ));
            }
        }
    }
    RotateDecision::Switch(target.account_id.clone())
}

// ── settings.json env token 操作（纯 JSON 层，可单测）──────────────────────

/// 从 settings.json JSON 中读取 env 认证 token（已剥离 Bearer、去空白）。
pub fn settings_env_token(value: &Value) -> Option<String> {
    value
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(AUTH_ENV_KEY))
        .and_then(Value::as_str)
        .map(clean_bearer)
        .filter(|t| !t.is_empty())
        .map(|s| s.to_string())
}

/// 在 settings.json JSON 上写入 env 认证 token（保留其余字段；env 非对象时报错）。
pub fn with_env_token(value: &mut Value, token: &str) -> Result<(), String> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| "CodeBuddy settings.json 顶层不是对象".to_string())?;
    let env = obj
        .entry("env")
        .or_insert_with(|| json!({}));
    let env = env
        .as_object_mut()
        .ok_or_else(|| "CodeBuddy settings.json 的 env 字段不是对象".to_string())?;
    env.insert(AUTH_ENV_KEY.to_string(), json!(clean_bearer(token)));
    Ok(())
}

// ── CLI 会话活动扫描（活跃保护数据源）─────────────────────────────────────

/// CLI 最近会话活动时间：递归扫 `~/.codebuddy/projects/**/*.jsonl`（含子目录），
/// 取最新 mtime（Unix 毫秒）；无会话文件返回 None。
pub fn cli_recent_activity() -> Option<i64> {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    if home.is_empty() {
        return None;
    }
    cli_recent_activity_at(&PathBuf::from(home).join(".codebuddy").join("projects"))
}

/// 指定目录版本（测试与扫描复用）。
pub fn cli_recent_activity_at(projects: &Path) -> Option<i64> {
    if !projects.is_dir() {
        return None;
    }
    let mut newest: Option<i64> = None;
    let mut stack = vec![projects.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Ok(meta) = std::fs::metadata(&path) {
                    if let Ok(modified) = meta.modified() {
                        if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                            let ms = dur.as_millis() as i64;
                            newest = Some(newest.map_or(ms, |n| n.max(ms)));
                        }
                    }
                }
            }
        }
    }
    newest
}

// ── 单测（decide_target 五重防护 + settings env 操作）─────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: i64 = 24 * 3600_000; // 防横跳差异阈值
    const URG: i64 = 72 * 3600_000; // 紧迫度阈值
    const GUARD: i64 = 30 * 60_000; // 活跃保护窗口

    fn cand(id: &str, expire_ms: Option<i64>, remaining: f64, valid: bool) -> CliCandidate {
        CliCandidate {
            account_id: id.to_string(),
            display_name: id.to_string(),
            soonest_expire_at_ms: expire_ms,
            total_remaining: remaining,
            valid,
            error: None,
        }
    }

    /// 默认参数调用：无冷却、无活跃会话、无价值过滤（now 注入保证确定性）。
    fn dt(candidates: &[CliCandidate], now: i64, current: Option<&str>) -> RotateDecision {
        decide_target(candidates, current, now, None, 0, GAP, URG, None, GUARD, 0.0)
    }

    #[test]
    fn switches_to_most_urgent_account() {
        let now = 1_000_000_000_000;
        let candidates = vec![
            cand("a", Some(now + 30 * 24 * 3600_000), 100.0, true),
            cand("b", Some(now + 1 * 24 * 3600_000), 50.0, true),
        ];
        assert_eq!(dt(&candidates, now, Some("a")), RotateDecision::Switch("b".into()));
    }

    #[test]
    fn noop_when_current_is_most_urgent() {
        let now = 1_000_000_000_000;
        let candidates = vec![
            cand("a", Some(now + 1 * 24 * 3600_000), 50.0, true),
            cand("b", Some(now + 30 * 24 * 3600_000), 100.0, true),
        ];
        assert_eq!(
            dt(&candidates, now, Some("a")),
            RotateDecision::Skip("当前账号已是最紧迫账号".to_string())
        );
    }

    #[test]
    fn skips_when_gap_below_threshold() {
        // 目标 c 比当前 a 早到期，但差异仅 3h < 24h → 防横跳不切
        let now = 1_000_000_000_000;
        let candidates = vec![
            cand("a", Some(now + 24 * 3600_000), 50.0, true),
            cand("b", Some(now + 22 * 3600_000), 60.0, true),
            cand("c", Some(now + 21 * 3600_000), 70.0, true),
        ];
        assert_eq!(
            dt(&candidates, now, Some("a")),
            RotateDecision::Skip("目标到期仅早 3 小时，未达切换阈值（24 小时）".to_string())
        );
    }

    #[test]
    fn respects_cooldown_window() {
        let now = 1_000_000_000_000;
        let candidates = vec![
            cand("a", Some(now + 30 * 24 * 3600_000), 100.0, true),
            cand("b", Some(now + 1 * 24 * 3600_000), 50.0, true),
        ];
        // 刚切过 10 分钟（冷却 30 分钟）→ 不切
        assert_eq!(
            decide_target(&candidates, Some("a"), now, Some(now - 10 * 60_000), 30 * 60_000, GAP, URG, None, GUARD, 0.0),
            RotateDecision::Skip("处于切换冷却期".to_string())
        );
        // 冷却结束 → 切
        assert_eq!(
            decide_target(&candidates, Some("a"), now, Some(now - 40 * 60_000), 30 * 60_000, GAP, URG, None, GUARD, 0.0),
            RotateDecision::Switch("b".into())
        );
    }

    #[test]
    fn urgency_threshold_blocks_early_switch() {
        // 所有账号 5 天后才过期：最紧迫剩余 5 天 > 72h 阈值 → 不切
        let now = 1_000_000_000_000;
        let candidates = vec![
            cand("a", Some(now + 6 * 24 * 3600_000), 100.0, true),
            cand("b", Some(now + 5 * 24 * 3600_000), 50.0, true),
        ];
        assert_eq!(
            dt(&candidates, now, Some("a")),
            RotateDecision::Skip("所有账号到期都还早（最紧迫的还剩 5 天），无需切换".to_string())
        );
    }

    #[test]
    fn active_guard_blocks_during_cli_usage() {
        let now = 1_000_000_000_000;
        let candidates = vec![
            cand("a", Some(now + 30 * 24 * 3600_000), 100.0, true),
            cand("b", Some(now + 1 * 24 * 3600_000), 50.0, true),
        ];
        // 10 分钟前有会话写入（窗口 30 分钟）→ 不切
        assert_eq!(
            decide_target(&candidates, Some("a"), now, None, 0, GAP, URG, Some(now - 10 * 60_000), GUARD, 0.0),
            RotateDecision::Skip("CLI 正在使用中，暂不切换".to_string())
        );
        // 1 小时前 → 已过窗口，正常切
        assert_eq!(
            decide_target(&candidates, Some("a"), now, None, 0, GAP, URG, Some(now - 60 * 60_000), GUARD, 0.0),
            RotateDecision::Switch("b".into())
        );
        // 无会话文件（None）→ 正常切
        assert_eq!(
            decide_target(&candidates, Some("a"), now, None, 0, GAP, URG, None, GUARD, 0.0),
            RotateDecision::Switch("b".into())
        );
    }

    #[test]
    fn min_remaining_filters_low_value_target() {
        let now = 1_000_000_000_000;
        let candidates = vec![
            cand("a", Some(now + 30 * 24 * 3600_000), 100.0, true),
            cand("b", Some(now + 1 * 24 * 3600_000), 10.0, true),
        ];
        // 目标剩余 10 < 阈值 30 → 不值得切
        assert_eq!(
            decide_target(&candidates, Some("a"), now, None, 0, GAP, URG, None, GUARD, 30.0),
            RotateDecision::Skip("目标账号剩余积分不足（10，阈值 30），不值得切换".to_string())
        );
        // 阈值 5：剩余 10 达标 → 切
        assert_eq!(
            decide_target(&candidates, Some("a"), now, None, 0, GAP, URG, None, GUARD, 5.0),
            RotateDecision::Switch("b".into())
        );
    }

    #[test]
    fn invalid_candidates_excluded_and_none_valid_skips() {
        let now = 1_000_000_000_000;
        // 过期/失败候选不参与
        let candidates = vec![
            cand("a", Some(now + 2 * 24 * 3600_000), 100.0, true),
            cand("expired", Some(now - 3600_000), 0.0, false),
            cand("failed", None, 0.0, false),
        ];
        assert_eq!(dt(&candidates, now, Some("expired")), RotateDecision::Switch("a".into()));
        // 全部无效 → Skip
        let bad = vec![cand("a", Some(now), 0.0, false), cand("b", None, 0.0, false)];
        assert_eq!(
            dt(&bad, now, Some("a")),
            RotateDecision::Skip("没有可用账号（查询失败/已过期/无剩余积分）".to_string())
        );
    }

    #[test]
    fn no_expire_candidates_sort_last() {
        // 无到期时间的账号排最后：即便它剩余更多也不作为目标
        let now = 1_000_000_000_000;
        let candidates = vec![
            cand("a", None, 999.0, true),
            cand("b", Some(now + 2 * 24 * 3600_000), 10.0, true),
        ];
        assert_eq!(dt(&candidates, now, Some("a")), RotateDecision::Switch("b".into()));
    }

    // ── settings env token 操作 ──

    #[test]
    fn env_token_write_preserves_other_fields() {
        let mut settings = json!({
            "enabledPlugins": {"pdf@codebuddy-plugins-official": true},
            "env": {"HTTPS_PROXY": "http://127.0.0.1:7890"}
        });
        with_env_token(&mut settings, "Bearer RAW_TOKEN").unwrap();
        assert_eq!(settings_env_token(&settings).as_deref(), Some("RAW_TOKEN"));
        assert_eq!(settings["enabledPlugins"]["pdf@codebuddy-plugins-official"], json!(true));
        assert_eq!(settings["env"]["HTTPS_PROXY"], "http://127.0.0.1:7890");
    }

    #[test]
    fn env_token_write_creates_env_and_rejects_non_object() {
        // 无 env 字段时自动创建
        let mut settings = json!({"trustedDirectories": []});
        with_env_token(&mut settings, "T1").unwrap();
        assert_eq!(settings_env_token(&settings).as_deref(), Some("T1"));
        // env 非对象时报错且不泄露 token
        let mut bad = json!({"env": "invalid"});
        let err = with_env_token(&mut bad, "SECRET").unwrap_err();
        assert!(err.contains("env 字段不是对象"));
        assert!(!err.contains("SECRET"));
    }

    #[test]
    fn env_token_read_handles_bearer_and_blank() {
        assert_eq!(settings_env_token(&json!({"env": {"CODEBUDDY_AUTH_TOKEN": "Bearer abc"}})).as_deref(), Some("abc"));
        assert_eq!(settings_env_token(&json!({"env": {"CODEBUDDY_AUTH_TOKEN": "  "}})), None);
        assert_eq!(settings_env_token(&json!({"env": {}})), None);
        assert_eq!(settings_env_token(&json!({})), None);
    }

    #[test]
    fn clean_bearer_strips_prefix_only() {
        assert_eq!(clean_bearer("Bearer tok"), "tok");
        assert_eq!(clean_bearer("tok"), "tok");
        assert_eq!(clean_bearer("Bearer"), "");
        assert_eq!(clean_bearer("  Bearer  tok  "), "tok");
    }
}
