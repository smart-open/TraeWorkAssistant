//! F-81：执行期跨池重试编排（第一期：流式三池）
//!
//! 池内取号耗尽（轮换耗尽收尾，必然未发出任何上游数据帧——已流出数据后的失败
//! 走就地收尾不进耗尽分支）时，按调度时点收集的健康候选链把剩余流生命周期
//! 移交给下一池。guard 按值移交：下一池取号成功时 bind_account 自动解绑旧绑
//! 新（wb_route 先例），全部耗尽时 Drop 兜底释放。
//!
//! 第一期范围：Buddy/BuddyAi ↔ Qoder 双向 + Trae 耗尽 → Buddy/Qoder；
//! Trae 作为移交目标（wb→trae / qoder→trae）待 Trae 流核心抽出后接入
//! （见 backlog F-81 第一期范围说明），Custom 池组内已有多上游冗余不参与移交。

use super::dispatch::{PoolCandidate, TargetPool};
use super::routes::Protocol;

/// 跨池移交落 [SCHED] 行（逐请求口径，对齐落池日志先例）。
/// 在移交发起点调用——文案为「移交发起」而非「移交成功」（审查 L：若接收池
/// 立即失败，先前的成功文案会误导排障；接收路径有自己的落池/请求行佐证实际结果）
fn log_cross_pool(
    state: &std::sync::Arc<super::ApiSharedState>,
    from: &str,
    to: &PoolCandidate,
) {
    state.logger.log_sched_event(&format!(
        "cross_pool_retry: from={} to={} model={}（池内耗尽移交发起）",
        from,
        to.pool.as_str(),
        to.model,
    ));
}

/// WB 池 body 预处理（对齐 handler 分派段：effort 提示注入）
fn prepare_wb_body(
    shared: &std::sync::Arc<super::ApiSharedState>,
    head: &PoolCandidate,
    body_vec: &[u8],
) -> Vec<u8> {
    let explicit = serde_json::from_slice::<serde_json::Value>(body_vec)
        .ok()
        .and_then(|v| v.get("reasoning_effort").and_then(|x| x.as_str()).map(|_| ()))
        .is_some();
    let hint = super::routes::effective_effort_hint(shared, head.effort_hint.clone(), explicit);
    super::routes::apply_effort_hint(body_vec.to_vec(), hint)
}

/// 审查修复（M，Trae 专有字段泄漏）：Trae 发起移交时 body 已含 Trae 出站注入
/// （`reasoning_effort_level` / `is_max_mode`，见 routes::inject_trae_outbound——
/// Trae 三协议入口在分派前注入），移交到 Buddy/Qoder 上游属非常规字段噪声
/// （指纹清洗体系 issue #122/#150 想消除的那类），且字段值（如 effort 取值
/// extra_high）对目标池无意义。统一在移交入口剥离后按目标池重建
///（effort 提示由 prepare_wb_body 按目标池 effort_hint 注入，语义不丢）。
fn strip_trae_outbound_fields(body_vec: &[u8]) -> Vec<u8> {
    serde_json::from_slice::<serde_json::Value>(body_vec)
        .ok()
        .and_then(|mut v| {
            if let Some(obj) = v.as_object_mut() {
                // 注意：两处 remove 必须都执行（短路 || 会让第二个字段残留）
                let removed_effort = obj.remove(super::routes::TRAE_EFFORT_FIELD).is_some();
                let removed_max = obj.remove(super::routes::TRAE_MAX_MODE_FIELD).is_some();
                if removed_effort || removed_max {
                    return serde_json::to_vec(&v).ok();
                }
            }
            None
        })
        .unwrap_or_else(|| body_vec.to_vec())
}

