//! X11 XTEST 输入引擎
//!
//! 通过 x11rb 使用 X11 XTEST 扩展注入键盘和鼠标事件。
//! 中文输入通过 X11 Selection（剪贴板）+ Ctrl+V 实现。图片通过 xclip + Ctrl+V。

use anyhow::{Context, Result};
use tracing::{debug, info};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Keycode, AtomEnum, ClientMessageEvent, EventMask};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// X11 事件类型
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

/// 延迟常量 (ms)
const KEY_HOLD_MS: u64 = 30;
const TYPING_DELAY_MS: u64 = 20;
const CLICK_HOLD_MS: u64 = 50;

/// X11 Keysym 常量
mod keysym {
    pub const XK_SPACE: u32 = 0x0020;
    pub const XK_RETURN: u32 = 0xFF0D;
    pub const XK_ESCAPE: u32 = 0xFF1B;
    pub const XK_TAB: u32 = 0xFF09;
    pub const XK_BACKSPACE: u32 = 0xFF08;
    pub const XK_DELETE: u32 = 0xFFFF;
    pub const XK_HOME: u32 = 0xFF50;
    pub const XK_END: u32 = 0xFF57;
    pub const XK_LEFT: u32 = 0xFF51;
    pub const XK_UP: u32 = 0xFF52;
    pub const XK_RIGHT: u32 = 0xFF53;
    pub const XK_DOWN: u32 = 0xFF54;
    pub const XK_SHIFT_L: u32 = 0xFFE1;
    pub const XK_CONTROL_L: u32 = 0xFFE3;
    pub const XK_ALT_L: u32 = 0xFFE4;
    pub const XK_F1: u32 = 0xFFBE;
    pub const XK_F2: u32 = 0xFFBF;
    pub const XK_F3: u32 = 0xFFC0;
    pub const XK_F4: u32 = 0xFFC1;
    pub const XK_F5: u32 = 0xFFC2;
}

/// X11 XTEST 输入引擎
pub struct InputEngine {
    conn: RustConnection,
    screen_root: u32,
    min_keycode: Keycode,
    max_keycode: Keycode,
    keysyms_per_keycode: u8,
    keysyms: Vec<u32>,
    // 缓存的 X11 Atom (在 X11 Session 内永不变, 启动时一次性 intern)
    atom_net_wm_name: u32,
    atom_utf8_string: u32,
    atom_net_client_list: u32,
    atom_net_active_window: u32,
    atom_net_close_window: u32,
    atom_clipboard: u32,
    atom_targets: u32,
}

impl InputEngine {
    /// 创建输入引擎
    pub fn new() -> Result<Self> {
        info!("🎮 初始化 X11 XTEST 输入引擎...");

        let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());
        let (conn, screen_num) = RustConnection::connect(Some(&display_env))
            .context(format!("连接 X11 失败 (DISPLAY={display_env})"))?;

        let screen = &conn.setup().roots[screen_num];
        let screen_root = screen.root;

        // 验证 XTEST 扩展
        x11rb::protocol::xtest::get_version(&conn, 2, 2)
            .context("XTEST 扩展不可用")?
            .reply()
            .context("XTEST 版本查询失败")?;

