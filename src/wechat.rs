//! 微信业务逻辑
//!
//! 依赖 atspi::AtSpi + input::InputEngine + chatwnd::ChatWnd，提供:
//! - 微信应用/控件查找 (含缓存)
//! - 会话管理: 列表、切换 (ChatWith)
//! - 发送消息: 定位输入框 → 聚焦 → 粘贴验证 → 发送验证
//! - 独立窗口管理: ChatWnd 弹出/监听/关闭

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::atspi::{AtSpi, NodeRef, SearchAction, is_structural_role};
use crate::chatwnd::ChatWnd;
use crate::input::InputEngine;

// =====================================================================
// 状态
// =====================================================================

#[derive(Debug, Clone, serde::Serialize)]
pub enum WeChatStatus {
    /// 微信未运行
    NotRunning,
    /// 微信已启动，等待扫码登录
    WaitingForLogin,
    /// 微信已登录
    LoggedIn,
}

impl std::fmt::Display for WeChatStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning => write!(f, "未运行"),
            Self::WaitingForLogin => write!(f, "等待扫码登录"),
            Self::LoggedIn => write!(f, "已登录"),
        }
    }
}

/// 会话信息
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub name: String,
    pub has_new: bool,
}

// =====================================================================
// WeChat 结构
// =====================================================================

/// 缓存的 AT-SPI 节点引用 (带 TTL)
struct CachedNode {
    node: NodeRef,
    cached_at: tokio::time::Instant,
}

impl CachedNode {
    fn new(node: NodeRef) -> Self {
        Self { node, cached_at: tokio::time::Instant::now() }
    }

    fn get(&self, ttl_secs: u64) -> Option<&NodeRef> {
        if self.cached_at.elapsed() < std::time::Duration::from_secs(ttl_secs) {
            Some(&self.node)
        } else {
            None
        }
    }
}

pub struct WeChat {
    atspi: Arc<AtSpi>,
    /// 独立聊天窗口集合 (who → ChatWnd)
    pub listen_windows: Mutex<HashMap<String, ChatWnd>>,
    /// 当前活跃的聊天名称 (避免重复点击同一会话触发双击)
    pub current_chat: Mutex<Option<String>>,
    /// @ 输入流程每步延迟 (ms, 来自 config.toml, 支持热更新)
    at_delay_ms: std::sync::atomic::AtomicU64,
    /// 缓存: 微信应用节点 (TTL 30s)
    cached_app: Mutex<Option<CachedNode>>,
    /// 缓存: 会话列表节点 (TTL 10s)
    cached_session_list: Mutex<Option<CachedNode>>,
}

impl WeChat {
    pub fn new(atspi: Arc<AtSpi>, at_delay_ms: u64) -> Self {
        Self {
            atspi,
            listen_windows: Mutex::new(HashMap::new()),
            current_chat: Mutex::new(None),
            at_delay_ms: std::sync::atomic::AtomicU64::new(at_delay_ms),
            cached_app: Mutex::new(None),
            cached_session_list: Mutex::new(None),
        }
    }

