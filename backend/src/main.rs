use std::net::SocketAddr;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{IntoResponse, Redirect},
    routing::{get, post},
    Router,
};
use tower_http::services::ServeDir;

mod app;
mod apple;
mod models;
mod proxy_pool;
mod wechat;

async fn vue_index() -> Redirect {
    Redirect::permanent("/static/web/")
}

/// WebSocket handler: pushes state updates to connected clients
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<app::AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: app::AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 变更检测用哈希而非完整字符串比较：
    // state 的 JSON 在 100 账号场景下可达上百 KB，每 2 秒做一次全串比较是纯浪费。
    let mut last_state_hash: u64 = 0;
    let mut last_logs_hash: u64 = 0;
    let mut first = true;

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    // 客户端主动发消息（前端心跳）：回一个 Pong 保活
                    Some(Ok(_)) => {
                        if socket.send(Message::Pong(Vec::new())).await.is_err() { break; }
                    }
                    _ => {}
                }
            }
            _ = interval.tick() => {
                let state_payload = state.serialize_state_for_ws().await;
                let state_hash = fnv1a_hash(state_payload.as_bytes());
                // 首帧必发（即使 hash 恰好为 0），之后仅在内容变化时推送
                if first || state_hash != last_state_hash {
                    last_state_hash = state_hash;
                    let msg = format!("{{\"type\":\"state\",\"data\":{}}}", state_payload);
                    if socket.send(Message::Text(msg)).await.is_err() { break; }
                }

                let logs_payload = state.serialize_logs_for_ws().await;
                let logs_hash = fnv1a_hash(logs_payload.as_bytes());
                if first || logs_hash != last_logs_hash {
                    last_logs_hash = logs_hash;
                    let msg = format!("{{\"type\":\"logs\",\"data\":{}}}", logs_payload);
                    if socket.send(Message::Text(msg)).await.is_err() { break; }
                }
                first = false;
            }
        }
    }
}

/// FNV-1a 64 位哈希：用于 WebSocket 变更检测。
/// 选它是因为够快（无分配、逐字节）且实现零依赖 —— 这里只需要「是否变化」的判断，
/// 不用于安全场景，抗碰撞要求不高。
pub fn fnv1a_hash(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[tokio::main]
async fn main() {
    let state = app::AppState::load_or_create("config.bin").await;
    state.start_session_keeper().await;
    state.start_proxy_pool_keeper().await;

    let app = Router::new()
        .route("/", get(vue_index))
        .route("/ws", get(ws_handler))
        .route("/api/state", get(app::api_state))
        .route("/api/logs", get(app::api_logs))
        .route("/api/logs/clear", post(app::api_clear_logs))
        .route("/api/models", get(app::api_models))
        .route("/api/stock/:id", post(app::api_stock))
        .route("/api/accounts/:id/bootstrap", post(app::api_bootstrap_account))
        .route("/api/start", post(app::api_start))
        .route("/api/stop", post(app::api_stop))
        .route("/api/central/clear-completed", post(app::api_clear_completed_orders))
        .route("/api/accounts", post(app::api_add_account))
        .route("/api/accounts/import", post(app::api_import_accounts))
        .route("/api/accounts/cookies", post(app::api_import_cookies))
        .route("/api/accounts/delete", post(app::api_delete_accounts))
        .route("/api/accounts/apply-bulk", post(app::api_apply_account_bulk))
        .route("/api/accounts/:id", axum::routing::post(app::api_update_account))
        .route("/api/accounts/:id", get(app::api_get_account))
        .route("/api/accounts/:id/start", post(app::api_start_account))
        .route("/api/accounts/:id/stop", post(app::api_stop_account))
        .route("/api/accounts/:id/order", post(app::api_manual_order))
        .route("/api/accounts/:id/wechat-order", post(app::api_wechat_order))
        .route("/api/accounts/:id/wechat-prepare", post(app::api_wechat_prepare))
        .route("/api/accounts/:id/wechat-place", post(app::api_wechat_place))
        .route("/api/wechat/prepare-batch", post(app::api_wechat_prepare_batch))
        .route(
            "/api/wechat/warmup-batch",
            post(app::api_wechat_warmup_batch),
        )
        .route("/api/proxy-pool/stats", get(app::api_proxy_pool_stats))
        .route("/api/proxy-pool/clear", post(app::api_proxy_pool_clear))
        .route("/api/proxy-pool/fetch", post(app::api_proxy_pool_fetch))
        .route("/api/settings", post(app::api_update_settings))
        .route("/api/login/start", post(app::api_login_start))
        .route("/api/login/verify", post(app::api_login_verify))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    println!("Dashboard: http://0.0.0.0:8080");
    println!("WebSocket: ws://0.0.0.0:8080/ws");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind 0.0.0.0:8080");
    axum::serve(listener, app).await.expect("server stopped");
}
