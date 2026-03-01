//! HTTP API 服务
//!
//! 提供 REST + WebSocket 接口:
//! - GET  /status        — 服务状态 (免认证)
//! - GET  /contacts      — 联系人列表 (数据库)
//! - GET  /sessions      — 会话列表 (优先数据库)
//! - GET  /messages      — 当前聊天全部消息
//! - GET  /messages/new  — 增量新消息 (优先数据库)
//! - POST /send          — 发送消息 (AT-SPI)
//! - POST /chat          — 切换聊天 (AT-SPI)
//! - POST /listen        — 添加监听 (弹出独立窗口)
//! - DELETE /listen      — 移除监听
//! - GET  /listen        — 监听列表
//! - GET  /listen/messages — 所有监听窗口的新消息
//! - GET  /debug/tree    — AT-SPI2 控件树
//! - GET  /ws            — WebSocket 实时推送

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post, delete},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::atspi::AtSpi;
use crate::db::DbManager;
use crate::input::InputEngine;
use crate::wechat::WeChat;

// =====================================================================
// 共享状态
// =====================================================================

pub struct AppState {
    pub wechat: Arc<WeChat>,
    pub atspi: Arc<AtSpi>,
    /// InputEngine 命令队列 (替代 Mutex, 消除长持锁)
    pub input_tx: tokio::sync::mpsc::Sender<InputCommand>,
    pub tx: broadcast::Sender<String>,
    /// 数据库管理器 (密钥获取成功时可用)
    pub db: Option<Arc<DbManager>>,
    /// API 认证 Token (None = 不启用认证)
    pub api_token: Option<String>,
    /// 启动时间 (用于 uptime 计算)
    pub start_time: std::time::Instant,
}

// =====================================================================
// InputEngine Actor
// =====================================================================

use tokio::sync::oneshot;

/// InputEngine 命令 (经 mpsc 队列发送给 actor)
pub enum InputCommand {
    SendMessage {
        to: String,
        text: String,
        skip_verify: bool,
        reply: oneshot::Sender<anyhow::Result<(bool, bool, String)>>,
    },
    SendImage {
        to: String,
        image_path: String,
        reply: oneshot::Sender<anyhow::Result<(bool, bool, String)>>,
    },
    ChatWith {
        who: String,
        reply: oneshot::Sender<anyhow::Result<Option<String>>>,
    },
    AddListen {
        who: String,
        reply: oneshot::Sender<anyhow::Result<bool>>,
    },
    RemoveListen {
        who: String,
        reply: oneshot::Sender<bool>,
    },
}

/// 启动 InputEngine actor (在独立 task 中顺序执行命令)
pub fn spawn_input_actor(
    mut engine: InputEngine,
    wechat: Arc<WeChat>,
    mut rx: tokio::sync::mpsc::Receiver<InputCommand>,
) {
    tokio::spawn(async move {
        info!("🎮 InputEngine actor 已启动");
        while let Some(cmd) = rx.recv().await {
            match cmd {
                InputCommand::SendMessage { to, text, skip_verify, reply } => {
                    let result = wechat.send_message(&mut engine, &to, &text, skip_verify).await;
                    let _ = reply.send(result);
                }
                InputCommand::SendImage { to, image_path, reply } => {
                    let result = wechat.send_image(&mut engine, &to, &image_path).await;
                    let _ = reply.send(result);
                }
                InputCommand::ChatWith { who, reply } => {
                    let result = wechat.chat_with(&mut engine, &who).await;
                    let _ = reply.send(result);
                }
                InputCommand::AddListen { who, reply } => {
                    let result = wechat.add_listen(&mut engine, &who).await;
                    let _ = reply.send(result);
                }
                InputCommand::RemoveListen { who, reply } => {
                    let result = wechat.remove_listen(&engine, &who).await;
                    let _ = reply.send(result);
                }
            }
        }
        info!("🎮 InputEngine actor 已停止");
    });
}

// =====================================================================
// 工具函数
// =====================================================================