    /// 获取当前 @ 延迟 (ms)
    pub fn get_at_delay_ms(&self) -> u64 {
        self.at_delay_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 热更新 @ 延迟 (ms)
    pub fn set_at_delay_ms(&self, ms: u64) {
        self.at_delay_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    // =================================================================
    // 状态检测
    // =================================================================

    /// 检测微信状态
    /// 通过查找 [tool bar] "导航" 来判断是否已登录
    pub async fn check_status(&self) -> WeChatStatus {
        let app = match self.find_app().await {
            Some(a) => a,
            None => return WeChatStatus::NotRunning,
        };
        // Linux 微信登录后会出现 [tool bar] "导航" 节点
        if self.find_nav_toolbar(&app).await.is_some() {
            WeChatStatus::LoggedIn
        } else {
            WeChatStatus::WaitingForLogin
        }
    }

    /// 触发 AT-SPI2 重连 (清空缓存)
    pub async fn try_reconnect(&self) -> bool {
        // 重连后清空所有缓存节点 (旧节点引用失效)
        *self.cached_app.lock().await = None;
        *self.cached_session_list.lock().await = None;
        self.atspi.reconnect().await
    }

    // =================================================================
    // 控件查找
    // =================================================================

    /// 在 AT-SPI2 Registry 中查找微信应用 (带 30s TTL 缓存)
    pub async fn find_app(&self) -> Option<NodeRef> {
        // 检查缓存
        {
            let cache = self.cached_app.lock().await;
            if let Some(ref cached) = *cache {
                if let Some(node) = cached.get(30) {
                    return Some(node.clone());
                }
            }
        }

        // 缓存未命中, 重新查找
        if let Some(app) = self.scan_registry().await {
            *self.cached_app.lock().await = Some(CachedNode::new(app.clone()));
            return Some(app);
        }
        debug!("Registry 未找到微信, 尝试重连...");
        if self.atspi.reconnect().await {
            if let Some(app) = self.scan_registry().await {
                *self.cached_app.lock().await = Some(CachedNode::new(app.clone()));
                return Some(app);
            }
        }
        // 查找失败, 清空缓存
        *self.cached_app.lock().await = None;
        None
    }

    /// 扫描 Registry 子节点查找微信
    async fn scan_registry(&self) -> Option<NodeRef> {
        let registry = AtSpi::registry()?;
        let count = self.atspi.child_count(&registry).await;
        debug!("Registry 子节点数: {count}");
        for i in 0..count {
            if let Some(child) = self.atspi.child_at(&registry, i).await {
                let name = self.atspi.name(&child).await;
                if is_wechat(&name) {
                    debug!("找到微信: {name}");
                    return Some(child);
                }
            }
        }
        None
    }

    /// 查找导航工具栏 [tool bar] "导航" — 用于判断登录状态
    pub async fn find_nav_toolbar(&self, app: &NodeRef) -> Option<NodeRef> {
        self.atspi.find_bfs(app, |role, name| {
            role == "tool bar" && (name.contains("导航") || name.contains("Navigation"))
        }).await
    }

    /// 查找 [splitter] — 会话列表和聊天区域的容器
    pub async fn find_split_pane(&self, app: &NodeRef) -> Option<NodeRef> {
        self.atspi.find_bfs(app, |role, _| {
            role == "splitter" || role == "split pane"
        }).await
    }

    /// 会话列表 — DFS 查找 [list] name='Chats' (带 10s TTL 缓存)
    pub async fn find_session_list(&self, app: &NodeRef) -> Option<NodeRef> {
        // 检查缓存
        {
            let cache = self.cached_session_list.lock().await;
            if let Some(ref cached) = *cache {
                if let Some(node) = cached.get(10) {
                    return Some(node.clone());
                }
            }
        }

        let result = self.atspi.find_dfs(app, &|role, name| {
            if role == "list" && (name.contains("Chats") || name.contains("会话")) {
                SearchAction::Found
            } else {
                SearchAction::Recurse
            }
        }, 0, 18, 20).await;
        if let Some(ref node) = result {
            debug!("[find_session_list] 找到会话列表");
            *self.cached_session_list.lock().await = Some(CachedNode::new(node.clone()));
        }
        result
    }

    /// 消息列表 — DFS 查找 [list] name='Messages'
    pub async fn find_message_list(&self, app: &NodeRef) -> Option<NodeRef> {
        let result = self.atspi.find_dfs(app, &|role, name| {
            if role == "list" && (name.contains("Messages") || name.contains("消息")) {
                SearchAction::Found
            } else {
                SearchAction::Recurse
            }
        }, 0, 18, 20).await;
        if result.is_some() {
            debug!("[find_message_list] 找到消息列表");
        }
        result
    }

    /// 在 app 范围内查找输入框 (role=entry 或 role=text) — DFS 到 depth 18
    pub async fn find_edit_box(&self, app: &NodeRef) -> Option<NodeRef> {
        self.atspi.find_dfs(app, &|role, _| {
            if role == "entry" || role == "text" {
                SearchAction::Found
            } else {
                SearchAction::Recurse
            }
        }, 0, 18, 20).await
    }

    /// 在会话容器中按名称查找联系人 (BFS 穿透 filler 层级)
    pub async fn find_session(&self, container: &NodeRef, name: &str) -> Option<NodeRef> {
        let mut best_starts_with: Option<NodeRef> = None;
        let mut best_contains: Option<NodeRef> = None;

        let mut frontier = vec![container.clone()];
        for _depth in 0..6 {
            if frontier.is_empty() { break; }
            let mut next = Vec::new();
            for node in &frontier {
                let count = self.atspi.child_count(node).await;
                for i in 0..count.min(30) {
                    if let Some(child) = self.atspi.child_at(node, i).await {
                        let item_name = self.atspi.name(&child).await;
                        let trimmed = item_name.trim();
                        if !trimmed.is_empty() {
                            // 精确匹配 → 直接返回
                            if trimmed == name {
                                return Some(child);
                            }
                            // starts_with 优先于 contains
                            if best_starts_with.is_none() && trimmed.starts_with(name) {
                                best_starts_with = Some(child.clone());
                            } else if best_contains.is_none() && trimmed.contains(name) {
                                best_contains = Some(child.clone());
                            }
                        }
                        let role = self.atspi.role(&child).await;
                        if is_structural_role(&role) {
                            next.push(child);
                        }
                    }
                }
            }
            frontier = next;
        }
        // 优先级: exact (已 early return) > starts_with > contains
        best_starts_with.or(best_contains)
    }

    // =================================================================
    // 会话管理 (借鉴 wxauto GetSessionList / ChatWith)
    // =================================================================

    /// 获取会话列表 — 读取 [list] 'Chats' 的直接子项
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let app = match self.find_app().await {
            Some(a) => a,
            None => return Vec::new(),
        };
        let list = match self.find_session_list(&app).await {
            Some(l) => l,
            None => return Vec::new(),
        };

        let count = self.atspi.child_count(&list).await;
        let mut sessions = Vec::new();

        for i in 0..count.min(50) {
            if let Some(child) = self.atspi.child_at(&list, i).await {
                let name = self.atspi.name(&child).await;
                let trimmed = name.trim().to_string();
                if trimmed.len() > 1 {
                    let has_new = self.check_session_has_new(&child).await;
                    sessions.push(SessionInfo { name: trimmed, has_new });
                }
            }
        }

        sessions
    }

    /// 检查会话是否有新消息
    async fn check_session_has_new(&self, session: &NodeRef) -> bool {
        let count = self.atspi.child_count(session).await;
        for i in 0..count.min(10) {
            if let Some(child) = self.atspi.child_at(session, i).await {
                let role = self.atspi.role(&child).await;
                let name = self.atspi.name(&child).await;
                // 未读角标通常是一个 label 包含数字
                if (role == "label" || role == "static")
                    && !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_digit())
                {
                    return true;
                }
            }
        }
        false
    }

