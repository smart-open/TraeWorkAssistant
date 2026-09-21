//! T6 SSE 事件流：`GET /api/events/checkin`。
//!
//! 广播通道（admin.events）→ BroadcastStream → SSE 帧逐条下发：
//! `event: <事件名>` + `data: <载荷 JSON 字符串>`。
//! 事件名 = "checkin-progress" | "checkin-done"（WorkBuddy 管线的
//! wb-checkin-progress / wb-oauth-* 亦经同一通道，客户端按事件名过滤）。
//! 断线重连由 EventSource 客户端自理；done 事件已落库 checkin_results，
//! 前端可在 SSE 不可用时降级轮询。

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use super::AdminState;

/// 订阅签到事件广播，以 SSE 形式下发（keep-alive 保活，代理/防火墙下不静默断连）
pub(super) async fn checkin_events(
    State(admin): State<Arc<AdminState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = admin.events.subscribe();
    // 广播 → SSE 帧：lag（接收过慢被内核丢帧）与通道错误项跳过，保持连接
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok((name, payload)) => {
            let data = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
            Some(Ok(Event::default().event(name).data(data)))
        }
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
