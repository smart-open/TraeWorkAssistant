//! 后台任务域（自 src-tauri/src/tasks 平移，T5/T8）：
//! 签到/积分任务实现 + 统一 ureq agent（直连语义：不读系统/环境代理）。
//! 豆包任务（doubao_*）与 UI 点击兜底（ui_click）随桌面壳退役，不平移。

pub mod buddy_ai_checkin;
pub mod qoder_catalog;
pub mod qoder_checkin;
pub mod qoder_common;
pub mod qoder_credits;
pub mod qoder_device;
pub mod qoder_oauth;
pub mod qoder_refresh;
pub mod qoder_sign;
pub mod qoder_upstream;
pub mod trae_checkin;
pub mod wb_checkin;
pub mod wb_common;
pub mod wb_credits;

use std::net::ToSocketAddrs;

use crate::state::AppState;

/// glibc DNS 解析失败短语（Linux 容器，docker 运行层 debian:bookworm-slim 下
/// getaddrinfo 失败文案，无稳定 os error 码）：is_transient_net_err / friendly_net_err
/// 共用，统一小写比较
const GLIBC_DNS_PHRASES: [&str; 4] = [
    "name or service not known",
    "temporary failure in name resolution",
    "no address associated with hostname",
    "failed to lookup address information",
];

/// 瞬时网络错误判定（issue #209）：仅连接期故障值得重试——连接未建立、请求未
/// 到达服务器，POST 重试无副作用；HTTP 4xx/5xx（Error::Status）与 ureq 自身
/// 超时（文案不含 os error 码）不在此列，避免重复提交。
/// 双平台覆盖（docker 适配）：Windows 桌面 WinSock 码（11001/11002=DNS 解析失败、
/// 10060=连接超时、10061=拒连、10064=主机不可达）+ Linux 容器 errno 文本
///（111=拒连、110=超时、113=不可达、101=网络断）与 glibc DNS 短语——原实现仅
/// 匹配 Windows 码，debian 容器下全部失效。统一小写比较；「os error 110」是
/// 「os error 11001」的前缀子串，但两者同属瞬时故障判定无害（friendly_net_err
/// 的 DNS 分支先行，11001 仍归 DNS 文案）。
pub fn is_transient_net_err(msg: &str) -> bool {
    // 统一小写比较（glibc 短语实际文案首字母可能大写，如 "Name or service not
    // known"；os error 码文本本身全小写，小写化无副作用）
    let lower = msg.to_lowercase();
    let win = ["11001", "11002", "10060", "10061", "10064"]
        .iter()
        .any(|c| lower.contains(&format!("os error {c}")));
    let linux = ["os error 111", "os error 110", "os error 113", "os error 101"]
        .iter()
        .any(|c| lower.contains(c))
        || GLIBC_DNS_PHRASES.iter().any(|p| lower.contains(p));
    win || linux
}

/// 网络错误文案归一化（issue #209）：DNS 失败多为本机安全软件（如火绒）网络
/// 防护或代理/VPN（TUN/fake-ip）拦截应用进程解析所致，附可操作提示后流入
/// 用户可见错误（事件/摘要/通知）。「No ip address」为 ureq 对解析返回空集的
/// 报错（无 os error 后缀），同属 DNS 类故障一并归入；Linux 容器侧补 glibc
/// DNS 短语与拒连/超时 errno 文案的归一化分支（双平台覆盖，同 is_transient_net_err）。
pub fn friendly_net_err(msg: &str) -> String {
    let lower = msg.to_lowercase();
    let dns_like = ["11001", "11002"]
        .iter()
        .any(|c| msg.contains(&format!("os error {c}")))
        || msg.contains("No ip address")
        || GLIBC_DNS_PHRASES.iter().any(|p| lower.contains(p));
    if dns_like {
        format!("{msg}（本地 DNS 解析失败：常由安全软件网络防护或代理/VPN 拦截引起，请检查后重试）")
    } else if is_transient_net_err(msg) {
        format!("{msg}（连接不可达：请检查网络/代理/VPN 后重试）")
    } else {
        msg.to_string()
    }
}