    /// 激活主窗口 (X11 原生 _NET_ACTIVE_WINDOW)
    /// 确保主窗口在独立窗口之上
    async fn focus_main_window(&self, engine: &mut InputEngine) {
        // 策略 1: X11 原生按窗口名精确激活
        for title in ["微信", "WeChat", "Weixin"] {
            match engine.activate_window_by_title(title, true) {
                Ok(true) => {
                    debug!("🖱️ 激活主窗口: {title}");
                    tokio::time::sleep(ms(300)).await;
                    return;
                }
                Ok(false) => {}
                Err(e) => debug!("🖱️ X11 激活失败: {e}"),
            }
        }

        // 策略 2: AT-SPI 坐标点击 (回退)
        if let Some(app) = self.find_app().await {
            let count = self.atspi.child_count(&app).await;
            for i in 0..count.min(10) {
                if let Some(child) = self.atspi.child_at(&app, i).await {
                    let role = self.atspi.role(&child).await;
                    let name = self.atspi.name(&child).await;
                    if role == "frame" && is_wechat_main(&name) {
                        if let Some(bbox) = self.atspi.bbox(&child).await {
                            let cx = (bbox.x + bbox.w / 2).max(0);
                            let cy = (bbox.y + 15).max(0);
                            debug!("🖱️ AT-SPI 点击主窗口聚焦: ({cx}, {cy})");
                            let _ = engine.click(cx, cy).await;
                            tokio::time::sleep(ms(300)).await;
                            return;
                        }
                    }
                }
            }
        }
        warn!("⚠️ 无法聚焦主窗口");
    }

