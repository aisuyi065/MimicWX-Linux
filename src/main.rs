//! MimicWX-Linux: 微信自动化框架
//!
//! 架构:
//! - atspi: AT-SPI2 底层原语 (D-Bus 通信) — 仅用于发送消息
//! - wechat: 微信业务逻辑 (控件查找、消息发送/验证、会话管理)
//! - chatwnd: 独立聊天窗口 (借鉴 wxauto ChatWnd)
//! - input: X11 XTEST 输入注入
//! - db: 数据库监听 (SQLCipher 解密 + fanotify WAL 监听)
//! - api: HTTP/WebSocket API

mod atspi;
mod api;
mod chatwnd;
mod db;
mod input;
mod wechat;

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tracing::{debug, error, info, warn};

// =====================================================================
// 配置文件
// =====================================================================

#[derive(Debug, Deserialize, Default)]
struct AppConfig {
    #[serde(default)]
    api: ApiConfig,
    #[serde(default)]
    listen: ListenConfig,
}

#[derive(Debug, Deserialize, Default)]
struct ApiConfig {
    /// API 认证 Token (留空或不配置则不启用认证)
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ListenConfig {
    /// 启动后自动弹出独立窗口并监听的对象
    #[serde(default)]
    auto: Vec<String>,
}

/// 加载配置文件 (搜索多个路径)
fn load_config() -> AppConfig {
    let search_paths = [
        PathBuf::from("./config.toml"),
        PathBuf::from("/home/wechat/mimicwx-linux/config.toml"),
        PathBuf::from("/etc/mimicwx/config.toml"),
    ];
    for path in &search_paths {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => match toml::from_str::<AppConfig>(&content) {
                    Ok(config) => {
                        info!("⚙️ 配置文件已加载: {}", path.display());
                        return config;
                    }
                    Err(e) => {
                        warn!("⚠️ 配置文件解析失败: {} - {}", path.display(), e);
                    }
                },
                Err(e) => {
                    warn!("⚠️ 配置文件读取失败: {} - {}", path.display(), e);
                }
            }
        }
    }
    info!("⚙️ 未找到配置文件, 使用默认配置");
    AppConfig::default()
}

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

    info!("🚀 MimicWX-Linux v{} 启动中...", env!("CARGO_PKG_VERSION"));

    // ① 加载配置文件
    let config = load_config();
    if !config.listen.auto.is_empty() {
        info!("📋 自动监听列表: {:?}", config.listen.auto);
    }

    // ② AT-SPI2 连接 (仍用于发送消息, 带重试)
    let atspi = loop {
        match atspi::AtSpi::connect().await {
            Ok(a) => {
                info!("✅ AT-SPI2 连接就绪");
                break Arc::new(a);
            }
            Err(e) => {
                warn!("⚠️ AT-SPI2 连接失败: {}, 5秒后重试...", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };

    // ③ X11 XTEST 输入引擎 (仅发送消息需要, 非必须)
    let engine = match input::InputEngine::new() {
        Ok(e) => {
            info!("✅ X11 XTEST 输入引擎就绪");
            Some(e)
        }
        Err(e) => {
            warn!("⚠️ X11 输入引擎不可用 (发送消息功能受限): {}", e);
            None
        }
    };

    // ④ WeChat 实例化 (AT-SPI 部分, 用于发送)
    let wechat = Arc::new(wechat::WeChat::new(atspi.clone()));

    // ⑤ 等待微信就绪
    let mut attempts = 0;
    let mut login_prompted = false;
    loop {
        let status = wechat.check_status().await;
        match status {
            wechat::WeChatStatus::LoggedIn => {
                info!("✅ 微信已登录");
                break;
            }
            wechat::WeChatStatus::NotRunning if attempts < 30 => {
                info!("⏳ 等待微信启动... ({}/30)", attempts + 1);
                if attempts % 5 == 4 {
                    wechat.try_reconnect().await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                attempts += 1;
            }
            wechat::WeChatStatus::WaitingForLogin => {
                if !login_prompted {
                    info!("📱 请通过 noVNC (http://localhost:6080/vnc.html) 扫码登录微信");
                    info!("🔑 GDB 密钥提取已在后台运行, 登录后将自动获取数据库密钥");
                    login_prompted = true;
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            _ => {
                break;
            }
        }
    }

    // ⑥ 读取 GDB 提取的数据库密钥 + 初始化 DbManager
    let key_path = "/tmp/wechat_key.txt";
    for i in 0..10 {
        if std::path::Path::new(key_path).exists() {
            break;
        }
        if i == 0 {
            info!("🔑 等待 GDB 提取密钥...");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    let db_manager: Option<Arc<db::DbManager>> = match std::fs::read_to_string(key_path) {
        Ok(key) => {
            let key = key.trim().to_string();
            if key.len() == 64 {
                info!("🔑 数据库密钥已获取 ({}...{})", &key[..8], &key[56..]);

                // 查找数据库目录
                let db_dir = find_db_dir();
                match db_dir {
                    Some(dir) => {
                        match db::DbManager::new(key, dir) {
                            Ok(mgr) => {
                                let mgr = Arc::new(mgr);
                                // 等待微信同步数据库后再加载联系人 (刚登录时表不完整)
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                if let Err(e) = mgr.refresh_contacts().await {
                                    warn!("⚠️ 联系人加载失败 (可能尚无数据): {}", e);
                                }
                                // 标记已有消息为已读
                                if let Err(e) = mgr.mark_all_read().await {
                                    warn!("⚠️ 标记已读失败: {}", e);
                                }
                                Some(mgr)
                            }
                            Err(e) => {
                                warn!("⚠️ DbManager 初始化失败: {}", e);
                                None
                            }
                        }
                    }
                    None => {
                        warn!("⚠️ 未找到微信数据库目录, 数据库监听不可用");
                        None
                    }
                }
            } else {
                warn!("⚠️ 密钥文件格式异常 (长度: {}), 跳过", key.len());
                None
            }
        }
        Err(_) => {
            warn!("⚠️ 未找到密钥文件, 数据库解密功能不可用");
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
        warn!("⚠️ X11 输入引擎不可用, InputEngine actor 未启动");
    }

    let state = Arc::new(api::AppState {
        wechat: wechat.clone(),
        atspi: atspi.clone(),
        input_tx: input_tx.clone(),
        tx: tx.clone(),
        db: db_manager.clone(),
        api_token: config.api.token.filter(|t| !t.is_empty()),
        start_time: std::time::Instant::now(),
    });

    let app = api::build_router(state.clone());
    let addr = "0.0.0.0:8899";
    info!("🌐 API 服务启动: http://{addr}");
    info!("📡 WebSocket: ws://{addr}/ws");
    info!("📌 端点: /status, /contacts, /sessions, /messages/new, /send, /chat, /listen, /ws");
    if state.api_token.is_some() {
        info!("🔒 API 认证已启用 (Bearer Token)");
    } else {
        warn!("⚠️ API 认证未启用 (config.toml [api] token 未配置)");
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
                        Ok(n) => info!("👥 联系人定时刷新完成: {} 条", n),
                        Err(e) => warn!("⚠️ 联系人定时刷新失败: {}", e),
                    }
                }
            });
        }

        // 启动 WAL fanotify 监听 (PID 过滤, 无需防抖)
        let mut wal_rx = db.spawn_wal_watcher();

        tokio::spawn(async move {
            info!("👂 数据库消息监听启动 (fanotify PID 过滤)");

            loop {
                // 等待 WAL 变化通知 (fanotify 已过滤自身事件, 无需防抖)
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    wal_rx.recv(),
                ).await {
                    Ok(Ok(())) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        error!("❌ WAL 监听通道关闭");
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
        // Fallback: AT-SPI 轮询 (无数据库密钥时)
        let listen_wechat = wechat.clone();
        let listen_tx = tx.clone();
        tokio::spawn(async move {
            info!("👂 后台监听 (AT-SPI fallback 模式)");
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                interval.tick().await;
                let msgs = listen_wechat.get_listen_messages().await;
                for (who, new_msgs) in &msgs {
                    for m in new_msgs {
                        let json = serde_json::json!({
                            "type": "listen_message",
                            "from": who,
                            "msg_type": m.msg_type,
                            "sender": m.sender,
                            "content": m.content,
                        });
                        let _ = listen_tx.send(json.to_string());
                    }
                }
            }
        });
    }

    // ⑩ 自动监听任务 (配置文件中的 auto listen 列表)
    if !config.listen.auto.is_empty() {
        let auto_targets = config.listen.auto.clone();
        let auto_input_tx = input_tx.clone();
        tokio::spawn(async move {
            // 等待 API 服务就绪 + 微信窗口稳定
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            info!("📋 开始自动添加监听 ({} 个目标)...", auto_targets.len());

            for target in &auto_targets {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if auto_input_tx.send(api::InputCommand::AddListen {
                    who: target.clone(),
                    reply: reply_tx,
                }).await.is_err() {
                    warn!("⚠️ InputEngine actor 已停止, 无法自动添加监听");
                    break;
                }
                match reply_rx.await {
                    Ok(Ok(true)) => info!("✅ 自动监听已添加: {}", target),
                    Ok(Ok(false)) => warn!("⚠️ 自动监听添加失败: {}", target),
                    Ok(Err(e)) => warn!("⚠️ 自动监听错误: {} - {}", target, e),
                    Err(_) => warn!("⚠️ actor 响应通道已关闭"),
                }
                // 每个目标间隔 3 秒, 给微信窗口时间稳定
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }

            info!("📋 自动监听配置完成");
        });
    }

    // ⑪ 控制台命令读取器 (stdin)
    {
        let console_exit = exit_code.clone();
        let console_shutdown = shutdown_tx.clone();
        let console_wechat = wechat.clone();
        tokio::spawn(async move {
            console_loop(console_exit, console_shutdown, console_wechat, console_db_ref).await;
        });
    }

    // ⑫ 启动 HTTP 服务 (带优雅退出)
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // 打印控制台命令提示
    info!("💡 控制台命令: /restart /stop /status /refresh /help");

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
        info!("🔄 MimicWX 准备重启...");
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

// =====================================================================
// 交互式控制台 (raw terminal mode + 行编辑 + 历史命令)
// =====================================================================

/// Raw mode guard — Drop 时自动恢复终端
struct RawModeGuard(libc::termios);

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.0); }
        let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\n");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

/// 启用 raw input mode (关闭行缓冲+回显, 保留输出处理和信号)
fn enable_raw_mode() -> Option<RawModeGuard> {
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 { return None; }
        let mut raw = orig;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) != 0 { return None; }
        Some(RawModeGuard(orig))
    }
}

/// 重绘提示行
fn redraw_prompt(line: &str, cursor: usize) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r\x1b[K> {}", line);
    let move_back = line[cursor..].chars().count();
    if move_back > 0 { let _ = write!(out, "\x1b[{}D", move_back); }
    let _ = out.flush();
}

/// 处理控制台命令, 返回 true = 应退出
async fn handle_command(
    cmd: &str, exit_code: &Arc<AtomicI32>,
    shutdown_tx: &tokio::sync::broadcast::Sender<()>,
    wechat: &Arc<wechat::WeChat>, db: &Option<Arc<db::DbManager>>,
) -> bool {
    match cmd {
        "/restart" => {
            info!("🔄 收到 /restart 命令, 准备重启...");
            exit_code.store(42, Ordering::Relaxed);
            let _ = shutdown_tx.send(()); true
        }
        "/stop" => {
            info!("🛑 收到 /stop 命令, 正常关闭...");
            exit_code.store(0, Ordering::Relaxed);
            let _ = shutdown_tx.send(()); true
        }
        "/status" => {
            let status = wechat.check_status().await;
            let listen_list = wechat.get_listen_list().await;
            let db_status = if db.is_some() { "可用" } else { "不可用" };
            let contacts = if let Some(ref d) = db { d.get_contacts().await.len() } else { 0 };
            info!("📊 === 运行时状态 ===");
            info!("📊 微信状态: {}", status);
            info!("📊 数据库: {} | 联系人: {} 条", db_status, contacts);
            info!("📊 监听窗口: {} 个 {:?}", listen_list.len(), listen_list);
            info!("📊 版本: v{}", env!("CARGO_PKG_VERSION"));
            info!("📊 =================="); false
        }
        "/refresh" => {
            if let Some(ref d) = db {
                info!("👥 手动刷新联系人...");
                match d.refresh_contacts().await {
                    Ok(n) => info!("👥 刷新完成: {} 条", n),
                    Err(e) => warn!("⚠️ 刷新失败: {}", e),
                }
            } else { info!("⚠️ 数据库不可用"); }
            false
        }
        "/help" => {
            info!("💡 === 可用命令 ===");
            info!("💡 /restart  — 优雅重启    /stop — 关闭程序");
            info!("💡 /status   — 运行状态    /refresh — 刷新联系人");
            info!("💡 /help     — 显示帮助");
            info!("💡 快捷键: ↑↓历史 ←→光标 Ctrl+U清行 Ctrl+L清屏");
            info!("💡 =================="); false
        }
        _ => { info!("❓ 未知命令: {} (/help 查看帮助)", cmd); false }
    }
}

/// 交互式控制台主循环 (raw mode)
async fn console_loop(
    exit_code: Arc<AtomicI32>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
    wechat: Arc<wechat::WeChat>,
    db: Option<Arc<db::DbManager>>,
) {
    let _guard = match enable_raw_mode() {
        Some(g) => g,
        None => {
            debug!("📥 非 TTY, 降级为简单模式");
            console_loop_simple(exit_code, shutdown_tx, wechat, db).await;
            return;
        }
    };

    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut line = String::new();
    let mut cursor: usize = 0;
    let mut history: Vec<String> = Vec::new();
    let mut hist_idx: usize = 0;

    redraw_prompt(&line, cursor);

    let mut buf = [0u8; 128];
    loop {
        let n = match stdin.read(&mut buf).await {
            Ok(0) => break, Ok(n) => n, Err(_) => break,
        };

        let bytes = &buf[..n];
        let mut i = 0;
        let mut redraw = false;
        let mut exec = false;

        while i < bytes.len() {
            match bytes[i] {
                b'\r' | b'\n' => { exec = true; i += 1; break; }
                0x7f | 0x08 => { // Backspace
                    if cursor > 0 {
                        let prev = line[..cursor].char_indices().last().map(|(p,_)|p).unwrap_or(0);
                        line.drain(prev..cursor); cursor = prev; redraw = true;
                    }
                    i += 1;
                }
                0x1b if i+2 < bytes.len() && bytes[i+1] == b'[' => match bytes[i+2] {
                    b'A' => { // ↑ 历史
                        if !history.is_empty() && hist_idx > 0 {
                            hist_idx -= 1; line = history[hist_idx].clone();
                            cursor = line.len(); redraw = true;
                        }
                        i += 3;
                    }
                    b'B' => { // ↓ 历史
                        if hist_idx < history.len() {
                            hist_idx += 1;
                            line = if hist_idx < history.len() { history[hist_idx].clone() } else { String::new() };
                            cursor = line.len(); redraw = true;
                        }
                        i += 3;
                    }
                    b'C' => { // →
                        if cursor < line.len() {
                            cursor = line[cursor..].char_indices().nth(1).map(|(ci,_)|cursor+ci).unwrap_or(line.len());
                            redraw = true;
                        }
                        i += 3;
                    }
                    b'D' => { // ←
                        if cursor > 0 {
                            cursor = line[..cursor].char_indices().last().map(|(p,_)|p).unwrap_or(0);
                            redraw = true;
                        }
                        i += 3;
                    }
                    b'H' => { cursor = 0; redraw = true; i += 3; }
                    b'F' => { cursor = line.len(); redraw = true; i += 3; }
                    b'3' if i+3 < bytes.len() && bytes[i+3] == b'~' => { // Delete
                        if cursor < line.len() {
                            let next = line[cursor..].char_indices().nth(1).map(|(ci,_)|cursor+ci).unwrap_or(line.len());
                            line.drain(cursor..next); redraw = true;
                        }
                        i += 4;
                    }
                    _ => { i += 3; }
                }
                0x01 => { cursor = 0; redraw = true; i += 1; }                   // Ctrl+A
                0x05 => { cursor = line.len(); redraw = true; i += 1; }           // Ctrl+E
                0x15 => { line.clear(); cursor = 0; redraw = true; i += 1; }      // Ctrl+U
                0x0c => { // Ctrl+L
                    let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[2J\x1b[H");
                    redraw = true; i += 1;
                }
                b if b >= 0x20 && b < 0x7f => { // ASCII
                    line.insert(cursor, b as char); cursor += 1; redraw = true; i += 1;
                }
                b if b >= 0x80 => { // UTF-8
                    let clen = if b < 0xE0 { 2 } else if b < 0xF0 { 3 } else { 4 };
                    if i + clen <= bytes.len() {
                        if let Ok(s) = std::str::from_utf8(&bytes[i..i+clen]) {
                            line.insert_str(cursor, s); cursor += s.len(); redraw = true;
                        }
                    }
                    i += clen;
                }
                _ => { i += 1; }
            }
        }

        if redraw && !exec { redraw_prompt(&line, cursor); }

        if exec {
            let cmd = line.trim().to_string();
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\n");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            if !cmd.is_empty() {
                if history.last().map(|h| h != &cmd).unwrap_or(true) { history.push(cmd.clone()); }
                if handle_command(&cmd, &exit_code, &shutdown_tx, &wechat, &db).await { return; }
            }
            line.clear(); cursor = 0; hist_idx = history.len();
            redraw_prompt(&line, cursor);
        }
    }
}

/// 简单控制台 (非 TTY 降级模式)
async fn console_loop_simple(
    exit_code: Arc<AtomicI32>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
    wechat: Arc<wechat::WeChat>,
    db: Option<Arc<db::DbManager>>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let cmd = line.trim().to_string();
                if !cmd.is_empty() {
                    if handle_command(&cmd, &exit_code, &shutdown_tx, &wechat, &db).await { break; }
                }
            }
            Err(_) => break,
        }
    }
}