/// 简单的 URL percent decode (%XX → 字节)
fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.as_bytes().iter();
    while let Some(&b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().copied().unwrap_or(0);
            let lo = chars.next().copied().unwrap_or(0);
            if let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo)) {
                bytes.push(h << 4 | l);
                continue;
            }
        }
        bytes.push(b);
    }
    String::from_utf8(bytes).unwrap_or_else(|_| input.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 轻量伪随机 u16 (无需引入 rand crate, 用时间纳秒低位)
fn rand_u16() -> u16 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.subsec_nanos() ^ (t.as_millis() as u32)) as u16
}

// =====================================================================
// 统一错误响应
// =====================================================================

/// API 错误类型 (带 HTTP 状态码)
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn unavailable(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::SERVICE_UNAVAILABLE, message: msg.into() }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: msg.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

// =====================================================================
// 认证中间件
// =====================================================================

/// Token 认证中间件
/// 检查 Header `Authorization: Bearer <token>` 或 Query `?token=<token>`
async fn auth_layer(
    State(state): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<impl IntoResponse, StatusCode> {
    let token = match &state.api_token {
        Some(t) => t,
        None => return Ok(next.run(req).await), // 未配置 token, 跳过认证
    };

    // 1. 检查 Authorization header
    if let Some(auth) = req.headers().get("authorization") {
        if let Ok(auth_str) = auth.to_str() {
            if let Some(bearer) = auth_str.strip_prefix("Bearer ") {
                if bearer.trim() == token {
                    return Ok(next.run(req).await);
                }
            }
        }
    }

    // 2. 检查 query param ?token=xxx (需 URL decode)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("token=") {
                // URL decode: %23 → #, %20 → space, etc.
                let decoded = percent_decode(val);
                if decoded == *token {
                    return Ok(next.run(req).await);
                }
            }
        }
    }

    warn!("🔒 API 认证失败: {}", req.uri().path());
    Err(StatusCode::UNAUTHORIZED)
}

// =====================================================================
// 路由
// =====================================================================

pub fn build_router(state: Arc<AppState>) -> Router {
    // 需要认证的路由
    let protected = Router::new()
        .route("/contacts", get(get_contacts))
        .route("/messages", get(get_messages))
        .route("/messages/new", get(get_new_messages))
        .route("/send", post(send_message))
        .route("/send_image", post(send_image))
        .route("/sessions", get(get_sessions))
        .route("/chat", post(chat_with))
        .route("/listen", get(get_listen_list))
        .route("/listen", post(add_listen))
        .route("/listen", delete(remove_listen))
        .route("/listen/messages", get(get_listen_messages))
        .route("/debug/tree", get(get_tree))
        .route("/debug/sessions", get(get_session_tree))
        .route("/ws", get(ws_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_layer));

    // 免认证路由
    Router::new()
        .route("/status", get(get_status))
        .merge(protected)
        .layer(tower_http::cors::CorsLayer::permissive()) // ⑩ CORS 支持
        .with_state(state)
}

// =====================================================================
// 请求/响应类型
// =====================================================================

#[derive(Serialize)]
struct StatusResponse {
    status: String,
    version: String,
    listen_count: usize,
    db_available: bool,
    contacts: usize,
    uptime_secs: u64,
}

#[derive(Deserialize)]
struct SendRequest {
    to: String,
    text: String,
}

#[derive(Deserialize)]
struct SendImageRequest {
    to: String,
    /// base64 编码的图片数据
    file: String,
    /// 文件名 (可选, 用于推断 MIME 类型)
    #[serde(default = "default_image_name")]
    name: String,
}

fn default_image_name() -> String {
    "image.png".to_string()
}

#[derive(Serialize)]
struct SendResponse {
    sent: bool,
    verified: bool,
    message: String,
}

#[derive(Deserialize)]
struct ChatRequest {
    who: String,
}

#[derive(Serialize)]
struct ChatResponse {
    success: bool,
    chat_name: Option<String>,
}

#[derive(Deserialize)]
struct ListenRequest {
    who: String,
}

#[derive(Serialize)]
struct ListenResponse {
    success: bool,
    message: String,
}

// =====================================================================
// Handlers
// =====================================================================

async fn get_status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let status = state.wechat.check_status().await;
    let listen_count = state.wechat.get_listen_list().await.len();
    let db_available = state.db.is_some();
    let contacts = if let Some(ref d) = state.db {
        d.get_contacts().await.len()
    } else { 0 };
    let uptime_secs = state.start_time.elapsed().as_secs();
    Json(StatusResponse {
        status: status.to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        listen_count, db_available, contacts, uptime_secs,
    })
}