/// DNS 解析（带瞬时失败重试，issue #209）：行为等价 StdResolver（to_socket_addrs），
/// getaddrinfo 被安全软件/代理链瞬时拦截（os error 11001/11002）时自动重试 2 次
///（200ms/1s 退避），避免单次解析失败让整轮签到报错。
fn resolve_with_retry(netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    const MAX_EXTRA: usize = 2;
    let mut attempt = 0;
    loop {
        match netloc.to_socket_addrs().map(|it| it.collect()) {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_EXTRA && is_transient_net_err(&e.to_string()) => {
                std::thread::sleep(std::time::Duration::from_millis(if attempt == 0 {
                    200
                } else {
                    1000
                }));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 构建 ureq agent（统一出口）。
/// 直连语义：ureq 默认不读系统/环境代理（对齐 python OPENER 绕代理约定），
/// 单请求可再用 `.timeout()` 覆盖。
pub fn http_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .resolver(resolve_with_retry)
        .build()
}

/// 代理 URL 日志脱敏：仅保留 scheme://host[:port]，剥离 userinfo——
/// proxy_url 常为 http://user:pass@host 形态，原始回显会把凭证泄进日志。
fn sanitize_proxy_url_for_log(s: &str) -> String {
    let trimmed = s.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return "<无 scheme>".into();
    };
    // userinfo 可含 @，host 不含——取最后一个 @ 之后为 host 段
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    format!("{scheme}://{host}")
}

/// 单账号独立出口代理（P4 防风控）：proxy_url 配置成功则按同参数构建带代理的
/// agent；URL 非法/为空回退直连（与 http_agent 同配置）。每账号独立 agent
/// 避免连接池复用把不同出口的 TCP/会话指纹串到一起。
pub fn http_agent_with_proxy(timeout_secs: u64, proxy_url: Option<&str>) -> ureq::Agent {
    let proxy = proxy_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| match ureq::Proxy::new(s) {
            Ok(p) => Some(p),
            // 纯函数无 data_dir/app logger 上下文，沿用 eprintln! 先例。
            // 审查修复（第五轮）：不回显原始 URL——proxy_url 常为
            // http://user:pass@host 形态，原样输出会把内嵌凭证泄进
            // stderr/docker logs；只留 scheme/host 前缀定位问题
            Err(e) => {
                eprintln!("单账号代理 URL 解析失败（已回退直连）: {} — {e}", sanitize_proxy_url_for_log(s));
                None
            }
        });
    match proxy {
        Some(p) => ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .resolver(resolve_with_retry)
            .proxy(p)
            .build(),
        None => http_agent(timeout_secs),
    }
}

/// 解析 `--task-run <name>` CLI 参数（`--task-run` 为首个参数时进入任务模式）。
pub fn parse_task_mode(args: &[String]) -> Option<String> {
    if args.len() >= 2 && args[1] == "--task-run" {
        return args.get(2).cloned().filter(|s| !s.is_empty());
    }
    None
}

/// CLI 任务分发（aiwork-server `--task-run` 兜底入口）。
/// 成功输出任务结果 JSON，失败输出 {"ok":false,"error":...}；返回进程退出码。
pub fn run_cli_task(name: &str, state: &AppState) -> i32 {
    let result = match name {
        // Trae 每日签到：vault 解密全量账号跑单轮（状态核验幂等）
        "checkin" => {
            let accounts = crate::vault::load_accounts(state);
            let retry = state.settings().retry.max(0) as u32;
            Ok(trae_checkin::run_round(state, &accounts.accounts, retry, &mut |ev| {
                println!("{}", serde_json::to_string(ev).unwrap_or_default());
            }))
        }
        // WorkBuddy 每日签到（--json-stream --skip-checked 同款参数）
        "wb-checkin" => Ok(wb_checkin::run_checkin_round(
            state,
            &wb_checkin::CheckinOpts::daily(),
            &mut |ev| println!("{}", serde_json::to_string(ev).unwrap_or_default()),
        )),
        // WorkBuddy token 兜底续期（lazy 24h）
        "wb-renew" => Ok(wb_checkin::run_renew_only(state, 24)),
        // Qoder 每日签到（F-80；CLI 直调 + 应用内调度器共用；10:15 单次覆盖双活动）
        "qoder-checkin" => {
            let opts = qoder_checkin::QoderCheckinOpts::daily();
            let done = qoder_checkin::run_checkin_round(state, &opts, &mut |ev| {
                if ev.get("type").and_then(serde_json::Value::as_str) != Some("done") {
                    println!("{}", serde_json::to_string(ev).unwrap_or_default());
                }
            });
            // 审查 M-1：存在失败账号时返 Err（CLI 退出码可见 + 通知渠道）。
            // 口径对齐调度器（scheduler.rs 同款）：empty_campaigns（无签到任务）
            // 2026-11-21 起归类 skip、不计失败，任务签到直接跳过；CLI 进程本身
            // 无 30min 冷却重试链，重试由次日调度覆盖
            let failed = done.get("failed").and_then(serde_json::Value::as_i64).unwrap_or(0);
            // 永久性认证失败与调度器口径对齐（scheduler.rs 同款）：重试注定失败，
            // 不计入 Err（否则失效账号让 CLI 每日执行结果恒为失败）
            let failed_permanent = done
                .get("failed_permanent")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            // BLOCKED 风控信任单列（issue #190）与调度器口径对齐：重试注定失败，
            // 不计入 Err（审查修复）
            let failed_blocked = done
                .get("failed_blocked")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            if failed - failed_permanent - failed_blocked > 0 {
                let ok = done.get("ok").and_then(serde_json::Value::as_i64).unwrap_or(0);
                let already = done.get("already").and_then(serde_json::Value::as_i64).unwrap_or(0);
                Err(format!(
                    "Qoder 签到：{ok} 成功 / {already} 已领 / {failed} 失败（稍后自动重试）"
                ))
            } else {
                Ok(done)
            }
        }
        // Qoder 积分快照（调度器/CLI 共用；空池自然空转）。CLI 独立任务模式无
        // 网关运行态（gateway_shared() 未注册），函数内部仅落 doc 快照不拨新内存池
        "qoder-credits-snapshot" => qoder_credits::run_snapshot_task(state),
        // Qoder 凭证 6h 兜底刷新（调度器/CLI 共用；空池空转）
        "qoder-refresh" => qoder_refresh::run_task(state),
        // Qoder 模型目录每日同步（调度器/CLI 共用；空池/无凭证空转，p3-3 收尾）
        "qoder-catalog-sync" => qoder_catalog::run_task(state),
        // Qoder 会话流水定时合并（移植 main；保留窗 3 天，幂等可重入）
        "qoder-sessions-compact" => crate::store::docs::qoder_sessions_compact(
            &crate::store::db(&state.data_dir),
            crate::store::docs::QODER_SESSIONS_COMPACT_AFTER_DAYS,
        )
        .map(|(merged, deleted)| {
            serde_json::json!({
                "ok": true,
                "merged": merged,
                "deleted": deleted,
                "days": crate::store::docs::QODER_SESSIONS_COMPACT_AFTER_DAYS
            })
        }),
        // 刷新全部账号剩余积分（credits_daily 快照按日重算）
        "refresh-credits" => crate::commands::accounts::refresh_remaining_credits_impl(state)
            .map(|n| serde_json::json!({ "refreshed": n, "snapshot": "credits_daily.json" })),
        // Buddy 国际版积分数据同步 / 凭证兜底续期 / 模型目录同步（调度器/CLI 共用；
        // CLI 独立任务模式无网关运行态（gateway_shared() 未注册），仅落库不做运行池拨新）
        "buddy-ai-credits-sync" => crate::commands::workbuddy::buddy_ai_credits_sync_impl(state)
            .map(|v| serde_json::json!({ "ok": true, "accounts": v.len() })),
        "buddy-ai-renew" => crate::commands::workbuddy::buddy_ai_renew_task(state),
        "buddy-ai-catalog-sync" => crate::commands::workbuddy::buddy_ai_catalog_sync_impl(state)
            .map(|n| serde_json::json!({ "ok": true, "models": n })),
        // Buddy 国际版每日活跃打卡（T4 审查修复：调度器有 buddy-ai-checkin 分支，
        // run_cli_task 却无对应分发——`--task-run buddy-ai-checkin` 落入未知任务
        // 报错退出）。口径对齐 scheduler.rs 同名分支：抢轮次锁与 UI 路径互斥
        //（skipped_busy 幂等跳过）；存在失败账号返 Err（退出码可见，NDJSON 事件
        // 已逐行打印失败明细）。不设 permanent 剔除：buddy-ai 凭证 401 可经续期/
        // 401 自愈轮换新值重试成功（T7 归一语义），CLI 无冷却重试链、次日调度覆盖
        "buddy-ai-checkin" => {
            match crate::commands::workbuddy::try_acquire_buddy_ai_round() {
                Ok(_round) => {
                    let done = buddy_ai_checkin::run_checkin_round(state, &mut |ev| {
                        println!("{}", serde_json::to_string(ev).unwrap_or_default());
                    });
                    let ok = done.get("ok").and_then(serde_json::Value::as_i64).unwrap_or(0);
                    let already = done
                        .get("already")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let failed = done
                        .get("failed")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    if failed > 0 {
                        Err(format!(
                            "buddy-ai 打卡：{ok} 成功 / {already} 已完成 / {failed} 失败"
                        ))
                    } else {
                        Ok(done)
                    }
                }
                Err(_) => Ok(serde_json::json!({
                    "ok": true,
                    "skipped": "已有 buddy-ai 打卡任务在执行中，本轮跳过",
                    "skipped_busy": true
                })),
            }
        }
        // Trae 消耗明细同步（issue #61；调度器/CLI 共用；无账号跳过，全部账号拉取失败返 Err）
        "trae-usage-sync" => {
            let accounts = crate::vault::load_accounts(state);
            if accounts.accounts.is_empty() {
                Ok(serde_json::json!({ "ok": true, "skipped": "无 Trae 账号" }))
            } else {
                crate::commands::usage_history::usage_history_fetch_impl(state, true)
                    .map(|r| serde_json::json!({ "ok": true, "accounts": r.accounts.len() }))
            }
        }
        other => Err(format!("未知任务: {other}")),
    };
    match result {
        Ok(v) => { println!("{}", serde_json::to_string(&v).unwrap_or_default()); 0 }
        Err(e) => { println!("{}", serde_json::json!({"ok": false, "error": e})); 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_proxy_url_for_log;

    #[test]
    fn 代理url脱敏_剥离userinfo保留host端口() {
        assert_eq!(sanitize_proxy_url_for_log("http://user:pass@1.2.3.4:7890"), "http://1.2.3.4:7890");
        assert_eq!(sanitize_proxy_url_for_log("socks5://a:b@c.com"), "socks5://c.com");
        // userinfo 可含 @：取最后一个 @ 之后（host 不含 @）
        assert_eq!(sanitize_proxy_url_for_log("http://u@p:w@host:8080"), "http://host:8080");
        // 无 userinfo 原样保留（trim 后）
        assert_eq!(sanitize_proxy_url_for_log(" http://host:8080 "), "http://host:8080");
        // 无 scheme：不猜结构，整段不回显
        assert_eq!(sanitize_proxy_url_for_log("user:pass@host"), "<无 scheme>");
    }
}
