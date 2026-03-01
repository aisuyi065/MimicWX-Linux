//! 微信业务逻辑
//!
//! 依赖 atspi::AtSpi + input::InputEngine + chatwnd::ChatWnd，提供:
//! - 微信应用/控件查找 (含缓存)
//! - 会话管理: 列表、切换 (ChatWith)
//! - 消息读取: 全量/增量 + 类型分类 + 内容哈希去重
//! - 发送消息: 定位输入框 → 聚焦 → 粘贴验证 → 发送验证
//! - 独立窗口管理: ChatWnd 弹出/监听/关闭

use anyhow::Result;
use std::collections::{HashMap, HashSet};
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

// =====================================================================
// 消息类型 (借鉴 wxauto _split + ParseMessage)
// =====================================================================

/// 聊天消息 (增强版)
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    /// 消息索引 (在列表中的位置)
    pub index: i32,
    /// AT-SPI2 角色 (list item / label / filler 等)
    pub role: String,
    /// AT-SPI2 Name 属性 (原始)
    pub name: String,
    /// 子节点内容
    pub children: Vec<ChatMessageChild>,
    /// 消息 ID (内容哈希, 稳定)
    pub msg_id: String,
    /// 消息类型: "sys" | "time" | "self" | "friend" | "recall" | "unknown"
    pub msg_type: String,
    /// 发送者名称
    pub sender: String,
    /// 消息文本内容 (解析后)
    pub content: String,
}

