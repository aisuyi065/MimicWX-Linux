//! 交互式控制台 (raw terminal mode + 行编辑 + 历史命令)
//!
//! 功能:
//! - Raw mode 字符级输入 (关闭行缓冲和回显)
//! - 方向键历史命令 (↑↓) 和光标移动 (←→)
//! - Home/End/Delete/Backspace/Ctrl+U/Ctrl+L 快捷键
//! - UTF-8 中文输入支持
//! - 非 TTY 环境自动降级为行模式

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tracing::{debug, info, warn};

use crate::api;
use crate::config::{self, AppConfig};
use crate::db;
use crate::wechat;

// =====================================================================
// Raw Mode
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

// =====================================================================
// 命令处理
// =====================================================================

async fn handle_command(
    cmd: &str, exit_code: &Arc<AtomicI32>,
    shutdown_tx: &tokio::sync::broadcast::Sender<()>,
    wechat: &Arc<wechat::WeChat>, db: &Option<Arc<db::DbManager>>,
    broadcast_tx: &tokio::sync::broadcast::Sender<String>,
    input_tx: &tokio::sync::mpsc::Sender<api::InputCommand>,
    config_path: &Option<PathBuf>,
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
        "/atmode" => {
            let msg = serde_json::json!({
                "type": "control",
                "cmd": "toggle_at_mode",
            });
            let _ = broadcast_tx.send(msg.to_string());
            info!("📢 已发送仅@模式切换指令");
            false
        }
        "/reload" => {
            if let Some(ref path) = config_path {
                match std::fs::read_to_string(path) {
                    Ok(content) => match toml::from_str::<AppConfig>(&content) {
                        Ok(new_config) => {
                            // 1. 更新 at_delay_ms
                            let old_delay = wechat.get_at_delay_ms();
                            let new_delay = new_config.timing.at_delay_ms;
                            if old_delay != new_delay {
                                wechat.set_at_delay_ms(new_delay);
                                info!("⚙️ at_delay_ms: {old_delay} → {new_delay}");
                            }
                            // 2. Diff listen 列表
                            let current_list = wechat.get_listen_list().await;
                            let new_list = new_config.listen.auto;
                            // 新增的
                            let to_add: Vec<_> = new_list.iter()
                                .filter(|n| !current_list.contains(n))
                                .cloned().collect();
                            // 移除的
                            let to_remove: Vec<_> = current_list.iter()
                                .filter(|n| !new_list.contains(n))
                                .cloned().collect();
                            if to_add.is_empty() && to_remove.is_empty() {
                                info!("⚙️ 监听列表无变化");
                            } else {
                                for who in &to_remove {
                                    info!("👂 /reload 移除监听: {who}");
                                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                    if input_tx.send(api::InputCommand::RemoveListen {
                                        who: who.clone(), reply: reply_tx,
                                    }).await.is_ok() {
                                        let _ = reply_rx.await;
                                    }
                                }
                                for who in &to_add {
                                    info!("👂 /reload 添加监听: {who}");
                                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                    if input_tx.send(api::InputCommand::AddListen {
                                        who: who.clone(), reply: reply_tx,
                                    }).await.is_ok() {
                                        match reply_rx.await {
                                            Ok(Ok(true)) => info!("✅ 监听已添加: {who}"),
                                            _ => warn!("⚠️ 添加监听失败: {who}"),
                                        }
                                    }
                                    // 每个目标间隔 3 秒
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                }
                            }
                            info!("⚙️ 配置已重新加载");
                        }
                        Err(e) => warn!("⚠️ 配置解析失败: {e}"),
                    },
                    Err(e) => warn!("⚠️ 读取配置失败: {e}"),
                }
            } else {
                info!("⚠️ 未找到配置文件路径, 无法重载");
            }
            false
        }
        "/sessions" => {
            if let Some(ref d) = db {
                match d.get_sessions().await {
                    Ok(sessions) => {
                        info!("💬 === 会话列表 ({} 个) ===", sessions.len());
                        for s in &sessions {
                            let unread = if s.unread_count > 0 {
                                format!(" [未读:{}]", s.unread_count)
                            } else { String::new() };
                            info!("💬  {} ({}){}", s.display_name, s.username, unread);
                        }
                        info!("💬 ==================");
                    }
                    Err(e) => warn!("⚠️ 获取会话失败: {}", e),
                }
            } else { info!("⚠️ 数据库不可用"); }
            false
        }
        _ if cmd.starts_with("/send ") => {
            let rest = cmd.strip_prefix("/send ").unwrap().trim();
            if let Some((to, text)) = rest.split_once(' ') {
                let to = to.trim();
                let text = text.trim();
                if to.is_empty() || text.is_empty() {
                    info!("❌ 用法: /send <收件人> <内容>");
                } else {
                    info!("📤 发送消息: [{to}] → {text}");
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    let has_db = db.is_some();
                    if input_tx.send(api::InputCommand::SendMessage {
                        to: to.to_string(), text: text.to_string(),
                        at: vec![], skip_verify: has_db,
                        reply: reply_tx,
                    }).await.is_ok() {
                        match reply_rx.await {
                            Ok(Ok((true, _, msg))) => info!("✅ {msg}"),
                            Ok(Ok((false, _, msg))) => warn!("⚠️ {msg}"),
                            Ok(Err(e)) => warn!("⚠️ 发送失败: {e}"),
                            Err(_) => warn!("⚠️ actor 响应通道已关闭"),
                        }
                    } else { warn!("⚠️ InputEngine actor 已停止"); }
                }
            } else {
                info!("❌ 用法: /send <收件人> <内容>");
            }
            false
        }
        _ if cmd.starts_with("/listen ") => {
            let who = cmd.strip_prefix("/listen ").unwrap().trim();
            if who.is_empty() {
                info!("❌ 用法: /listen <联系人/群名>");
            } else {
                info!("👂 添加监听: {who}");
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if input_tx.send(api::InputCommand::AddListen {
                    who: who.to_string(), reply: reply_tx,
                }).await.is_ok() {
                    match reply_rx.await {
                        Ok(Ok(true)) => {
                            info!("✅ 监听已添加: {who}");
                            // 持久化: 写入 config.toml
                            if let Some(ref path) = config_path {
                                let mut list = wechat.get_listen_list().await;
                                if !list.contains(&who.to_string()) {
                                    list.push(who.to_string());
                                }
                                config::save_listen_list(path, &list);
                            }
                        }
                        Ok(Ok(false)) => warn!("⚠️ 添加监听失败: {who}"),
                        Ok(Err(e)) => warn!("⚠️ 添加监听错误: {e}"),
                        Err(_) => warn!("⚠️ actor 响应通道已关闭"),
                    }
                } else { warn!("⚠️ InputEngine actor 已停止"); }
            }
            false
        }
        _ if cmd.starts_with("/unlisten ") => {
            let who = cmd.strip_prefix("/unlisten ").unwrap().trim();
            if who.is_empty() {
                info!("❌ 用法: /unlisten <联系人/群名>");
            } else {
                info!("👂 移除监听: {who}");
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if input_tx.send(api::InputCommand::RemoveListen {
                    who: who.to_string(), reply: reply_tx,
                }).await.is_ok() {
                    match reply_rx.await {
                        Ok(true) => {
                            info!("✅ 监听已移除: {who}");
                            // 持久化: 写入 config.toml
                            if let Some(ref path) = config_path {
                                let mut list = wechat.get_listen_list().await;
                                list.retain(|n| n != who);
                                config::save_listen_list(path, &list);
                            }
                        }
                        Ok(false) => info!("⚠️ 未找到监听: {who}"),
                        Err(_) => warn!("⚠️ actor 响应通道已关闭"),
                    }
                } else { warn!("⚠️ InputEngine actor 已停止"); }
            }
            false
        }
        "/help" => {
            info!("💡 === 可用命令 ===");
            info!("💡 /restart  — 优雅重启    /stop — 关闭程序");
            info!("💡 /status   — 运行状态    /refresh — 刷新联系人");
            info!("💡 /atmode   — 切换仅@模式  /sessions — 查看会话列表");
            info!("💡 /reload   — 热重载配置    /help — 显示帮助");
            info!("💡 /send <收件人> <内容> — 发送消息");
            info!("💡 /listen <名称>       — 添加监听");
            info!("💡 /unlisten <名称>     — 移除监听");
            info!("💡 快捷键: ↑↓历史 ←→光标 Ctrl+U清行 Ctrl+L清屏");
            info!("💡 =================="); false
        }
        _ => { info!("❓ 未知命令: {} (/help 查看帮助)", cmd); false }
    }
}