/// 联系人列表 (从数据库)
async fn get_contacts(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let db = state.db.as_ref().ok_or_else(|| ApiError::unavailable("数据库不可用"))?;
    let contacts = db.get_contacts().await;
    Ok(Json(serde_json::json!({ "contacts": contacts })))
}

async fn get_messages(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let msgs = state.wechat.get_all_messages().await;
    Json(msgs)
}

async fn get_new_messages(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // 优先使用数据库
    if let Some(db) = &state.db {
        match db.get_new_messages().await {
            Ok(msgs) => return Json(serde_json::to_value(msgs).unwrap_or_default()),
            Err(e) => {
                tracing::warn!("数据库消息查询失败, fallback AT-SPI: {}", e);
            }
        }
    }
    // Fallback: AT-SPI
    let msgs = state.wechat.get_new_messages().await;
    Json(serde_json::to_value(msgs).unwrap_or_default())
}

async fn send_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendRequest>,
) -> Result<Json<SendResponse>, ApiError> {
    // DB 可用时跳过 AT-SPI 验证, 由下面的 DB 验证替代
    let has_db = state.db.is_some();

    // 在发送前订阅自发消息广播 (避免竞态: 发送期间的广播不会丢失)
    let sent_rx = state.db.as_ref().map(|db| db.subscribe_sent());

    // 发送命令到 actor
    let (reply_tx, reply_rx) = oneshot::channel();
    state.input_tx.send(InputCommand::SendMessage {
        to: req.to.clone(),
        text: req.text.clone(),
        skip_verify: has_db,
        reply: reply_tx,
    }).await.map_err(|_| ApiError::unavailable("InputEngine actor 已停止"))?;

    match reply_rx.await {
        Ok(Ok((sent, atspi_verified, message))) => {
            // DB 验证 (优先): DB 可用时用已订阅的 receiver 等待匹配
            let verified = if let Some(rx) = sent_rx {
                state.db.as_ref().unwrap()
                    .verify_sent(&req.text, rx).await
                    .unwrap_or(atspi_verified)
            } else {
                atspi_verified
            };

            let msg_json = serde_json::json!({
                "type": "sent",
                "to": req.to,
                "text": req.text,
                "verified": verified,
            });
            let _ = state.tx.send(msg_json.to_string());
            Ok(Json(SendResponse { sent, verified, message }))
        }
        Ok(Err(e)) => Err(ApiError::internal(format!("发送失败: {e}"))),
        Err(_) => Err(ApiError::internal("actor 响应通道已关闭")),
    }
}

async fn send_image(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendImageRequest>,
) -> Result<Json<SendResponse>, ApiError> {
    use std::io::Write;

    // 解码 base64 图片
    use base64::Engine;
    let image_data = base64::engine::general_purpose::STANDARD
        .decode(&req.file)
        .map_err(|e| ApiError::internal(format!("base64 解码失败: {e}")))?;

    // 保存到临时文件
    let ext = if req.name.contains('.') {
        req.name.rsplit('.').next().unwrap_or("png")
    } else {
        "png"
    };
    let tmp_path = format!("/tmp/mimicwx_img_{}_{:04x}.{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
        rand_u16(), ext);
    {
        let mut f = std::fs::File::create(&tmp_path)
            .map_err(|e| ApiError::internal(format!("创建临时文件失败: {e}")))?;
        f.write_all(&image_data)
            .map_err(|e| ApiError::internal(format!("写入图片失败: {e}")))?;
    }

    // 发送命令到 actor
    let (reply_tx, reply_rx) = oneshot::channel();
    state.input_tx.send(InputCommand::SendImage {
        to: req.to.clone(),
        image_path: tmp_path.clone(),
        reply: reply_tx,
    }).await.map_err(|_| ApiError::unavailable("InputEngine actor 已停止"))?;

    let result = reply_rx.await;

    // 清理临时文件
    let _ = std::fs::remove_file(&tmp_path);

    match result {
        Ok(Ok((sent, verified, message))) => Ok(Json(SendResponse { sent, verified, message })),
        Ok(Err(e)) => Err(ApiError::internal(format!("发送图片失败: {e}"))),
        Err(_) => Err(ApiError::internal("actor 响应通道已关闭")),
    }
}