/// 把剩余流生命周期移交给下一候选池。`from` 为移交发起方池标签（日志用）。
/// 返回 false = 剩余候选中无第一期支持的池（Trae/Custom 目标被跳过后无可用候选），
/// 调用方继续原错误帧收尾。
/// 审查 P1 修复：Trae/Custom 目标候选被跳过后继续尝试链上其余候选
///（原实现直接 return false 会吞掉后续 wb/qoder 候选，如链=[trae, qoder]）。
///
/// `encoded` 透传语义（审查修复 H-1 重钉）：仅在 proto==Responses 时消费（wb_sse
/// 的工具名还原）。qoder 自身出站不消费，但其移交入口必须透传——Responses 请求
/// 恒由 wb 池受理（routes.rs /v1/responses 入口），wb 耗尽移交 qoder 再耗尽移交
/// 回 wb 时，wb 侧需要原集合做还原；断链会导致还原静默丢失（不报错）。
/// trae 移交入口传空集成立：trae 非第一期移交目标，且 Responses 不进 trae solo
/// 管线；wb 移交入口恒持真实集合。
#[allow(clippy::too_many_arguments)]
pub fn continue_stream_on_next_pool(
    shared: &std::sync::Arc<super::ApiSharedState>,
    from: &str,
    next: &[PoolCandidate],
    body_vec: &[u8],
    proto: Protocol,
    key_id: &str,
    chat_id: &str,
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    guard: super::InflightGuard,
    encoded: &std::collections::HashSet<String>,
    start_ts: std::time::Instant,
) -> bool {
    let stripped = strip_trae_outbound_fields(body_vec);
    let body_vec: &[u8] = &stripped;
    for (i, cand) in next.iter().enumerate() {
        let rest = &next[i + 1..];
        match cand.pool {
            TargetPool::Buddy | TargetPool::BuddyAi => {
                log_cross_pool(shared, from, cand);
                let body = prepare_wb_body(shared, cand, body_vec);
                let view =
                    super::wb_route::WbView::new(shared.clone(), cand.pool == TargetPool::BuddyAi);
                super::wb_route::run_wb_stream(
                    &view,
                    &body,
                    &cand.model,
                    proto,
                    key_id,
                    chat_id,
                    tx,
                    start_ts,
                    guard,
                    encoded,
                    rest,
                );
                return true;
            }
            TargetPool::Qoder => {
                log_cross_pool(shared, from, cand);
                super::qoder_route::run_qoder_stream(
                    shared,
                    body_vec,
                    &cand.model,
                    proto,
                    key_id,
                    chat_id,
                    tx,
                    start_ts,
                    guard,
                    rest,
                    encoded,
                );
                return true;
            }
            // 第一期暂不支持（Trae 流核心未抽出 / Custom 组内自冗余）：跳过该候选
            TargetPool::Trae | TargetPool::Custom => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::strip_trae_outbound_fields;

    /// Trae 专有出站字段剥离：reasoning_effort_level / is_max_mode 均被移除，
    /// 其余字段（含标准 reasoning_effort）原样保留（审查修复 M 回归锁定）
    #[test]
    fn strip_trae_outbound_removes_trae_only_fields() {
        let body = br#"{"model":"glm-5.3","reasoning_effort_level":"extra_high","is_max_mode":true,"reasoning_effort":"high","messages":[]}"#;
        let out = strip_trae_outbound_fields(body);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("reasoning_effort_level").is_none());
        assert!(v.get("is_max_mode").is_none());
        assert_eq!(v.get("reasoning_effort").and_then(|x| x.as_str()), Some("high"));
        assert!(v.get("messages").is_some());
    }

    /// 无 Trae 专有字段的 body 原样返回（零重序列化；仍有一次全量拷贝——
    /// unwrap_or_else 兜底路径，仅 trae 发起移交且无字段时的非常见场景）
    #[test]
    fn strip_trae_outbound_passthrough_without_trae_fields() {
        let body = br#"{"model":"glm-5.3","messages":[]}"#;
        assert_eq!(strip_trae_outbound_fields(body), body.to_vec());
    }

    /// 非 JSON body（畸形）原样返回不 panic
    #[test]
    fn strip_trae_outbound_malformed_body_passthrough() {
        let body = b"not-json";
        assert_eq!(strip_trae_outbound_fields(body), body.to_vec());
    }
}