    /// 切换到指定聊天 (借鉴 wxauto ChatWith)
    ///
    /// 逻辑: 检查是否已在目标聊天 → 在会话列表找 → 找不到则 Ctrl+F 搜索
    pub async fn chat_with(
        &self,
        engine: &mut InputEngine,
        who: &str,
    ) -> Result<Option<String>> {
        // 快速路径: 已在目标聊天时跳过切换 (避免重复点击触发双击弹窗)
        {
            let current = self.current_chat.lock().await;
            if let Some(ref name) = *current {
                if name == who {
                    debug!("💬 已在聊天 [{who}], 跳过切换");
                    return Ok(Some(who.to_string()));
                }
            }
        }

        info!("💬 ChatWith: {who}");

        // 先聚焦主窗口 (独立窗口可能遮挡)
        self.focus_main_window(engine).await;

        let app = self.find_app().await
            .ok_or_else(|| anyhow::anyhow!("找不到微信应用"))?;

        // 1. 尝试在会话列表中直接定位
        if let Some(list) = self.find_session_list(&app).await {
            if let Some(item) = self.find_session(&list, who).await {
                if let Some(bbox) = self.atspi.bbox(&item).await {
                    let (cx, cy) = bbox.center();
                    debug!("💬 会话列表找到 [{who}], 点击 ({cx}, {cy})");
                    engine.click(cx, cy).await?;
                    // 轮询等待消息列表出现 (替代固定 500ms)
                    let loaded = wait_for(&self.atspi, &app, 1500, 50,
                        |atspi, app| {
                            let atspi = atspi.clone();
                            let app = app.clone();
                            async move {
                                atspi.find_dfs(&app, &|role, name| {
                                    if role == "list" && (name.contains("消息") || name.contains("Messages")) {
                                        SearchAction::Found
                                    } else { SearchAction::Recurse }
                                }, 0, 18, 20).await.is_some()
                            }
                        }
                    ).await;
                    debug!("💬 ChatWith 点击后消息列表: {}", if loaded { "已就绪" } else { "超时" });
                    *self.current_chat.lock().await = Some(who.to_string());
                    return Ok(Some(who.to_string()));
                }
            }
        }

        // 2. 搜索回退 (借鉴 wxauto Ctrl+F 搜索)
        debug!("💬 列表未找到 [{who}], 进入搜索模式");

        // Ctrl+F 打开搜索
        engine.key_combo("ctrl+f").await?;
        // 轮询等待搜索输入框出现 (替代固定 500ms)
        wait_for(&self.atspi, &app, 800, 50,
            |atspi, app| {
                let atspi = atspi.clone();
                let app = app.clone();
                async move {
                    atspi.find_dfs(&app, &|role, _| {
                        if role == "entry" || role == "text" {
                            SearchAction::Found
                        } else { SearchAction::Recurse }
                    }, 0, 18, 20).await.is_some()
                }
            }
        ).await;

        // 清除可能的旧搜索内容
        engine.key_combo("ctrl+a").await?;
        tokio::time::sleep(ms(100)).await;

        // 粘贴搜索关键词
        engine.paste_text(who).await?;
        // 轮询等待搜索结果出现 (替代固定 1500ms, 最多等 2s)
        wait_for(&self.atspi, &app, 2000, 100,
            |atspi, app| {
                let atspi = atspi.clone();
                let app = app.clone();
                async move {
                    // 搜索结果出现时会有新的 list item
                    atspi.find_dfs(&app, &|role, name| {
                        if role == "list" && !name.contains("Chats") && !name.contains("会话") && !name.is_empty() {
                            SearchAction::Found
                        } else { SearchAction::Recurse }
                    }, 0, 18, 20).await.is_some()
                }
            }
        ).await;

        // 选择第一个搜索结果 (Enter)
        engine.press_enter().await?;
        // 轮询等待消息列表出现 (替代固定 800ms)
        let loaded = wait_for(&self.atspi, &app, 2000, 50,
            |atspi, app| {
                let atspi = atspi.clone();
                let app = app.clone();
                async move {
                    atspi.find_dfs(&app, &|role, name| {
                        if role == "list" && (name.contains("消息") || name.contains("Messages")) {
                            SearchAction::Found
                        } else { SearchAction::Recurse }
                    }, 0, 18, 20).await.is_some()
                }
            }
        ).await;
        debug!("💬 搜索切换后消息列表: {}", if loaded { "已就绪" } else { "超时" });

        // Esc 关闭搜索框 (借鉴 wxauto _refresh)
        engine.press_key("Escape").await?;
        // 轮询等待消息列表恢复 (替代固定 500ms)
        wait_for(&self.atspi, &app, 800, 50,
            |atspi, app| {
                let atspi = atspi.clone();
                let app = app.clone();
                async move {
                    atspi.find_dfs(&app, &|role, name| {
                        if role == "list" && (name.contains("消息") || name.contains("Messages")) {
                            SearchAction::Found
                        } else { SearchAction::Recurse }
                    }, 0, 18, 20).await.is_some()
                }
            }
        ).await;

        // 验证是否切换成功
        if self.find_message_list(&app).await.is_some() {
            debug!("💬 搜索切换成功: {who}");
            // 仅缓存真正的显示名, 不缓存 chatroom ID (避免后续误跳过)
            if !who.contains("@chatroom") {
                *self.current_chat.lock().await = Some(who.to_string());
            }
            Ok(Some(who.to_string()))
        } else {
            info!("💬 搜索未找到结果: [{who}]");
            *self.current_chat.lock().await = None;
            return Ok(None);
        }
    }

