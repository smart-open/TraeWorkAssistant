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
//!
//! 审查 A-5/H-1：`encoded` 工具名集合（Responses 协议 wb 侧 namespace 编码名）
//! 随执行链全链透传（wb_stream_chat/run_wb_stream → continue_stream_on_next_pool
//! → Qoder 分支 → run_qoder_stream → try_cross_pool → 回 wb 分支）。集合由 wb
//! 受理 Responses 时收集（routes.rs encoded_tool_names）；各协议直受理入口传空集。
//! 当前透传链不可达（防御性预留）：Responses 流式入口 routes.rs responses_api 以
//! 空移交链调用 wb_stream_chat，移交链恒空、集合恒不参与，零行为变化。未来
//! Responses 接入 dispatch 建链时集合随链生效：回程投影按集合成员把编码名还原为
//! {name, namespace} 双字段（issue #93 Codex 契约），无需再改执行链签名。

use super::dispatch::{PoolCandidate, TargetPool};
use super::routes::Protocol;

/// 跨池移交落 [SCHED] 行（逐请求口径，对齐落池日志先例）。
/// 在移交发起点调用（目标池执行前）；若目标池执行仍耗尽，该 [SCHED] 行不撤销，
/// 后续结果见落池日志
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

/// 把剩余流生命周期移交给下一候选池。`from` 为移交发起方池标签（日志用）。
/// 返回 false = 剩余候选中无第一期支持的池（Trae/Custom 目标被跳过后无可用候选），
/// 调用方继续原错误帧收尾。
/// 审查 P1 修复：Trae/Custom 目标候选被跳过后继续尝试链上其余候选
///（原实现直接 return false 会吞掉后续 wb/qoder 候选，如链=[trae, qoder]）。
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
                    encoded,
                    rest,
                );
                return true;
            }
            // 第一期暂不支持（Trae 流核心未抽出 / Custom 组内自冗余）：跳过该候选
            TargetPool::Trae | TargetPool::Custom => {}
        }
    }
    false
}