        // 获取键盘映射
        let setup = conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let reply = conn.get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)?
            .reply()
            .context("获取键盘映射失败")?;

        let keysyms_per_keycode = reply.keysyms_per_keycode;
        let keysyms: Vec<u32> = reply.keysyms.iter().map(|k| (*k).into()).collect();

        // 一次性 intern 所有需要的 Atom (避免每次调用重复查询)
        let atom_net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let atom_utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let atom_net_client_list = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
        let atom_net_active_window = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")?.reply()?.atom;
        let atom_net_close_window = conn.intern_atom(false, b"_NET_CLOSE_WINDOW")?.reply()?.atom;
        let atom_clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        let atom_targets = conn.intern_atom(false, b"TARGETS")?.reply()?.atom;

        info!("✅ X11 XTEST 就绪 (DISPLAY={display_env}, keycodes={min_keycode}~{max_keycode})");

        Ok(Self {
            conn, screen_root, min_keycode, max_keycode, keysyms_per_keycode, keysyms,
            atom_net_wm_name, atom_utf8_string, atom_net_client_list,
            atom_net_active_window, atom_net_close_window,
            atom_clipboard, atom_targets,
        })
    }

    // =================================================================
    // Keysym 查找
    // =================================================================

    fn keysym_to_keycode(&self, keysym: u32) -> Option<(Keycode, bool)> {
        let per = self.keysyms_per_keycode as usize;
        let total = (self.max_keycode - self.min_keycode + 1) as usize;

        for i in 0..total {
            for j in 0..per {
                if self.keysyms[i * per + j] == keysym {
                    let keycode = self.min_keycode + i as u8;
                    let need_shift = j == 1;
                    return Some((keycode, need_shift));
                }
            }
        }
        None
    }

    fn char_to_keysym(ch: char) -> Option<u32> {
        match ch {
            ' ' => Some(keysym::XK_SPACE),
            '\n' => Some(keysym::XK_RETURN),
            '\t' => Some(keysym::XK_TAB),
            c if c.is_ascii() => Some(c as u32),
            _ => None,
        }
    }

    fn key_name_to_keysym(name: &str) -> Option<u32> {
        match name.to_lowercase().as_str() {
            "return" | "enter" => Some(keysym::XK_RETURN),
            "escape" | "esc" => Some(keysym::XK_ESCAPE),
            "tab" => Some(keysym::XK_TAB),
            "backspace" => Some(keysym::XK_BACKSPACE),
            "delete" => Some(keysym::XK_DELETE),
            "space" => Some(keysym::XK_SPACE),
            "home" => Some(keysym::XK_HOME),
            "end" => Some(keysym::XK_END),
            "left" => Some(keysym::XK_LEFT),
            "right" => Some(keysym::XK_RIGHT),
            "up" => Some(keysym::XK_UP),
            "down" => Some(keysym::XK_DOWN),
            "shift" => Some(keysym::XK_SHIFT_L),
            "ctrl" | "control" => Some(keysym::XK_CONTROL_L),
            "alt" => Some(keysym::XK_ALT_L),
            "f1" => Some(keysym::XK_F1),
            "f2" => Some(keysym::XK_F2),
            "f3" => Some(keysym::XK_F3),
            "f4" => Some(keysym::XK_F4),
            "f5" => Some(keysym::XK_F5),
            s if s.len() == 1 => Self::char_to_keysym(s.chars().next()?),
            _ => None,
        }
    }

    // =================================================================
    // 底层 XTEST 操作
    // =================================================================

    fn raw_key_press(&self, keycode: Keycode) -> Result<()> {
        self.conn.xtest_fake_input(KEY_PRESS, keycode, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn raw_key_release(&self, keycode: Keycode) -> Result<()> {
        self.conn.xtest_fake_input(KEY_RELEASE, keycode, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    // =================================================================
    // 键盘操作
    // =================================================================

    /// 模拟单次按键
    pub async fn press_key(&mut self, key_name: &str) -> Result<()> {
        let ks = Self::key_name_to_keysym(key_name)
            .ok_or_else(|| anyhow::anyhow!("未知按键: {key_name}"))?;
        let (keycode, need_shift) = self.keysym_to_keycode(ks)
            .ok_or_else(|| anyhow::anyhow!("按键无映射: {key_name}"))?;

        // Shift
        let shift_kc = if need_shift {
            self.keysym_to_keycode(keysym::XK_SHIFT_L).map(|(kc, _)| kc)
        } else { None };
        if let Some(skc) = shift_kc { self.raw_key_press(skc)?; }

        self.raw_key_press(keycode)?;
        tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
        self.raw_key_release(keycode)?;

        if let Some(skc) = shift_kc { self.raw_key_release(skc)?; }

        debug!("⌨️ press_key: {key_name}");
        Ok(())
    }

    /// 组合键 (如 "ctrl+f", "ctrl+v", "ctrl+a")
    pub async fn key_combo(&mut self, combo: &str) -> Result<()> {
        let parts: Vec<&str> = combo.split('+').collect();
        let mut keycodes = Vec::new();

        for part in &parts {
            let ks = Self::key_name_to_keysym(part.trim())
                .ok_or_else(|| anyhow::anyhow!("未知按键: {part}"))?;
            let (kc, _) = self.keysym_to_keycode(ks)
                .ok_or_else(|| anyhow::anyhow!("按键无映射: {part}"))?;
            keycodes.push(kc);
        }

        // 按顺序按下
        for &kc in &keycodes {
            self.raw_key_press(kc)?;
            tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
        }
        // 逆序释放
        for &kc in keycodes.iter().rev() {
            self.raw_key_release(kc)?;
        }

        debug!("⌨️ key_combo: {combo}");
        Ok(())
    }

    /// 逐字输入 ASCII 文本 (中文请用 paste_text)
    pub async fn type_text(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            let ks = Self::char_to_keysym(ch)
                .ok_or_else(|| anyhow::anyhow!("字符无映射: '{ch}' — 请用 paste_text"))?;
            let (keycode, need_shift) = self.keysym_to_keycode(ks)
                .ok_or_else(|| anyhow::anyhow!("字符无 keycode: '{ch}'"))?;

            let shift_kc = if need_shift {
                self.keysym_to_keycode(keysym::XK_SHIFT_L).map(|(kc, _)| kc)
            } else { None };
            if let Some(skc) = shift_kc { self.raw_key_press(skc)?; }

            self.raw_key_press(keycode)?;
            tokio::time::sleep(std::time::Duration::from_millis(KEY_HOLD_MS)).await;
            self.raw_key_release(keycode)?;

            if let Some(skc) = shift_kc { self.raw_key_release(skc)?; }
            tokio::time::sleep(std::time::Duration::from_millis(TYPING_DELAY_MS)).await;
        }
        Ok(())
    }

    /// 通过剪贴板粘贴文本 (支持中文、空格等任意字符)
    pub async fn paste_text(&mut self, text: &str) -> Result<()> {
        self.clipboard_paste(text).await
    }

    async fn clipboard_paste(&mut self, text: &str) -> Result<()> {
        info!("📋 粘贴文本: {} 字符", text.len());

        // 使用 X11 Selection 协议设置剪贴板 (无需 xclip 子进程)
        // 流程: spawn_blocking 中获取 CLIPBOARD ownership + 事件循环
        //       main thread 中发送 Ctrl+V, 触发 SelectionRequest
        let text_owned = text.to_string();
        let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".into());

        // 使用缓存的 Atom 值 (启动时已 intern, 避免每次重复查询)
        let clipboard_atom = self.atom_clipboard;
        let utf8_atom = self.atom_utf8_string;
        let targets_atom_cached = self.atom_targets;

        // 同步通道: blocking thread 通知 ownership 已就绪
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::task::spawn_blocking(move || -> Result<()> {
            use x11rb::connection::Connection;
            use x11rb::wrapper::ConnectionExt as _;
            use x11rb::protocol::xproto::*;
            use x11rb::protocol::Event;

            let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(Some(&display_env))
                .context("X11 clipboard 连接失败")?;
            let screen = &conn.setup().roots[screen_num];

            // 复用缓存的 Atom, 无需重新 intern
            let clipboard = clipboard_atom;
            let utf8_string = utf8_atom;
            let targets_atom = targets_atom_cached;

            // 隐藏窗口作为 clipboard owner
            let win = conn.generate_id()?;
            conn.create_window(
                0, win, screen.root,
                0, 0, 1, 1, 0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )?;
            conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME)?;
            conn.flush()?;

            let owner = conn.get_selection_owner(clipboard)?.reply()?.owner;
            if owner != win {
                conn.destroy_window(win)?;
                conn.flush()?;
                anyhow::bail!("无法获取 CLIPBOARD ownership");
            }

            // 通知主线程: ownership 已就绪, 可以发 Ctrl+V 了
            let _ = ready_tx.send(());

            // 事件循环: 响应 SelectionRequest (Ctrl+V 触发后目标应用会请求)
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);

            while std::time::Instant::now() < deadline {
                if let Ok(Some(event)) = conn.poll_for_event() {
                    match event {
                        Event::SelectionRequest(req) => {
                            let mut reply = SelectionNotifyEvent {
                                response_type: 31,
                                sequence: 0,
                                time: req.time,
                                requestor: req.requestor,
                                selection: req.selection,
                                target: req.target,
                                property: 0u32.into(),
                            };

                            if req.target == targets_atom {
                                let targets = [targets_atom, utf8_string, AtomEnum::STRING.into()];
                                let _ = conn.change_property32(
                                    PropMode::REPLACE, req.requestor, req.property,
                                    AtomEnum::ATOM, &targets,
                                );
                                reply.property = req.property;
                            } else if req.target == utf8_string || req.target == u32::from(AtomEnum::STRING) {
                                let _ = conn.change_property8(
                                    PropMode::REPLACE, req.requestor, req.property,
                                    utf8_string, text_owned.as_bytes(),
                                );
                                reply.property = req.property;
                            }

                            let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, reply);
                            let _ = conn.flush();

                            // UTF8 内容已提供,短暂等待后退出 (目标可能还会请求 TARGETS 等)
                            if req.target == utf8_string || req.target == u32::from(AtomEnum::STRING) {
                                // 多等 200ms 处理可能的后续请求 (如 SAVE_TARGETS)
                                let extra_deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
                                while std::time::Instant::now() < extra_deadline {
                                    if let Ok(Some(Event::SelectionRequest(req2))) = conn.poll_for_event() {
                                        let mut r2 = SelectionNotifyEvent {
                                            response_type: 31, sequence: 0,
                                            time: req2.time, requestor: req2.requestor,
                                            selection: req2.selection, target: req2.target,
                                            property: 0u32.into(),
                                        };
                                        if req2.target == targets_atom {
                                            let targets = [targets_atom, utf8_string, AtomEnum::STRING.into()];
                                            let _ = conn.change_property32(PropMode::REPLACE, req2.requestor, req2.property, AtomEnum::ATOM, &targets);
                                            r2.property = req2.property;
                                        } else if req2.target == utf8_string || req2.target == u32::from(AtomEnum::STRING) {
                                            let _ = conn.change_property8(PropMode::REPLACE, req2.requestor, req2.property, utf8_string, text_owned.as_bytes());
                                            r2.property = req2.property;
                                        }
                                        let _ = conn.send_event(false, req2.requestor, EventMask::NO_EVENT, r2);
                                        let _ = conn.flush();
                                    } else {
                                        std::thread::sleep(std::time::Duration::from_millis(10));
                                    }
                                }
                                break;
                            }
                        }
                        Event::SelectionClear(_) => break,
                        _ => {}
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }

            conn.destroy_window(win)?;
            conn.flush()?;
            Ok(())
        });

        // 等待 ownership 就绪后发 Ctrl+V
        let _ = ready_rx.await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        self.key_combo("ctrl+v").await?;

        // 等待 blocking thread 完成事件处理
        handle.await??;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(())
    }

    /// 通过剪贴板粘贴图片文件 (xclip + Ctrl+V)
    pub async fn paste_image(&mut self, image_path: &str) -> Result<()> {
        info!("🖼️ 粘贴图片: {}", image_path);

        // 检测 MIME 类型
        let mime = if image_path.ends_with(".png") {
            "image/png"
        } else if image_path.ends_with(".jpg") || image_path.ends_with(".jpeg") {
            "image/jpeg"
        } else if image_path.ends_with(".gif") {
            "image/gif"
        } else if image_path.ends_with(".bmp") {
            "image/bmp"
        } else {
            "image/png" // 默认 PNG
        };

        // xclip -selection clipboard -t image/png -i /path/to/image (异步)
        let status = tokio::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", mime, "-i", image_path])
            .status()
            .await
            .context("启动 xclip 失败 (图片)")?;

        if !status.success() {
            anyhow::bail!("xclip 图片复制失败: exit={:?}", status.code());
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Ctrl+V 粘贴
        self.key_combo("ctrl+v").await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        Ok(())
    }

    // =================================================================
    // 鼠标操作
    // =================================================================

    /// 鼠标移动到绝对坐标
    pub async fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        self.conn.xtest_fake_input(MOTION_NOTIFY, 0, 0, self.screen_root, x as i16, y as i16, 0)?;
        self.conn.flush()?;
        debug!("🖱️ move_mouse: ({x}, {y})");
        Ok(())
    }

    /// 鼠标单击
    pub async fn click(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 按下左键
        self.conn.xtest_fake_input(BUTTON_PRESS, 1, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;
        tokio::time::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS)).await;

        // 释放左键
        self.conn.xtest_fake_input(BUTTON_RELEASE, 1, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;

        debug!("🖱️ click: ({x}, {y})");
        Ok(())
    }

    /// 鼠标双击
    pub async fn double_click(&mut self, x: i32, y: i32) -> Result<()> {
        self.click(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.click(x, y).await?;
        Ok(())
    }

    /// 鼠标右键点击
    pub async fn right_click(&mut self, x: i32, y: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.conn.xtest_fake_input(BUTTON_PRESS, 3, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;
        tokio::time::sleep(std::time::Duration::from_millis(CLICK_HOLD_MS)).await;

        self.conn.xtest_fake_input(BUTTON_RELEASE, 3, 0, self.screen_root, 0, 0, 0)?;
        self.conn.flush()?;

        debug!("🖱️ right_click: ({x}, {y})");
        Ok(())
    }

    /// 鼠标滚轮 (正=上, 负=下)
    ///
    /// X11: button 4 = scroll up, button 5 = scroll down
    pub async fn scroll(&mut self, x: i32, y: i32, clicks: i32) -> Result<()> {
        self.move_mouse(x, y).await?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let button: u8 = if clicks > 0 { 4 } else { 5 };
        for _ in 0..clicks.unsigned_abs() {
            self.conn.xtest_fake_input(BUTTON_PRESS, button, 0, self.screen_root, 0, 0, 0)?;
            self.conn.xtest_fake_input(BUTTON_RELEASE, button, 0, self.screen_root, 0, 0, 0)?;
            self.conn.flush()?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        debug!("🖱️ scroll: ({x}, {y}) clicks={clicks}");
        Ok(())
    }

    // =================================================================
    // 窗口管理 (X11 原生, 替代 xdotool)
    // =================================================================

    /// 按标题搜索窗口 (EWMH _NET_CLIENT_LIST + 标题匹配)
    ///
    /// `exact=true`: 精确匹配; `exact=false`: contains 匹配
    /// 返回匹配的 (window_id, window_name) 列表
    pub fn find_windows_by_title(&self, title: &str, exact: bool) -> Result<Vec<(u32, String)>> {
        // 使用缓存的 Atom (启动时已 intern)
        let wm_name_atom = self.atom_net_wm_name;
        let utf8_atom = self.atom_utf8_string;
        let client_list_atom = self.atom_net_client_list;

        // 优先: _NET_CLIENT_LIST (WM 托管的所有顶层窗口)
        let windows: Vec<u32> = if let Ok(reply) = self.conn.get_property(
            false, self.screen_root, client_list_atom,
            u32::from(AtomEnum::WINDOW), 0, 4096,
        )?.reply() {
            if reply.format == 32 && !reply.value.is_empty() {
                reply.value.chunks_exact(4)
                    .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                    .collect()
            } else {
                // 回退: query_tree
                self.conn.query_tree(self.screen_root)?.reply()?.children
            }
        } else {
            self.conn.query_tree(self.screen_root)?.reply()?.children
        };

        let mut found = Vec::new();

        for &win in &windows {
            // 尝试 _NET_WM_NAME (UTF-8), 回退 WM_NAME
            let name = if let Ok(reply) = self.conn.get_property(
                false, win, wm_name_atom, utf8_atom, 0, 1024,
            )?.reply() {
                if reply.value.is_empty() {
                    if let Ok(reply2) = self.conn.get_property(
                        false, win, u32::from(AtomEnum::WM_NAME), u32::from(AtomEnum::STRING), 0, 1024,
                    )?.reply() {
                        String::from_utf8_lossy(&reply2.value).to_string()
                    } else {
                        continue;
                    }
                } else {
                    String::from_utf8_lossy(&reply.value).to_string()
                }
            } else {
                continue;
            };

            let matched = if exact { name == title } else { name.contains(title) };
            if matched {
                found.push((win, name));
            }
        }
        Ok(found)
    }

    /// 通过窗口标题激活指定窗口 (X11 _NET_ACTIVE_WINDOW)
    ///
    /// 返回是否成功找到并激活了窗口
    pub fn activate_window_by_title(&self, title: &str, exact: bool) -> Result<bool> {
        let windows = self.find_windows_by_title(title, exact)?;
        if let Some((win, name)) = windows.first() {
            debug!("🖱️ 激活窗口: '{name}' (wid={win})");
            let active_atom = self.atom_net_active_window;
            // _NET_ACTIVE_WINDOW: data[0]=source(1=app), data[1]=timestamp, data[2]=requestor
            let event = ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: *win,
                type_: active_atom,
                data: [1u32, 0, 0, 0, 0].into(),
            };
            self.conn.send_event(
                false,
                self.screen_root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
            self.conn.flush()?;
            Ok(true)
        } else {
            debug!("🖱️ 未找到标题匹配 '{title}' 的窗口");
            Ok(false)
        }
    }

    /// 通过窗口标题关闭指定窗口 (X11 _NET_CLOSE_WINDOW)
    pub fn close_window_by_title(&self, title: &str) -> Result<bool> {
        let windows = self.find_windows_by_title(title, false)?;
        if let Some((win, name)) = windows.first() {
            info!("🗑️ 关闭窗口: '{name}' (匹配 '{title}')");
            let close_atom = self.atom_net_close_window;
            let event = ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: *win,
                type_: close_atom,
                data: [0u32; 5].into(),
            };
            self.conn.send_event(
                false,
                self.screen_root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
            self.conn.flush()?;
            Ok(true)
        } else {
            debug!("🗑️ 未找到标题包含 '{title}' 的窗口");
            Ok(false)
        }
    }

    /// 发送 Enter 键
    pub async fn press_enter(&mut self) -> Result<()> {
        self.press_key("Return").await
    }
}