    // =================================================================
    // 独立窗口管理 (借鉴 wxauto AddListenChat / ChatWnd)
    // =================================================================

    /// 添加监听目标 — 弹出独立窗口
    ///
    /// 流程: ChatWith 切换 → 双击弹出独立窗口 → 在 Registry 中查找新窗口
    pub async fn add_listen(
        &self,
        engine: &mut InputEngine,
        who: &str,
    ) -> Result<bool> {
        info!("👂 添加监听: {who}");

        let app = self.find_app().await
            .ok_or_else(|| anyhow::anyhow!("找不到微信应用"))?;

        // 1. 先检查是否已有记录
        {
            let mut windows = self.listen_windows.lock().await;
            if let Some(chatwnd) = windows.get(who) {
                if chatwnd.is_alive().await {
                    debug!("👂 独立窗口已存在且存活: {who}");
                    return Ok(true);
                } else {
                    debug!("👂 独立窗口已失效, 移除旧记录: {who}");
                    windows.remove(who);
                }
            }
        }

        // 2. 检查是否有未注册的独立窗口
        if let Some(wnd_node) = self.find_chat_window(&app, who).await {
            let mut windows = self.listen_windows.lock().await;
            let mut chatwnd = ChatWnd::new(who.to_string(), self.atspi.clone(), wnd_node);
            chatwnd.init_edit_box().await;
            chatwnd.init_msg_list().await;
            windows.insert(who.to_string(), chatwnd);
            debug!("👂 找到现有独立窗口, 已注册: {who}");
            return Ok(true);
        }

        // 2. 点击主窗口确保聚焦 (避免被旧的独立窗口遮挡)
        self.focus_main_window(engine).await;

        // 3. 切换到该聊天
        self.chat_with(engine, who).await?;

        // 3. 在会话列表中找到该项并双击弹出独立窗口
        if let Some(list) = self.find_session_list(&app).await {
            if let Some(item) = self.find_session(&list, who).await {
                if let Some(bbox) = self.atspi.bbox(&item).await {
                    let (cx, cy) = bbox.center();
                    engine.double_click(cx, cy).await?;
                    debug!("👂 双击会话弹出独立窗口: ({cx}, {cy})");
                    // 轮询等待独立窗口出现 (替代固定 1000ms)
                    let appeared = wait_for(&self.atspi, &app, 2000, 100,
                        |atspi, app| {
                            let atspi = atspi.clone();
                            let app = app.clone();
                            let who_owned = who.to_string();
                            async move {
                                let count = atspi.child_count(&app).await;
                                for i in 0..count.min(20) {
                                    if let Some(child) = atspi.child_at(&app, i).await {
                                        let role = atspi.role(&child).await;
                                        let name = atspi.name(&child).await;
                                        if role == "frame" && name.contains(&who_owned) && !is_wechat_main(&name) {
                                            return true;
                                        }
                                    }
                                }
                                false
                            }
                        }
                    ).await;
                    debug!("👂 独立窗口弹出: {}", if appeared { "已检测到" } else { "超时" });
                    // 双击弹出独立窗口后, 主窗口状态已变, 重置 current_chat
                    *self.current_chat.lock().await = None;
                }
            }
        }

        // 4. 查找新弹出的独立窗口 — 轮询 (替代固定 3×1500ms 重试)
        let wnd_node = wait_for_result(&self.atspi, &app, 5000, 200,
            |atspi, app| {
                let atspi = atspi.clone();
                let app = app.clone();
                let who_owned = who.to_string();
                async move {
                    let count = atspi.child_count(&app).await;
                    for i in 0..count.min(20) {
                        if let Some(child) = atspi.child_at(&app, i).await {
                            let role = atspi.role(&child).await;
                            let name = atspi.name(&child).await;
                            if role == "frame" && name.contains(&who_owned) && !is_wechat_main(&name) {
                                return Some(child);
                            }
                        }
                    }
                    None
                }
            }
        ).await;

        if let Some(wnd_node) = wnd_node {
            let mut chatwnd = ChatWnd::new(who.to_string(), self.atspi.clone(), wnd_node);
            chatwnd.init_edit_box().await;
            chatwnd.init_msg_list().await;
            let mut windows = self.listen_windows.lock().await;
            windows.insert(who.to_string(), chatwnd);
            info!("👂 成功添加监听: {who}");
            return Ok(true);
        }
        warn!("👂 轮询超时后仍未找到独立窗口: {who}");
        Ok(false)
    }

