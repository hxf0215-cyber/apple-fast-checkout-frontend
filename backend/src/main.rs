use std::net::SocketAddr;

/// 版本号：由 build.rs 在编译时注入（取自 Cargo.toml）。
pub const APP_VERSION: &str = "0.2.0";
/// 构建时间：运行时读取 exe 自身的修改时间。
/// 比 build.rs 注入更可靠 —— build.rs 会被 cargo 缓存，不会每次编译都重跑，
/// 导致时间戳停留在首次编译。读 exe mtime 则始终反映当前二进制。
pub fn build_stamp() -> String {
    use std::sync::OnceLock;
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let Ok(exe) = std::env::current_exe() else {
                return "unknown".to_string();
            };
            let Ok(meta) = std::fs::metadata(&exe) else {
                return "unknown".to_string();
            };
            let Ok(modified) = meta.modified() else {
                return "unknown".to_string();
            };
            let secs = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // 本机是 UTC+8，统一按北京时间展示（build.rs 里也用同一算法）
            let (y, mo, d, h, mi) = civil_from_unix(secs + 8 * 3600);
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
        })
        .clone()
}

/// Unix 秒 → (年, 月, 日, 时, 分)，UTC。
/// 采用 Howard Hinnant 的 civil_from_days 算法，零依赖。
fn civil_from_unix(secs: i64) -> (i32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y } as i32;
    (y, mo, d, h, mi)
}
/// 构建时的 git 短哈希，便于定位提交。
pub const GIT_HASH: &str = "unknown";
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

/// 把工作目录切到「可执行文件所在目录」。
///
/// 原因：程序里所有资源路径（static/、config.bin、logs/、backups/）都是相对的。
/// 双击 exe 时工作目录可能就是 exe 目录，但从快捷方式、命令行或其他目录启动时不是，
/// 会导致找不到 static/ 而启动失败（表现为"闪退"）。这里统一修正。
/// 开发时（cargo run）exe 在 target/release/ 下，需要向上找到项目根（含 Cargo.toml 的那层）。
fn set_working_dir_to_exe_dir() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(exe_dir) = exe.parent() else {
        return;
    };

    // cargo run 场景：target/release/xxx.exe → 向上查找含 Cargo.toml 的目录
    let mut candidate = exe_dir.to_path_buf();
    for _ in 0..4 {
        if candidate.join("Cargo.toml").exists() {
            break;
        }
        match candidate.parent() {
            Some(parent) => candidate = parent.to_path_buf(),
            None => break,
        }
    }

    // 发布版：exe 目录本身就是根（含 static/）
    let root = if exe_dir.join("static").exists() {
        exe_dir.to_path_buf()
    } else if candidate.join("static").exists() {
        candidate
    } else {
        exe_dir.to_path_buf()
    };

    if std::env::set_current_dir(&root).is_ok() {
        println!("Working directory: {}", root.display());
    }
}

/// 给响应加缓存控制头，避免前端升级后浏览器仍使用旧资源。
///
/// 背景：前端产物里 index.html 是固定文件名，若被浏览器强缓存，
/// 即使服务端已更新，页面仍会去加载「旧 hash」的 chunk，表现为「改了没生效」。
/// 这里统一设置为 no-cache（允许缓存但每次必须校验，304 命中时开销很小）。
async fn add_cache_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut res = next.run(req).await;
    // 只有静态资源需要这个策略；API 响应本来就带 no-store 语义，不必强改。
    let is_static = res.headers().get("content-type").is_some();
    if is_static {
        res.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache, must-revalidate"),
        );
    }
    res
}

/// 返回当前版本信息。前端用它显示版本号，也便于脚本校验部署的是哪一版。
async fn api_version() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "version": APP_VERSION,
        "build": build_stamp(),
        "git": GIT_HASH,
    }))
}

#[tokio::main]
async fn main() {
    // 必须在读取任何资源前执行：修正工作目录到 exe 所在目录
    set_working_dir_to_exe_dir();

    let state = app::AppState::load_or_create("config.bin").await;
    state.start_session_keeper().await;
    state.start_proxy_pool_keeper().await;

    let app = Router::new()
        .route("/", get(vue_index))
        .route("/ws", get(ws_handler))
        .route("/api/state", get(app::api_state))
        .route("/api/version", get(api_version))
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
        .route("/api/proxy/test", post(app::api_proxy_test))
        .route("/api/proxy-pool/stats", get(app::api_proxy_pool_stats))
        .route("/api/proxy-pool/clear", post(app::api_proxy_pool_clear))
        .route("/api/proxy-pool/fetch", post(app::api_proxy_pool_fetch))
        .route("/api/settings", post(app::api_update_settings))
        .route("/api/login/start", post(app::api_login_start))
        .route("/api/login/verify", post(app::api_login_verify))
        .nest_service(
            "/static",
            ServeDir::new("static")
        )
        // 缓存策略：index.html 等入口文件必须每次校验，否则升级后浏览器仍加载旧 JS。
        // 带 hash 的 chunk 本身可长缓存，但为了简化统一改为 no-cache（协商缓存，命中 304 也很轻）。
        .layer(axum::middleware::from_fn(add_cache_headers))
        .with_state(state);

    // 端口自动探测：8080 被占用时依次尝试后续端口，避免直接崩溃（双击启动时很常见）
    let (listener, port) = {
        let mut bound = None;
        for candidate in 8080u16..8090 {
            let addr = SocketAddr::from(([0, 0, 0, 0], candidate));
            match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => {
                    bound = Some((l, candidate));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => {
                    eprintln!("无法绑定端口 {candidate}：{e}");
                    std::process::exit(1);
                }
            }
        }
        match bound {
            Some(v) => v,
            None => {
                eprintln!("8080-8089 全部被占用，请先关闭占用端口的程序。");
                std::process::exit(1);
            }
        }
    };

    println!("========================================");
    println!("  Apple Fast Checkout");
    println!("  版本: {APP_VERSION}  构建: {}", build_stamp());
    println!("  Git:  {GIT_HASH}");
    println!("----------------------------------------");
    println!("  控制台地址: http://127.0.0.1:{port}");
    println!("  局域网访问: http://本机IP:{port}");
    println!("  关闭本窗口即停止服务");
    println!("========================================");

    // Windows：设置控制台窗口标题，方便在任务栏/多窗口时辨认版本
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", &format!("title Apple Fast Checkout {APP_VERSION} - {}", build_stamp())])
            .spawn();
    }

    // 自动打开浏览器（失败不影响服务）
    let url = format!("http://127.0.0.1:{port}");
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn();
    }

    axum::serve(listener, app).await.expect("server stopped");
}