/// 消息子节点
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessageChild {
    pub role: String,
    pub name: String,
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

pub struct WeChat {
    atspi: Arc<AtSpi>,
    /// 已读消息 ID 集合 (主窗口, 用于增量读取)
    seen_msg_ids: Mutex<HashSet<String>>,
    /// 独立聊天窗口集合 (who → ChatWnd)
    pub listen_windows: Mutex<HashMap<String, ChatWnd>>,
    /// 当前活跃的聊天名称 (避免重复点击同一会话触发双击)
    pub current_chat: Mutex<Option<String>>,
    /// 缓冲区: 轮询任务检测到的新消息存在这里, HTTP API 从这里读取
    pending_messages: Mutex<HashMap<String, Vec<ChatMessage>>>,
}

impl WeChat {
    pub fn new(atspi: Arc<AtSpi>) -> Self {
        Self {
            atspi,
            seen_msg_ids: Mutex::new(HashSet::new()),
            listen_windows: Mutex::new(HashMap::new()),
            current_chat: Mutex::new(None),
            pending_messages: Mutex::new(HashMap::new()),
        }
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

    /// 触发 AT-SPI2 重连
    pub async fn try_reconnect(&self) -> bool {
        self.atspi.reconnect().await
    }

    // =================================================================
    // 控件查找
    // =================================================================

    /// 在 AT-SPI2 Registry 中查找微信应用
    pub async fn find_app(&self) -> Option<NodeRef> {
        if let Some(app) = self.scan_registry().await {
            return Some(app);
        }
        debug!("Registry 未找到微信, 尝试重连...");
        if self.atspi.reconnect().await {
            if let Some(app) = self.scan_registry().await {
                return Some(app);
            }
        }
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

    /// 会话列表 — DFS 查找 [list] name='Chats'
    pub async fn find_session_list(&self, app: &NodeRef) -> Option<NodeRef> {
        let result = self.atspi.find_dfs(app, &|role, name| {
            if role == "list" && (name.contains("Chats") || name.contains("会话")) {
                SearchAction::Found
            } else {
                SearchAction::Recurse
            }
        }, 0, 18, 20).await;
        if result.is_some() {
            debug!("[find_session_list] 找到会话列表");
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
        let mut frontier = vec![container.clone()];
        for _depth in 0..6 {
            if frontier.is_empty() { return None; }
            let mut next = Vec::new();
            for node in &frontier {
                let count = self.atspi.child_count(node).await;
                for i in 0..count.min(30) {
                    if let Some(child) = self.atspi.child_at(node, i).await {
                        let item_name = self.atspi.name(&child).await;
                        if !item_name.trim().is_empty() && item_name.contains(name) {
                            return Some(child);
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
        None
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
                    info!("🖱️ 激活主窗口: {title}");
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
                            info!("🖱️ AT-SPI 点击主窗口聚焦: ({cx}, {cy})");
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
                    info!("💬 已在聊天 [{who}], 跳过切换");
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
                    info!("💬 会话列表找到 [{who}], 点击 ({cx}, {cy})");
                    engine.click(cx, cy).await?;
                    tokio::time::sleep(ms(500)).await;
                    *self.current_chat.lock().await = Some(who.to_string());
                    return Ok(Some(who.to_string()));
                }
            }
        }

        // 2. 搜索回退 (借鉴 wxauto Ctrl+F 搜索)
        info!("💬 列表未找到 [{who}], 进入搜索模式");

        // Ctrl+F 打开搜索
        engine.key_combo("ctrl+f").await?;
        tokio::time::sleep(ms(500)).await;

        // 清除可能的旧搜索内容
        engine.key_combo("ctrl+a").await?;
        tokio::time::sleep(ms(100)).await;

        // 粘贴搜索关键词
        engine.paste_text(who).await?;
        tokio::time::sleep(ms(1500)).await;

        // 选择第一个搜索结果 (Enter)
        engine.press_enter().await?;
        tokio::time::sleep(ms(800)).await;

        // Esc 关闭搜索框 (借鉴 wxauto _refresh)
        engine.press_key("Escape").await?;
        tokio::time::sleep(ms(500)).await;

        // 验证是否切换成功
        if self.find_message_list(&app).await.is_some() {
            info!("💬 搜索切换成功: {who}");
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
                    info!("👂 独立窗口已存在且存活: {who}");
                    return Ok(true);
                } else {
                    info!("👂 独立窗口已失效, 移除旧记录: {who}");
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
            info!("👂 找到现有独立窗口, 已注册: {who}");
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
                    info!("👂 双击会话弹出独立窗口: ({cx}, {cy})");
                    tokio::time::sleep(ms(1000)).await;
                    // 双击弹出独立窗口后, 主窗口状态已变, 重置 current_chat
                    *self.current_chat.lock().await = None;
                }
            }
        }

        // 4. 查找新弹出的独立窗口 — 重试 3 次 (窗口需要时间出现在 AT-SPI2 树中)
        for attempt in 0..3 {
            tokio::time::sleep(ms(1500)).await;
            if let Some(wnd_node) = self.find_chat_window(&app, who).await {
                let mut chatwnd = ChatWnd::new(who.to_string(), self.atspi.clone(), wnd_node);
                chatwnd.init_edit_box().await;
                chatwnd.init_msg_list().await;
                chatwnd.mark_all_read().await;
                let mut windows = self.listen_windows.lock().await;
                windows.insert(who.to_string(), chatwnd);
                info!("👂 成功添加监听: {who} (尝试 {attempt})");
                return Ok(true);
            }
            debug!("👂 第 {attempt} 次尝试未找到独立窗口, 继续等待...");
        }
        warn!("👂 3 次尝试后仍未找到独立窗口: {who}");
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

    /// 获取所有监听窗口的新消息 (轮询任务调用, 检测并存入缓冲区)
    pub async fn get_listen_messages(&self) -> HashMap<String, Vec<ChatMessage>> {
        // 先在 listen_windows 锁内收集新消息, 避免嵌套锁
        let mut collected: Vec<(String, Vec<ChatMessage>)> = Vec::new();
        {
            let mut windows = self.listen_windows.lock().await;
            for (who, chatwnd) in windows.iter_mut() {
                let new_msgs = chatwnd.get_new_messages().await;
                if !new_msgs.is_empty() {
                    info!("👂 [poll] {} 有 {} 条新消息", who, new_msgs.len());
                    collected.push((who.clone(), new_msgs));
                }
            }
        } // listen_windows 锁在此释放

        // 再写入 pending_messages (不再嵌套持锁)
        let mut result = HashMap::new();
        if !collected.is_empty() {
            let mut pending = self.pending_messages.lock().await;
            for (who, new_msgs) in collected {
                pending.entry(who.clone())
                    .or_insert_with(Vec::new)
                    .extend(new_msgs.clone());
                result.insert(who, new_msgs);
            }
        }

        result
    }

    /// 取出缓冲区中的新消息 (HTTP API 调用, 读后清空)
    pub async fn take_pending_messages(&self) -> HashMap<String, Vec<ChatMessage>> {
        let mut pending = self.pending_messages.lock().await;
        std::mem::take(&mut *pending)
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
                    info!("📌 找到独立聊天窗口 (app 子节点): {name}");
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
                                        info!("📌 找到独立聊天窗口 (registry): {fname}");
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
    // 消息读取 (主窗口)
    // =================================================================

    /// 读取当前聊天所有消息 (主窗口)
    pub async fn get_all_messages(&self) -> Vec<ChatMessage> {
        let app = match self.find_app().await {
            Some(a) => a,
            None => return Vec::new(),
        };

        let msg_list = match self.find_message_list(&app).await {
            Some(l) => l,
            None => return Vec::new(),
        };

        self.read_message_list(&msg_list).await
    }

    /// 读取消息列表中的所有消息项 (增强版: 带分类)
    async fn read_message_list(&self, msg_list: &NodeRef) -> Vec<ChatMessage> {
        let count = self.atspi.child_count(msg_list).await;
        let mut messages = Vec::new();

        for i in 0..count.min(100) {
            if let Some(child) = self.atspi.child_at(msg_list, i).await {
                let msg = self.parse_message_item(&child, i).await;
                messages.push(msg);
            }
        }

        messages
    }

    /// 解析单个消息项 (借鉴 wxauto _split)
    async fn parse_message_item(&self, item: &NodeRef, index: i32) -> ChatMessage {
        parse_message_item(&self.atspi, item, index).await
    }

    /// 获取新消息 (增量读取, 主窗口)
    pub async fn get_new_messages(&self) -> Vec<ChatMessage> {
        let all = self.get_all_messages().await;

        let mut seen = self.seen_msg_ids.lock().await;
        let new_msgs: Vec<ChatMessage> = all
            .into_iter()
            .filter(|m| !seen.contains(&m.msg_id))
            .collect();

        for m in &new_msgs {
            seen.insert(m.msg_id.clone());
        }

        // 防止无限增长: 超过 500 条时保留最近 200 条 (而非全部清空)
        if seen.len() > 500 {
            // 收集所有 ID 并保留后 200 个
            let all_ids: Vec<String> = seen.iter().cloned().collect();
            seen.clear();
            for id in all_ids.into_iter().rev().take(200) {
                seen.insert(id);
            }
            // 确保本次新消息也在其中
            for m in &new_msgs {
                seen.insert(m.msg_id.clone());
            }
        }

        new_msgs
    }

    /// 重置已读消息 ID (初始化时调用)
    pub async fn mark_all_read(&self) {
        let all = self.get_all_messages().await;
        let mut seen = self.seen_msg_ids.lock().await;
        seen.clear();
        for m in &all {
            seen.insert(m.msg_id.clone());
        }
        debug!("标记 {} 条消息为已读", seen.len());
    }

    // =================================================================
    // 发送消息 (增强版)
    // =================================================================

    /// 完整发送流程 (简化版, 参考 MimicWX-old)
    ///
    /// 流程: 切换到目标聊天 → 直接粘贴发送
    /// 微信选中聊天后输入框自动获焦, 无需手动查找 edit box
    pub async fn send_message(
        &self,
        engine: &mut InputEngine,
        to: &str,
        text: &str,
        skip_verify: bool,
    ) -> Result<(bool, bool, String)> {
        info!("📤 开始发送: [{to}] → {text}");

        // 检查是否有独立窗口可用
        {
            let mut windows = self.listen_windows.lock().await;
            if let Some(chatwnd) = windows.get_mut(to) {
                if chatwnd.is_alive().await {
                    info!("📤 使用独立窗口发送: {to}");
                    return chatwnd.send_message(engine, text, skip_verify).await;
                } else {
                    info!("📤 独立窗口已失效, 移除: {to}");
                    windows.remove(to);
                    // 独立窗口失效 → 清空缓存, 确保主窗口路径重新切换
                    drop(windows);
                    *self.current_chat.lock().await = None;
                }
            }
        }

        // 主窗口发送
        let app = self.find_app().await
            .ok_or_else(|| anyhow::anyhow!("找不到微信应用"))?;

        // 1. 切换到目标聊天
        //    chat_with 内部会在需要切换时先聚焦主窗口
        //    缓存命中时直接跳过, 不破坏已有的输入框焦点
        let chat_result = self.chat_with(engine, to).await?;
        if chat_result.is_none() {
            return Ok((false, false, format!("未找到聊天: {to}")));
        }

        // 2. 等待 WeChat 聚焦输入框
        tokio::time::sleep(ms(300)).await;

        // 3. 粘贴消息 (xclip + Ctrl+V)
        engine.paste_text(text).await?;
        tokio::time::sleep(ms(300)).await;

        // 4. Enter 发送
        engine.press_enter().await?;
        tokio::time::sleep(ms(500)).await;

        // 5. 验证 (可跳过, 由 API 层 DB 验证替代)
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

        // 检查是否有独立窗口可用
        {
            let mut windows = self.listen_windows.lock().await;
            if let Some(chatwnd) = windows.get_mut(to) {
                if chatwnd.is_alive().await {
                    info!("🖼️ 使用独立窗口发送图片: {to}");
                    return chatwnd.send_image(engine, image_path).await;
                } else {
                    info!("🖼️ 独立窗口已失效, 移除: {to}");
                    windows.remove(to);
                    drop(windows);
                    *self.current_chat.lock().await = None;
                }
            }
        }

        // 主窗口发送
        // 强制清除缓存, 确保重新切换 (避免独立窗口偷焦点)
        *self.current_chat.lock().await = None;
        let chat_result = self.chat_with(engine, to).await?;
        if chat_result.is_none() {
            return Ok((false, false, format!("未找到聊天: {to}")));
        }

        tokio::time::sleep(ms(300)).await;

        // 粘贴图片
        engine.paste_image(image_path).await?;
        tokio::time::sleep(ms(500)).await;

        // Enter 发送
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

// is_structural_role 已移入 atspi.rs (统一搜索原语)

/// 解析单个 AT-SPI2 消息项 (公共函数, wechat/chatwnd 共用)
pub(crate) async fn parse_message_item(atspi: &AtSpi, item: &NodeRef, index: i32) -> ChatMessage {
    let role = atspi.role(item).await;
    let name = atspi.name(item).await;

    let child_count = atspi.child_count(item).await;
    let mut children = Vec::new();
    let mut has_button = false;
    let mut button_name = String::new();

    for i in 0..child_count.min(10) {
        if let Some(child) = atspi.child_at(item, i).await {
            let c_role = atspi.role(&child).await;
            let c_name = atspi.name(&child).await;

            if c_role == "push button" && !c_name.is_empty() {
                has_button = true;
                button_name = c_name.clone();
            }

            children.push(ChatMessageChild {
                role: c_role,
                name: c_name,
            });
        }
    }

    let (msg_type, sender, content) = classify_message(
        &name, &children, has_button, &button_name,
    );
    let msg_id = generate_msg_id(index, &msg_type, &sender, &content);

    ChatMessage {
        index,
        role,
        name: name.clone(),
        children,
        msg_id,
        msg_type,
        sender,
        content,
    }
}

/// 消息分类 (借鉴 wxauto _split 的逻辑)
pub(crate) fn classify_message(
    name: &str,
    children: &[ChatMessageChild],
    has_button: bool,
    button_name: &str,
) -> (String, String, String) {
    if !has_button {
        if is_time_text(name) {
            return ("time".into(), "SYS".into(), name.into());
        }
        if name.contains("撤回") || name.contains("recalled") || name.contains("revoke") {
            return ("recall".into(), "SYS".into(), name.into());
        }
        return ("sys".into(), "SYS".into(), name.into());
    }

    // 有头像按钮 = 聊天消息
    let content = extract_content(children, name);
    let sender = button_name.to_string();
    // 默认为 friend；self 判断需要知道自己的昵称或通过坐标
    let msg_type = "friend".to_string();

    (msg_type, sender, content)
}

/// 从子节点中提取消息文本
pub(crate) fn extract_content(children: &[ChatMessageChild], fallback: &str) -> String {
    for child in children {
        if (child.role == "label" || child.role == "text") && !child.name.is_empty() {
            return child.name.clone();
        }
    }
    fallback.into()
}

/// 生成稳定的消息 ID
pub(crate) fn generate_msg_id(index: i32, msg_type: &str, sender: &str, content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let index_bucket = index / 3;
    (index_bucket, msg_type, sender, content).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 判断文本是否是时间格式 (更严格: 要求冒号前后是数字)
pub(crate) fn is_time_text(text: &str) -> bool {
    let text = text.trim();
    if text.len() > 25 || text.is_empty() { return false; }
    // 数字:数字 格式 (如 "14:30", "下午 2:30", "2026/3/1 14:30")
    if text.contains(':') {
        let has_digit_colon = text.as_bytes().windows(3).any(|w| {
            w[0].is_ascii_digit() && w[1] == b':' && w[2].is_ascii_digit()
        });
        if has_digit_colon { return true; }
    }
    if text.contains("昨天") || text.contains("前天") || text.contains("星期") { return true; }
    if text.contains("年") && text.contains("月") { return true; }
    let days = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday", "Yesterday"];
    days.iter().any(|d| text.contains(d))
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
                info!("✅ 验证成功 (attempt {attempt})");
                return true;
            }
        }
    }
    false
}

pub(crate) fn ms(n: u64) -> std::time::Duration {
    std::time::Duration::from_millis(n)
}