    /// 移除监听目标 — 关闭独立窗口 (X11 原生)
    pub async fn remove_listen(&self, engine: &InputEngine, who: &str) -> bool {
        let mut windows = self.listen_windows.lock().await;
        if windows.remove(who).is_some() {
            info!("👂 移除监听: {who}");
            drop(windows); // 释放锁
            // X11 原生关闭窗口
            match engine.close_window_by_title(who) {
                Ok(true) => info!("👂 已关闭独立窗口: {who}"),
                Ok(false) => info!("👂 未找到独立窗口 (可能已关闭): {who}"),
                Err(e) => warn!("👂 X11 关闭窗口失败: {e}"),
            }
            *self.current_chat.lock().await = None;
            true
        } else {
            false
        }
    }

    /// 获取所有监听目标
    pub async fn get_listen_list(&self) -> Vec<String> {
        let windows = self.listen_windows.lock().await;
        windows.keys().cloned().collect()
    }



    /// 查找独立聊天窗口
    ///
    /// 策略:
    /// 1. 在 wechat app 的子节点中查找以 who 命名的 frame (独立窗口是 app 的子 frame)
    /// 2. 在 AT-SPI2 registry 中查找单独注册的窗口
    async fn find_chat_window(&self, app: &NodeRef, who: &str) -> Option<NodeRef> {
        // 策略 1: 在 wechat app 的直接子节点中查找
        let app_child_count = self.atspi.child_count(app).await;
        for i in 0..app_child_count.min(20) {
            if let Some(child) = self.atspi.child_at(app, i).await {
                let role = self.atspi.role(&child).await;
                let name = self.atspi.name(&child).await;
                if role == "frame" && name.contains(who) && !is_wechat_main(&name) {
                    debug!("📌 找到独立聊天窗口 (app 子节点): {name}");
                    return Some(child);
                }
            }
        }

        // 策略 2: 在 AT-SPI2 registry 中查找单独注册的窗口
        if let Some(registry) = AtSpi::registry() {
            let count = self.atspi.child_count(&registry).await;
            for i in 0..count {
                if let Some(child) = self.atspi.child_at(&registry, i).await {
                    let name = self.atspi.name(&child).await;
                    if name.contains(who) && !is_wechat_main(&name) {
                        // 遍历子 frame
                        let child_count = self.atspi.child_count(&child).await;
                        for j in 0..child_count.min(5) {
                            if let Some(frame) = self.atspi.child_at(&child, j).await {
                                let role = self.atspi.role(&frame).await;
                                if role == "frame" {
                                    let fname = self.atspi.name(&frame).await;
                                    if fname.contains(who) {
                                        debug!("📌 找到独立聊天窗口 (registry): {fname}");
                                        return Some(frame);
                                    }
                                }
                            }
                        }
                        let role = self.atspi.role(&child).await;
                        debug!("📌 跳过非精确匹配的节点: [{role}] {name} (内层 frame 未匹配)");
                    }
                }
            }
        }
        None
    }


    // =================================================================
    // 发送消息 (增强版)
    // =================================================================

    /// 清理失效的独立窗口 (send_message/send_image 的公共前置步骤)
    ///
    /// 返回 true = 独立窗口存活, 调用方应使用独立窗口发送
    /// 返回 false = 无独立窗口或已失效, 调用方应回退主窗口
    pub async fn check_listen_window(&self, to: &str) -> bool {
        let mut windows = self.listen_windows.lock().await;
        if let Some(chatwnd) = windows.get(to) {
            if chatwnd.is_alive().await {
                return true;
            }
            debug!("📤 独立窗口已失效, 移除: {to}");
            windows.remove(to);
            drop(windows);
            *self.current_chat.lock().await = None;
        }
        false
    }

