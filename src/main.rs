//! MimicWX-Linux: 微信自动化框架
//!
//! 架构:
//! - atspi: AT-SPI2 底层原语 (D-Bus 通信) — 仅用于发送消息
//! - wechat: 微信业务逻辑 (控件查找、消息发送/验证、会话管理)
//! - chatwnd: 独立聊天窗口 (借鉴 wxauto ChatWnd)
//! - input: X11 XTEST 输入注入
//! - db: 数据库监听 (SQLCipher 解密 + fanotify WAL 监听)
//! - api: HTTP/WebSocket API
//! - config: 配置文件管理
//! - console: 交互式控制台

mod atspi;
mod api;
mod chatwnd;
mod config;
mod console;
mod db;
mod input;
mod wechat;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tracing::{debug, error, info, warn};


#[tokio::main]
async fn main() -> Result<()> {
    // 日志 (with_ansi(true) 强制启用 ANSI 颜色, 即使 stderr 重定向到文件)
    tracing_subscriber::fmt()
        .with_ansi(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mimicwx=debug,tower_http=info".into()),
        )
        .init();

    info!("[init] MimicWX-Linux v{} 启动中...", env!("CARGO_PKG_VERSION"));

    // ① 加载配置文件
    let (config, config_path) = config::load_config();
    if !config.listen.auto.is_empty() {
        debug!("[table] 自动监听列表: {:?}", config.listen.auto);
    }

    // ② AT-SPI2 连接 (仍用于发送消息, 带重试)
    let atspi = loop {
        match atspi::AtSpi::connect().await {
            Ok(a) => {
                info!("[ok] AT-SPI2 连接就绪");
                break Arc::new(a);
            }
            Err(e) => {
                warn!("[warn] AT-SPI2 连接失败: {}, 5秒后重试...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };

    // ③ X11 XTEST 输入引擎 (仅发送消息需要, 非必须)
    let engine = match input::InputEngine::new() {
        Ok(e) => {
            info!("[ok] X11 XTEST 输入引擎就绪");
            Some(e)
        }
        Err(e) => {
            warn!("[warn] X11 输入引擎不可用 (发送消息功能受限): {}", e);
            None
        }
    };

    // ④ WeChat 实例化 (AT-SPI 部分, 用于发送)
    let wechat = Arc::new(wechat::WeChat::new(atspi.clone(), config.timing.at_delay_ms));

    // ⑤ 等待微信就绪
    let mut attempts = 0;
    let mut login_prompted = false;
    loop {
        let status = wechat.check_status().await;
        match status {
            wechat::WeChatStatus::LoggedIn => {
                info!("[ok] 微信已登录");
                break;
            }
            wechat::WeChatStatus::NotRunning if attempts < 30 => {
                debug!("[wait] 等待微信启动... ({}/30)", attempts + 1);
                if attempts % 5 == 4 {
                    wechat.try_reconnect().await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                attempts += 1;
            }
            wechat::WeChatStatus::WaitingForLogin => {
                if !login_prompted {
                    info!("[login] 请通过 noVNC (http://localhost:6080/vnc.html) 扫码登录微信");
                    info!("[key] GDB 密钥提取已在后台运行, 登录后将自动获取数据库密钥");
                    login_prompted = true;
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            _ => {
                break;
            }
        }
    }

    // ⑥ 读取数据库密钥 (内存扫描提取) + 初始化 DbManager
    // 优先检查持久化路径, 回退到 /tmp (兼容)
    let key_paths = ["/home/wechat/.xwechat/wechat_key.txt", "/tmp/wechat_key.txt"];
    for i in 0..10 {
        if key_paths.iter().any(|p| std::path::Path::new(p).exists()) {
            break;
        }
        if i == 0 {
            info!("[key] 等待密钥提取...");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    let key_path = key_paths.iter()
        .find(|p| std::path::Path::new(p).exists())
        .copied()
        .unwrap_or(key_paths[0]);

    let db_manager: Option<Arc<db::DbManager>> = match std::fs::read_to_string(key_path) {
        Ok(key) => {
            let key = key.trim().to_string();
            if key.len() == 96 || key.len() == 64 {
                info!("[key] 数据库密钥已获取 ({}...{}) [{}hex]", &key[..8], &key[key.len()-8..], key.len());

                // 查找数据库目录
                let db_dir = find_db_dir();
                match db_dir {
                    Some(dir) => {
                        match db::DbManager::new(key, dir) {
                            Ok(mgr) => {
                                let mgr = Arc::new(mgr);
                                // 等待微信创建消息数据库后再标记已读
                                // 首次登录时 message_N.db 可能尚未创建, 需要重试等待
                                let mark_ok = {
                                    let mut ok = false;
                                    for attempt in 0..10 {
                                        let wait = if attempt == 0 { 5 } else { 3 };
                                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                                        match mgr.mark_all_read().await {
                                            Ok(()) => { ok = true; break; }
                                            Err(e) => {
                                                if attempt < 9 {
                                                    debug!("[wait] 消息数据库尚未就绪 (第{}次), {}秒后重试: {}",
                                                        attempt + 1, 3, e);
                                                } else {
                                                    warn!("[warn] 标记已读失败 (已重试10次): {}", e);
                                                }
                                            }
                                        }
                                    }
                                    ok
                                };
                                // 联系人加载 (在消息表就绪后执行, 或独立尝试)
                                if let Err(e) = mgr.refresh_contacts().await {
                                    warn!("[warn] 联系人加载失败 (可能尚无数据): {}", e);
                                }
                                if !mark_ok {
                                    info!("ℹ️ 消息数据库将在收到首条消息时自动连接");
                                }
                                Some(mgr)
                            }
                            Err(e) => {
                                warn!("[warn] DbManager 初始化失败: {}", e);
                                None
                            }
                        }
                    }
                    None => {
                        warn!("[warn] 未找到微信数据库目录, 数据库监听不可用");
                        None
                    }
                }
            } else {
                warn!("[warn] 密钥文件格式异常 (长度: {}), 跳过", key.len());
                None
            }
        }
        Err(_) => {
            warn!("[warn] 未找到密钥文件, 数据库解密功能不可用");
            None
        }
    };

    // ⑦ 广播通道 (WebSocket)
    let (tx, _) = tokio::sync::broadcast::channel::<String>(128);

    // ⑧ InputEngine Actor + API 服务
    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<api::InputCommand>(32);

    // Spawn actor (engine 所有权转移给 actor)
    if let Some(eng) = engine {
        api::spawn_input_actor(eng, wechat.clone(), input_rx);
    } else {
        warn!("[warn] X11 输入引擎不可用, InputEngine actor 未启动");
    }

    let state = Arc::new(api::AppState {
        wechat: wechat.clone(),
        atspi: atspi.clone(),
        input_tx: input_tx.clone(),
        tx: tx.clone(),
        db: db_manager.clone(),
        api_token: config.api.token.filter(|t| !t.is_empty()),
        start_time: std::time::Instant::now(),
        config_path: config_path.clone(),
    });

    let app = api::build_router(state.clone());
    let addr = "0.0.0.0:8899";
    info!("🌐 API 服务启动: http://{addr}");
    info!("📡 WebSocket: ws://{addr}/ws");
    info!("[pin] 端点: /status, /contacts, /sessions, /messages/new, /send, /chat, /listen, /ws");
    if state.api_token.is_some() {
        info!("🔒 API 认证已启用 (Bearer Token)");
    } else {
        warn!("[warn] API 认证未启用 (config.toml [api] token 未配置)");
    }

    // 退出码: 0=正常退出, 42=重启
    let exit_code = Arc::new(AtomicI32::new(0));

    // 关闭信号 (Ctrl+C 或 /restart 触发)
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let shutdown_tx_clone = shutdown_tx.clone();

    // 保留 db_manager 引用给控制台命令使用 (db_manager 会被下面的 if let 消费)
    let console_db_ref = db_manager.clone();

    // ⑧½ AT-SPI2 健康检查心跳 (每 30s 检查连接, 连续 3 次异常自动重连)
    {
        let hb_atspi = atspi.clone();
        let mut hb_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut fail_count: u32 = 0;
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await; // 跳过首次立即触发

            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = hb_shutdown.recv() => {
                        debug!("💓 AT-SPI2 心跳停止");
                        break;
                    }
                }

                if let Some(registry) = atspi::AtSpi::registry() {
                    let count = hb_atspi.child_count(&registry).await;
                    if count > 0 {
                        if fail_count > 0 {
                            info!("💓 AT-SPI2 连接恢复 ({count} 个应用)");
                        }
                        fail_count = 0;
                    } else {
                        fail_count += 1;
                        warn!("💓 AT-SPI2 心跳异常: Registry 返回 0 个应用 (连续 {fail_count} 次)");
                        if fail_count >= 3 {
                            warn!("💓 连续 3 次异常, 尝试重连...");
                            if hb_atspi.reconnect().await {
                                fail_count = 0;
                                info!("💓 AT-SPI2 重连成功");
                            } else {
                                warn!("💓 AT-SPI2 重连失败, 30s 后再试");
                            }
                        }
                    }
                }
            }
        });
    }

    // ⑨ 后台数据库消息监听任务
    if let Some(db) = db_manager {
        let listen_tx = tx.clone();

        // ⑨-a) 联系人定时刷新 (每 5 分钟, 新好友/群不用重启就有名字)
        {
            let refresh_db = Arc::clone(&db);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
                interval.tick().await; // 跳过首次 (启动时已加载)
                loop {
                    interval.tick().await;
                    match refresh_db.refresh_contacts().await {
                        Ok(n) => debug!("[contacts] 联系人定时刷新完成: {} 条", n),
                        Err(e) => warn!("[warn] 联系人定时刷新失败: {}", e),
                    }
                }
            });
        }

        // 启动 WAL fanotify 监听 (PID 过滤, 无需防抖)
        let mut wal_rx = db.spawn_wal_watcher();

        tokio::spawn(async move {
            info!("[listen] 数据库消息监听启动 (fanotify PID 过滤)");

            loop {
                // 等待 WAL 变化通知 (fanotify 已过滤自身事件, 无需防抖)
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    wal_rx.recv(),
                ).await {
                    Ok(Ok(())) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        error!("[err] WAL 监听通道关闭");
                        break;
                    }
                    Err(_) => {
                        // 30s 超时也执行一次轮询 (fallback)
                    }
                }

                // 拉取新消息
                match db.get_new_messages().await {
                    Ok(msgs) => {
                        for m in &msgs {
                            let json = serde_json::json!({
                                "type": "db_message",
                                "chat": m.chat,
                                "chat_display": m.chat_display_name,
                                "talker": m.talker,
                                "talker_display": m.talker_display_name,
                                "content": m.content,
                                "parsed": m.parsed,
                                "msg_type": m.msg_type,
                                "create_time": m.create_time,
                                "local_id": m.local_id,
                                "is_self": m.is_self,
                                "is_at_me": m.is_at_me,
                                "at_user_list": m.at_user_list,
                            });
                            let _ = listen_tx.send(json.to_string());
                        }
                    }
                    Err(e) => {
                        tracing::debug!("📭 消息查询: {}", e);
                    }
                }
            }
        });
    } else {
        warn!("[warn] 数据库密钥不可用, 消息监听功能未启动");
    }

    // ⑩ 自动监听任务 (配置文件中的 auto listen 列表)
    if !config.listen.auto.is_empty() {
        let auto_targets = config.listen.auto.clone();
        let auto_input_tx = input_tx.clone();
        tokio::spawn(async move {
            // 等待 API 服务就绪 + 微信窗口稳定
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            info!("[table] 开始自动添加监听 ({} 个目标)...", auto_targets.len());

            for target in &auto_targets {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if auto_input_tx.send(api::InputCommand::AddListen {
                    who: target.clone(),
                    reply: reply_tx,
                }).await.is_err() {
                    warn!("[warn] InputEngine actor 已停止, 无法自动添加监听");
                    break;
                }
                match reply_rx.await {
                    Ok(Ok(true)) => info!("[ok] 自动监听已添加: {}", target),
                    Ok(Ok(false)) => warn!("[warn] 自动监听添加失败: {}", target),
                    Ok(Err(e)) => warn!("[warn] 自动监听错误: {} - {}", target, e),
                    Err(_) => warn!("[warn] actor 响应通道已关闭"),
                }
                // 每个目标间隔 3 秒, 给微信窗口时间稳定
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }

            info!("[table] 自动监听配置完成");
        });
    }

    // ⑪ 控制台命令读取器 (stdin)
    {
        let console_exit = exit_code.clone();
        let console_shutdown = shutdown_tx.clone();
        let console_wechat = wechat.clone();
        let console_tx = tx.clone();
        let console_input_tx = input_tx.clone();
        let console_config_path = config_path.clone();
        tokio::spawn(async move {
            console::console_loop(console_exit, console_shutdown, console_wechat, console_db_ref, console_tx, console_input_tx, console_config_path).await;
        });
    }

    // ⑫ 启动 HTTP 服务 (带优雅退出)
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // 打印控制台命令提示
    info!("[help] 控制台命令: /restart /stop /status /refresh /help");

    // 优雅退出: 监听 shutdown 信号 + Ctrl+C
    let mut shutdown_rx = shutdown_tx_clone.subscribe();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // 等待 shutdown 信号或 Ctrl+C
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("🛑 收到关闭信号, 停止 API 服务...");
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("🛑 收到 Ctrl+C, 停止服务...");
                }
            }
        })
        .await?;

    let code = exit_code.load(Ordering::Relaxed);
    if code == 42 {
        info!("[retry] MimicWX 准备重启...");
    } else {
        info!("👋 MimicWX 已停止");
    }
    std::process::exit(code);
}

