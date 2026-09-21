//! WebSocket 双向推送（Phase 3 T12c）：`GET /api/ws`
//! - 复用管理面 Cookie 鉴权（auth_middleware 覆盖 /api/* 后再升级）；
//! - 服务端把 broadcast 事件以 `{event, payload}` JSON 文本帧推送；
//! - 客户端控制帧：`{"type":"ping"}` → `{"type":"pong"}`（30s 心跳保活防反代 idle）；
//!   `{"type":"subscribe"|"unsubscribe","events":[...]}` → 事件过滤（默认全量）；
//! - 慢消费者丢帧：Lagged 时下发 `ws-lagged` 事件提示（done 数据已落库可降级拉取）；
//! - 单连接读写分离：控制帧经 mpsc 通道转交写循环统一执行，过滤集无锁（仅写侧持有）。

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use super::AdminState;

/// GET /api/ws：Cookie 鉴权通过后升级为 WebSocket，进入事件转发循环
pub(super) async fn ws_handler(State(admin): State<Arc<AdminState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, admin))
}

/// 客户端控制帧（读任务解析后经通道转交写任务统一执行）
#[derive(Debug)]
enum Ctl {
    /// 回 `{"type":"pong"}`（保活探测）
    Pong,
    /// 增量订阅事件名
    Subscribe(Vec<String>),
    /// 取消订阅事件名
    Unsubscribe(Vec<String>),
}

/// 解析客户端控制帧；无法识别的帧静默忽略
fn parse_ctl(text: &str) -> Option<Ctl> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v.get("type")?.as_str()? {
        "ping" => Some(Ctl::Pong),
        "subscribe" => Some(Ctl::Subscribe(events_from(&v))),
        "unsubscribe" => Some(Ctl::Unsubscribe(events_from(&v))),
        _ => None,
    }
}

/// 控制帧中的事件名列表（缺失/类型不符 → 空表）
fn events_from(v: &Value) -> Vec<String> {
    v.get("events")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

async fn handle_socket(socket: WebSocket, admin: Arc<AdminState>) {
    let (mut tx, mut rx) = socket.split();
    let mut events = admin.events.subscribe();
    let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Ctl>();

    // 读任务：控制帧解析 → 通道；连接关闭/异常时任务结束（写循环随之退出）
    let read_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = rx.next().await {
            match msg {
                Message::Text(text) => {
                    if let Some(ctl) = parse_ctl(&text) {
                        if ctl_tx.send(ctl).is_err() {
                            break;
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {} // Binary / Ping / Pong：Ping 由 axum 底层自动回 Pong
            }
        }
    });

    // 写循环：broadcast 事件 + 控制通道 select；None（读端结束）即退出
    let mut filter: Option<HashSet<String>> = None;
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok((name, payload)) => {
                    if filter.as_ref().map_or(true, |s| s.contains(&name)) {
                        let frame = json!({"event": name, "payload": payload}).to_string();
                        if tx.send(Message::Text(frame)).await.is_err() {
                            break;
                        }
                    }
                }
                // 慢消费者丢帧：提示后继续（前端可降级拉落库数据）
                Err(RecvError::Lagged(n)) => {
                    let frame = json!({"event": "ws-lagged", "payload": {"missed": n}}).to_string();
                    if tx.send(Message::Text(frame)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            ctl = ctl_rx.recv() => match ctl {
                None => break,
                Some(Ctl::Pong) => {
                    if tx.send(Message::Text(r#"{"type":"pong"}"#.to_string())).await.is_err() {
                        break;
                    }
                }
                Some(Ctl::Subscribe(list)) => {
                    filter.get_or_insert_with(HashSet::new).extend(list);
                }
                Some(Ctl::Unsubscribe(list)) => {
                    if let Some(s) = filter.as_mut() {
                        for name in list {
                            s.remove(&name);
                        }
                    }
                }
            },
        }
    }
    read_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 控制帧解析：ping / subscribe / unsubscribe / 非法与未知帧忽略
    #[test]
    fn parse_ctl_variants() {
        assert!(matches!(parse_ctl(r#"{"type":"ping"}"#), Some(Ctl::Pong)));
        match parse_ctl(r#"{"type":"subscribe","events":["a","b"]}"#) {
            Some(Ctl::Subscribe(list)) => assert_eq!(list, vec!["a", "b"]),
            other => panic!("期望 Subscribe，实际 {other:?}"),
        }
        match parse_ctl(r#"{"type":"unsubscribe","events":["a"]}"#) {
            Some(Ctl::Unsubscribe(list)) => assert_eq!(list, vec!["a"]),
            other => panic!("期望 Unsubscribe，实际 {other:?}"),
        }
        // 非法 / 未知类型 / 缺 type → 忽略；events 缺省 → 空表（空表不改变订阅语义）
        assert!(parse_ctl("not json").is_none());
        assert!(parse_ctl(r#"{"type":"unknown"}"#).is_none());
        assert!(parse_ctl(r#"{"foo":1}"#).is_none());
        assert!(matches!(
            parse_ctl(r#"{"type":"subscribe"}"#),
            Some(Ctl::Subscribe(list)) if list.is_empty()
        ));
    }
}