    /// 尝试恢复失效的独立窗口
    ///
    /// 在 check_listen_window 返回 false 后调用,
    /// 如果该联系人之前在监听列表中, 自动重建独立窗口
    pub async fn try_recover_listen_window(
        &self, engine: &mut InputEngine, to: &str,
    ) -> bool {
        // 只在确实没有窗口时尝试恢复 (避免对从未监听的联系人重建)
        let has_window = self.listen_windows.lock().await.contains_key(to);
        if has_window {
            return true; // 窗口还在, 不需要恢复
        }
        // 注意: 这里不检查 "之前是否监听过" — 如果窗口刚被 check_listen_window 移除,
        // 说明之前确实在监听, 值得尝试恢复
        info!("🔄 尝试自动恢复独立窗口: {to}");
        match self.add_listen(engine, to).await {
            Ok(true) => {
                info!("✅ 独立窗口自动恢复成功: {to}");
                true
            }
            Ok(false) => {
                warn!("⚠️ 独立窗口自动恢复失败: {to}");
                false
            }
            Err(e) => {
                warn!("⚠️ 独立窗口自动恢复出错: {to} — {e}");
                false
            }
        }
    }

    /// 主窗口发送前置: 切换到目标聊天并等待输入框就绪
    async fn prepare_main_send(
        &self,
        engine: &mut InputEngine,
        to: &str,
        force_switch: bool,
    ) -> Result<bool> {
        if force_switch {
            *self.current_chat.lock().await = None;
        }
        let chat_result = self.chat_with(engine, to).await?;
        if chat_result.is_none() {
            return Ok(false);
        }
        tokio::time::sleep(ms(300)).await;
        Ok(true)
    }

    /// 完整发送流程
    ///
    /// 流程: 优先独立窗口 → 回退主窗口 → 切换聊天 → @ → 粘贴 → 发送
    pub async fn send_message(
        &self,
        engine: &mut InputEngine,
        to: &str,
        text: &str,
        at: &[String],
        skip_verify: bool,
    ) -> Result<(bool, bool, String)> {
        info!("📤 开始发送: [{to}] → {text} (@ {} 人)", at.len());

        // 优先使用独立窗口
        if self.check_listen_window(to).await {
            let mut windows = self.listen_windows.lock().await;
            if let Some(chatwnd) = windows.get_mut(to) {
                debug!("📤 使用独立窗口发送: {to}");
                // 独立窗口: 先激活并聚焦输入框, 然后 @ + 文本
                chatwnd.activate_and_focus_input(engine).await?;
                type_at_mentions(engine, at, self.get_at_delay_ms()).await?;
                return chatwnd.send_message(engine, text, skip_verify).await;
            }
        }

        // 主窗口发送
        if !self.prepare_main_send(engine, to, false).await? {
            return Ok((false, false, format!("未找到聊天: {to}")));
        }

        let app = self.find_app().await
            .ok_or_else(|| anyhow::anyhow!("找不到微信应用"))?;

        // 输入 @ 列表
        type_at_mentions(engine, at, self.get_at_delay_ms()).await?;

        engine.paste_text(text).await?;
        tokio::time::sleep(ms(300)).await;

        engine.press_enter().await?;
        tokio::time::sleep(ms(500)).await;

        let verified = if skip_verify {
            debug!("⏩ 跳过 AT-SPI 验证 (将由 DB 验证): [{to}]");
            false
        } else {
            self.verify_sent(&app, text).await
        };

        let msg = if verified { "消息已发送" } else { "消息已发送 (未验证)" };
        info!("✅ 完成: [{to}] verified={verified}");
        Ok((true, verified, msg.into()))
    }

    /// 发送图片 (优先独立窗口, 回退主窗口)
    pub async fn send_image(
        &self,
        engine: &mut InputEngine,
        to: &str,
        image_path: &str,
    ) -> Result<(bool, bool, String)> {
        info!("🖼️ 开始发送图片: [{to}] → {image_path}");

        // 优先使用独立窗口
        if self.check_listen_window(to).await {
            let mut windows = self.listen_windows.lock().await;
            if let Some(chatwnd) = windows.get_mut(to) {
                debug!("🖼️ 使用独立窗口发送图片: {to}");
                return chatwnd.send_image(engine, image_path).await;
            }
        }

        // 主窗口发送 (强制切换, 避免独立窗口偷焦点)
        if !self.prepare_main_send(engine, to, true).await? {
            return Ok((false, false, format!("未找到聊天: {to}")));
        }

        engine.paste_image(image_path).await?;
        tokio::time::sleep(ms(500)).await;

        engine.press_enter().await?;

        info!("✅ 图片发送完成: [{to}]");
        Ok((true, false, "图片已发送".into()))
    }