// =====================================================================
// 控制台主循环
// =====================================================================

/// 交互式控制台主循环 (raw mode)
pub async fn console_loop(
    exit_code: Arc<AtomicI32>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
    wechat: Arc<wechat::WeChat>,
    db: Option<Arc<db::DbManager>>,
    broadcast_tx: tokio::sync::broadcast::Sender<String>,
    input_tx: tokio::sync::mpsc::Sender<api::InputCommand>,
    config_path: Option<PathBuf>,
) {
    let _guard = match enable_raw_mode() {
        Some(g) => g,
        None => {
            debug!("📥 非 TTY, 降级为简单模式");
            console_loop_simple(exit_code, shutdown_tx, wechat, db, broadcast_tx, input_tx, config_path).await;
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
                if handle_command(&cmd, &exit_code, &shutdown_tx, &wechat, &db, &broadcast_tx, &input_tx, &config_path).await { return; }
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
    broadcast_tx: tokio::sync::broadcast::Sender<String>,
    input_tx: tokio::sync::mpsc::Sender<api::InputCommand>,
    config_path: Option<PathBuf>,
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
                    if handle_command(&cmd, &exit_code, &shutdown_tx, &wechat, &db, &broadcast_tx, &input_tx, &config_path).await { break; }
                }
            }
            Err(_) => break,
        }
    }
}
