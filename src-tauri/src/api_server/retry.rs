//! 分级重试策略表（T2.2/F-33 v1.2，吸收 antigravity-tools 设计）
//!
//! 错误三态：
//! - `RETRY_SAME`（同 Key 重试：502/503/超时/限频等待窗口）
//! - `SWITCH_KEY`（401/403/429 重试耗尽后 → 刷新换号）
//! - `FATAL`（400 context_too_long 等 → 终止，换号无意义）
//!
//! 本模块为纯函数（无 IO），便于单测；调用方负责 sleep 与换号。

/// 重试决策
#[derive(Debug, Clone, PartialEq)]
pub enum RetryAction {
    /// 同账号重试，等待 delay_ms 后再试
    RetrySame { delay_ms: u64 },
    /// 换号（含刷新凭证重试一次）
    SwitchKey,
    /// 终止：把错误透传给客户端
    Fatal,
}

/// 判定错误体是否为上游偶发签名校验抖动（§3.9 ③ 重试表第 3 行）
pub fn is_thinking_signature_error(body: &str) -> bool {
    body.contains("thinking.signature")
}

/// 11128 渠道风控识别（issue #57）：上游判定「非授权渠道调用」全量拦截。
/// 以报文关键词匹配（不裸匹配数字 11128，避免与 token 里的数字误撞）。
/// 该拦截按请求指纹判定、与账号/模型无关，正确处置是「强制指纹清洗后
/// 同号重试一次」（retry_plan 不感知清洗状态，升级逻辑在调用方）
pub fn is_illegal_channel_error(body: &str) -> bool {
    body.contains("Illegal API invocation")
        || body.contains("unapproved channel")
        || contains_code_value(body, "11128")
}

/// 11140 账号级风控识别（issue #136 反馈）：上游判定「账号存在异常行为，
/// 已被限制使用」，账号整体被限制、需申诉恢复。与 11128 渠道级（按请求
/// 指纹、与账号无关、换号无意义）相反——11140 换号有意义，受限账号由
/// classify → Forbidden 禁用（内存态，无手动启用入口——申诉处理后重启
/// 应用或重新登录账号即随池快照重建恢复调度）。
/// 中文关键词 + code 数字边界匹配（报文可能只带关键词或只带 code）
pub fn is_account_restricted_error(body: &str) -> bool {
    body.contains("账号存在异常")
        || body.contains("已被限制使用")
        || contains_code_value(body, "11140")
}