    /// 验证消息是否出现在消息列表末尾 (检查最后几条)
    async fn verify_sent(&self, app: &NodeRef, text: &str) -> bool {
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(ms(500)).await;
            }
            if let Some(msg_list) = self.find_message_list(app).await {
                if verify_sent_in_list(&self.atspi, &msg_list, text, attempt).await {
                    return true;
                }
            }
        }
        false
    }
}

// =====================================================================
// 辅助函数
// =====================================================================

fn is_wechat(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("wechat") || lower.contains("weixin") || name.contains("微信")
}

/// 区分微信主窗口 vs 独立聊天窗口
fn is_wechat_main(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "wechat" || lower == "weixin" || name == "微信"
}

/// 在输入框中逐个输入 @ 列表 (触发微信联系人选择器)
///
/// 流程 (每人): 输入 "@" → 等待选择器弹出 → 粘贴名字搜索 → 回车选中
async fn type_at_mentions(engine: &mut InputEngine, at: &[String], delay_ms: u64) -> anyhow::Result<()> {
    for name in at {
        if name.is_empty() { continue; }
        debug!("📢 输入 @: {name}");
        // 1. 键盘输入 "@" 字符触发联系人选择器 (必须用 type_text 而非 paste_text,
        //    因为微信只响应键盘事件触发选择器, 剪贴板粘贴不会触发)
        engine.type_text("@").await?;
        tokio::time::sleep(ms(delay_ms)).await;
        // 2. 粘贴名字搜索
        engine.paste_text(name).await?;
        tokio::time::sleep(ms(delay_ms)).await;
        // 3. 回车选中第一个匹配结果
        engine.press_enter().await?;
        tokio::time::sleep(ms(delay_ms * 2 / 3)).await;
    }
    Ok(())
}



/// 公共发送验证: 检查消息列表末尾是否包含指定文本
///
/// 被 WeChat::verify_sent 和 ChatWnd::verify_sent 共用, 消除 copy-paste
pub(crate) async fn verify_sent_in_list(atspi: &AtSpi, msg_list: &NodeRef, text: &str, attempt: i32) -> bool {
    let count = atspi.child_count(msg_list).await;
    if count <= 0 { return false; }

    let check_range = 3.min(count);
    for i in (count - check_range)..count {
        if let Some(child) = atspi.child_at(msg_list, i).await {
            let name = atspi.name(&child).await;
            let trimmed = name.trim();
            let len_ok = !trimmed.is_empty()
                && trimmed.len() <= text.len() * 2 + 10
                && text.len() <= trimmed.len() * 2 + 10;
            if len_ok && (trimmed.contains(text) || text.contains(trimmed)) {
                debug!("✅ 验证成功 (attempt {attempt})");
                return true;
            }
        }
    }
    false
}

pub(crate) fn ms(n: u64) -> std::time::Duration {
    std::time::Duration::from_millis(n)
}

/// 轮询等待条件满足 (布尔版)
///
/// 最多等待 `max_ms` 毫秒, 每 `interval_ms` 检查一次
/// 返回: 条件是否在超时前满足
async fn wait_for<F, Fut>(
    atspi: &Arc<AtSpi>, app: &NodeRef,
    max_ms: u64, interval_ms: u64,
    check: F,
) -> bool
where
    F: Fn(&Arc<AtSpi>, &NodeRef) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(max_ms);
    while tokio::time::Instant::now() < deadline {
        if check(atspi, app).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
    false
}

/// 轮询等待并返回结果 (泛型版)
///
/// 最多等待 `max_ms` 毫秒, 每 `interval_ms` 检查一次
/// 返回: 检查函数的结果 (Some = 成功, None = 超时)
async fn wait_for_result<F, Fut, T>(
    atspi: &Arc<AtSpi>, app: &NodeRef,
    max_ms: u64, interval_ms: u64,
    check: F,
) -> Option<T>
where
    F: Fn(&Arc<AtSpi>, &NodeRef) -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(max_ms);
    while tokio::time::Instant::now() < deadline {
        if let Some(result) = check(atspi, app).await {
            return Some(result);
        }
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
    None
}
