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
}