async fn get_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // 优先使用数据库
    if let Some(db) = &state.db {
        match db.get_sessions().await {
            Ok(sessions) => return Json(serde_json::to_value(sessions).unwrap_or_default()),
            Err(e) => {
                tracing::warn!("数据库会话查询失败, fallback AT-SPI: {}", e);
            }
        }
    }
    // Fallback: AT-SPI
    let sessions = state.wechat.list_sessions().await;
    Json(serde_json::to_value(sessions).unwrap_or_default())
}

async fn chat_with(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, ApiError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state.input_tx.send(InputCommand::ChatWith {
        who: req.who.clone(),
        reply: reply_tx,
    }).await.map_err(|_| ApiError::unavailable("InputEngine actor 已停止"))?;

    match reply_rx.await {
        Ok(Ok(Some(name))) => Ok(Json(ChatResponse { success: true, chat_name: Some(name) })),
        Ok(Ok(None)) => Ok(Json(ChatResponse { success: false, chat_name: None })),
        Ok(Err(e)) => Err(ApiError::internal(format!("切换聊天失败: {e}"))),
        Err(_) => Err(ApiError::internal("actor 响应通道已关闭")),
    }
}

async fn add_listen(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ListenRequest>,
) -> Result<Json<ListenResponse>, ApiError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state.input_tx.send(InputCommand::AddListen {
        who: req.who.clone(),
        reply: reply_tx,
    }).await.map_err(|_| ApiError::unavailable("InputEngine actor 已停止"))?;

    match reply_rx.await {
        Ok(Ok(true)) => Ok(Json(ListenResponse {
            success: true,
            message: format!("已添加监听: {}", req.who),
        })),
        Ok(Ok(false)) => Ok(Json(ListenResponse {
            success: false,
            message: format!("添加监听失败: {}", req.who),
        })),
        Ok(Err(e)) => Err(ApiError::internal(format!("添加监听错误: {e}"))),
        Err(_) => Err(ApiError::internal("actor 响应通道已关闭")),
    }
}

async fn remove_listen(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ListenRequest>,
) -> Json<ListenResponse> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let sent = state.input_tx.send(InputCommand::RemoveListen {
        who: req.who.clone(),
        reply: reply_tx,
    }).await;

    let removed = if sent.is_ok() {
        reply_rx.await.unwrap_or(false)
    } else {
        false
    };
    Json(ListenResponse {
        success: removed,
        message: if removed {
            format!("已移除监听: {}", req.who)
        } else {
            format!("未找到监听: {}", req.who)
        },
    })
}

async fn get_listen_list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let list = state.wechat.get_listen_list().await;
    Json(list)
}

async fn get_listen_messages(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let msgs = state.wechat.take_pending_messages().await;

    // 推送到 WebSocket
    for (who, new_msgs) in &msgs {
        for m in new_msgs {
            let msg_json = serde_json::json!({
                "type": "listen_message",
                "from": who,
                "msg_type": m.msg_type,
                "sender": m.sender,
                "content": m.content,
            });
            let _ = state.tx.send(msg_json.to_string());
        }
    }

    Json(msgs)
}

async fn get_tree(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let max_depth = params.get("depth")
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(5)
        .min(15);
    if let Some(app) = state.wechat.find_app().await {
        let tree = state.atspi.dump_tree(&app, max_depth).await;
        Json(tree)
    } else {
        Json(vec![])
    }
}

/// 只 dump 会话容器的子树 (用于调试)
async fn get_session_tree(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(app) = state.wechat.find_app().await {
        if let Some(container) = state.wechat.find_session_list(&app).await {
            let tree = state.atspi.dump_tree(&container, 4).await;
            return Json(tree);
        }
    }
    Json(vec![])
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.tx.subscribe();
    info!("🔌 WebSocket 连接建立");

    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_interval.tick().await; // 跳过首次

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Pong(_))) => {} // 心跳响应
                    _ => {}
                }
            }
            _ = ping_interval.tick() => {
                // ⑴ WebSocket 心跳: 每 30s 发 Ping
                if socket.send(Message::Ping(vec![].into())).await.is_err() { break; }
            }
        }
    }

    info!("🔌 WebSocket 连接断开");
}