/// 查找微信数据库目录
///
/// WeChat Linux 数据库路径 (实际):
/// ~/Documents/xwechat_files/wxid_xxx/db_storage
/// 当存在多个 wxid 时 (换账号), 选择最近修改的目录
fn find_db_dir() -> Option<PathBuf> {
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    // 收集所有可能的 xwechat_files 路径 (用 HashSet 去重)
    let mut search_dirs = std::collections::HashSet::new();
    search_dirs.insert(PathBuf::from("/home/wechat/Documents/xwechat_files"));
    search_dirs.insert(dirs_or_home().join("Documents/xwechat_files"));
    // Fallback: 扫描 /home 下所有用户
    if let Ok(homes) = std::fs::read_dir("/home") {
        for h in homes.flatten() {
            search_dirs.insert(h.path().join("Documents/xwechat_files"));
        }
    }

    for xwechat_dir in &search_dirs {
        if let Ok(entries) = std::fs::read_dir(xwechat_dir) {
            for entry in entries.flatten() {
                let db_storage = entry.path().join("db_storage");
                if db_storage.exists() {
                    let msg_dir = db_storage.join("message");
                    let mtime = msg_dir.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    debug!("📂 候选: {} (mtime={:?})", db_storage.display(), mtime);
                    candidates.push((db_storage, mtime));
                }
            }
        }
    }

    // 选择最新修改的目录 (活跃账号)
    if !candidates.is_empty() {
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        let chosen = &candidates[0].0;
        if candidates.len() > 1 {
            info!("📂 发现 {} 个账号目录, 选择最新的: {}", candidates.len(), chosen.display());
        } else {
            info!("📂 数据库目录: {}", chosen.display());
        }
        return Some(chosen.clone());
    }

    // 也尝试旧路径格式
    let old_path = PathBuf::from("/home/wechat/.local/share/weixin/data/db_storage");
    if old_path.exists() {
        info!("📂 数据库目录 (旧格式): {}", old_path.display());
        return Some(old_path);
    }

    None
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