/// `"code": <code>` 宽松匹配：容忍键/冒号/值之间的序列化空白差异
/// （Go encoding/json 紧凑无空格，但报文可能经中间层重序列化）；
/// 值须带数字边界（`111280`/`111289` 不误撞），键名引号边界天然排除
/// `some_code` 等字段名
fn contains_code_value(body: &str, code: &str) -> bool {
    const KEY: &str = "\"code\"";
    let mut rest = body;
    while let Some(pos) = rest.find(KEY) {
        let after_key = rest[pos + KEY.len()..].trim_start();
        if let Some(after_colon) = after_key.strip_prefix(':') {
            let after_colon = after_colon.trim_start();
            if let Some(tail) = after_colon.strip_prefix(code) {
                if !tail.starts_with(|c: char| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
        rest = &rest[pos + KEY.len()..];
    }
    false
}

/// 分级重试决策（按 §3.9 ③ 表格逐行实现）
///
/// - `attempt`：同一账号已尝试次数（0 = 第一次失败后决策）
/// - `retry_after`：上游 429 响应的 Retry-After 头（秒）
pub fn retry_plan(status: u16, body: &str, attempt: u32, retry_after: Option<u64>) -> RetryAction {
    // 400 + thinking.signature：固定 200ms 后重试一次（仅一次）
    if status == 400 && is_thinking_signature_error(body) {
        return if attempt == 0 {
            RetryAction::RetrySame { delay_ms: 200 }
        } else {
            RetryAction::Fatal
        };
    }
    // 400 + 积分耗尽（issue #67）：请求本身合法、账号积分已尽 → 换号。
    // 默认 400 → Fatal 会把积分耗尽直接透传终止，池内其余账号积压不用
    if status == 400 && super::is_credit_exhausted_error(body) {
        return RetryAction::SwitchKey;
    }
    // 400 + 11140 账号级风控（issue #136 反馈）：账号被上游限制使用，与 11128
    // 渠道级相反——换号有意义（受限账号由 classify → Forbidden 禁用），
    // Fatal 会把请求卡死在受限账号上每问必败
    if status == 400 && is_account_restricted_error(body) {
        return RetryAction::SwitchKey;
    }
    match status {
        // 429：优先 Retry-After；缺省线性退避 1/2/3s；耗尽后换号
        429 => {
            if attempt >= 3 {
                return RetryAction::SwitchKey;
            }
            let delay = retry_after
                .filter(|s| *s > 0)
                .map(|s| s.saturating_mul(1000))
                .unwrap_or(1000 * (attempt as u64 + 1));
            RetryAction::RetrySame { delay_ms: delay }
        }
        // 503 / 529：指数退避 10/20/40s（并入模型级渐进退避），耗尽后换号
        503 | 529 => {
            if attempt >= 3 {
                return RetryAction::SwitchKey;
            }
            RetryAction::RetrySame {
                delay_ms: 10_000u64 << attempt.min(2),
            }
        }
        // 502：同 Key 重试 1 次（错误三态 RETRY_SAME）
        502 => {
            if attempt == 0 {
                RetryAction::RetrySame { delay_ms: 500 }
            } else {
                RetryAction::SwitchKey
            }
        }
        // 401/403：凭证失效/封禁 → 刷新换号
        401 | 403 => RetryAction::SwitchKey,
        // 400：请求本身问题（context_too_long 等），换号无意义
        400 => RetryAction::Fatal,
        // 其余 5xx：换号试试
        s if s >= 500 => RetryAction::SwitchKey,
        // 其余 4xx：终止
        _ => RetryAction::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_signature_retries_once_with_200ms() {
        assert_eq!(
            retry_plan(400, "invalid thinking.signature", 0, None),
            RetryAction::RetrySame { delay_ms: 200 }
        );
        assert_eq!(
            retry_plan(400, "invalid thinking.signature", 1, None),
            RetryAction::Fatal
        );
        // 非 400 的 thinking.signature 不走该分支
        assert_eq!(
            retry_plan(500, "thinking.signature", 0, None),
            RetryAction::SwitchKey
        );
    }

    #[test]
    fn rate_limit_prefers_retry_after_header() {
        assert_eq!(
            retry_plan(429, "rate limited", 0, Some(7)),
            RetryAction::RetrySame { delay_ms: 7000 }
        );
        // 缺省线性退避 1/2/3s
        assert_eq!(
            retry_plan(429, "", 0, None),
            RetryAction::RetrySame { delay_ms: 1000 }
        );
        assert_eq!(
            retry_plan(429, "", 1, None),
            RetryAction::RetrySame { delay_ms: 2000 }
        );
        assert_eq!(
            retry_plan(429, "", 2, None),
            RetryAction::RetrySame { delay_ms: 3000 }
        );
        // 3 次耗尽后换号
        assert_eq!(retry_plan(429, "", 3, None), RetryAction::SwitchKey);
    }

    #[test]
    fn server_errors_backoff_exponentially() {
        assert_eq!(
            retry_plan(503, "", 0, None),
            RetryAction::RetrySame { delay_ms: 10_000 }
        );
        assert_eq!(
            retry_plan(529, "", 1, None),
            RetryAction::RetrySame { delay_ms: 20_000 }
        );
        assert_eq!(
            retry_plan(503, "", 2, None),
            RetryAction::RetrySame { delay_ms: 40_000 }
        );
        assert_eq!(retry_plan(529, "", 3, None), RetryAction::SwitchKey);
    }

    #[test]
    fn bad_gateway_retries_same_once() {
        assert_eq!(
            retry_plan(502, "", 0, None),
            RetryAction::RetrySame { delay_ms: 500 }
        );
        assert_eq!(retry_plan(502, "", 1, None), RetryAction::SwitchKey);
    }

    #[test]
    fn auth_errors_switch_and_bad_request_is_fatal() {
        assert_eq!(retry_plan(401, "", 0, None), RetryAction::SwitchKey);
        assert_eq!(retry_plan(403, "", 0, None), RetryAction::SwitchKey);
        assert_eq!(retry_plan(400, "context_too_long", 0, None), RetryAction::Fatal);
        assert_eq!(retry_plan(404, "", 0, None), RetryAction::Fatal);
        assert_eq!(retry_plan(422, "", 0, None), RetryAction::Fatal);
    }

    #[test]
    fn credit_exhausted_400_switches_key() {
        // issue #67：400 + 积分耗尽短语 → 换号（非 Fatal）
        assert_eq!(
            retry_plan(400, "insufficient credit balance", 0, None),
            RetryAction::SwitchKey
        );
        assert_eq!(retry_plan(400, "积分不足，请充值", 0, None), RetryAction::SwitchKey);
        assert_eq!(retry_plan(400, "账户额度不足", 0, None), RetryAction::SwitchKey);
        // 同号重试（attempt>0）后耗尽仍换号——issue #67 核心场景
        assert_eq!(
            retry_plan(400, "insufficient credit", 1, None),
            RetryAction::SwitchKey
        );
        // quota exceeded 有意不命中（4008 频率限流走 SoftRate 口径）
        assert_eq!(retry_plan(400, "quota exceeded", 0, None), RetryAction::Fatal);
        // 非 400 状态码不受影响
        assert_eq!(retry_plan(500, "insufficient credit", 0, None), RetryAction::SwitchKey);
    }

    #[test]
    fn illegal_channel_code_match_tolerates_whitespace() {
        // 紧凑与空白两种序列化形态均命中（issue #57 审查修复）
        assert!(is_illegal_channel_error(r#"{"error":{"code":11128,"msg":"x"}}"#));
        assert!(is_illegal_channel_error(r#"{"error":{"code": 11128 }}"#));
        assert!(is_illegal_channel_error(r#"{"error" : {"code"
: 11128}}"#));
        // 数字边界：更长数字不误撞
        assert!(!is_illegal_channel_error(r#"{"code":111280}"#));
        assert!(!is_illegal_channel_error(r#"{"code":111289}"#));
        // 键名引号边界：some_code 等字段不误撞
        assert!(!is_illegal_channel_error(r#"{"some_code":11128}"#));
        // 关键词兜底仍有效
        assert!(is_illegal_channel_error("Illegal API invocation from channel"));
        assert!(!is_illegal_channel_error("normal upstream error"));
    }

    #[test]
    fn account_restricted_error_and_retry_plan() {
        // 11140 账号级风控识别（issue #136 反馈）：code 形态 + 关键词形态
        assert!(is_account_restricted_error(
            r#"{"error":{"code":11140,"msg":"账号存在异常行为，已被限制使用"}}"#
        ));
        assert!(is_account_restricted_error(
            "系统检测到该账号存在异常使用行为，如需申诉请提交反馈 11140"
        ));
        assert!(is_account_restricted_error("账号已被限制使用"));
        // 数字边界：更长/相邻数字不误撞
        assert!(!is_account_restricted_error(r#"{"code":111400}"#));
        assert!(!is_account_restricted_error(r#"{"code":11141}"#));
        assert!(!is_account_restricted_error("insufficient credit"));
        // 400 + 11140 → SwitchKey（换号接替），不得 Fatal 卡死在受限账号
        assert_eq!(
            retry_plan(400, r#"{"code":11140,"msg":"账号存在异常行为，已被限制使用"}"#, 0, None),
            RetryAction::SwitchKey
        );
        // 与 11128 渠道级互不误撞
        assert!(!is_illegal_channel_error(
            r#"{"code":11140,"msg":"账号存在异常行为"}"#
        ));
        assert!(!is_account_restricted_error(
            r#"{"code":11128,"msg":"request illegal"}"#
        ));
    }
}
