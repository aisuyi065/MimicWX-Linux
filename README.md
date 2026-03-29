# MimicWX-Linux 🐧

**零风险微信自动化框架** — 基于 AT-SPI2 无障碍接口 + X11 XTEST 输入注入 + SQLCipher 数据库解密

> Zero-risk WeChat automation framework for Linux via AT-SPI2 accessibility + X11 XTEST input injection + SQLCipher database decryption

---

## ✨ 特性

- 🔍 **数据库消息检测** — SQLCipher 解密 WCDB + fanotify WAL 实时监听，亚秒级延迟，支持文本/图片/语音/视频/文件/名片/位置/链接等 16+ 种消息类型结构化解析
- ⌨️ **X11 原生输入注入** — XTEST 扩展注入键鼠事件 + X11 Selection 协议直接操作剪贴板（零外部进程依赖），原生窗口管理
- 🔑 **自动密钥提取** — 进程内存扫描 + HMAC 验证，扫码登录后自动提取 32 字节 AES 密钥，支持密钥过期自动更新
- 💬 **独立聊天窗口** — 借鉴 [wxauto](https://github.com/cluic/wxauto) 的 ChatWnd 设计，支持多窗口并行收发 + 缓存节点自动失效重建
- 🔌 **REST + WebSocket API** — 完整 HTTP API + WebSocket 实时推送 (30s 心跳保活)，CORS 全开放，可对接 Yunzai 等机器人框架
- 🐳 **Docker 一键部署** — 多阶段构建 + Xvfb/VNC 虚拟桌面，开箱即用
- 🔒 **Token 认证** — 支持 Bearer Token 认证保护 API 安全
- 🖥️ **交互式控制台** — 支持 `/restart`、`/stop`、`/status`、`/refresh`、`/help` 命令，方向键切换历史
- 💡 **自动弹性** — AT-SPI2 心跳自动重连、密钥过期自愈、独立窗口弹出重试、联系人定时刷新、优雅重启/关闭

---

## 🏗️ 系统架构

```
┌─ Docker 容器 (Ubuntu 22.04) ──────────────────────────────────────────────┐
│                                                                           │
│  ┌─ 桌面环境 ────────────────────────────────────────────────────────────┐ │
│  │  Xvfb (虚拟显示 :1)  ←→  TigerVNC  ←→  noVNC (浏览器远程桌面)      │ │
│  │  XFCE4 桌面  +  WeChat Linux 版                                     │ │
│  └──────────────────────────────────────────────────────────────────────┘ │
│                                                                           │
│  ┌─ MimicWX 核心 (Rust) ────────────────────────────────────────────────┐ │
│  │                                                                       │ │
│  │  ┌── 消息检测层 ──────────────────────────────────────────────────┐   │ │
│  │  │  db.rs:    SQLCipher 解密 → fanotify WAL 监听 → 增量消息拉取  │   │ │
│  │  │  atspi.rs: D-Bus → AT-SPI2 Registry → 节点遍历/属性读取       │   │ │
│  │  └────────────────────────────────────────────────────────────────┘   │ │
│  │                                                                       │ │
│  │  ┌── 输入控制层 ──────────────────────────────────────────────────┐   │ │
│  │  │  input.rs: X11 XTEST 键鼠注入 + X11 Selection 剪贴板 + 窗口管理│   │ │
│  │  └────────────────────────────────────────────────────────────────┘   │ │
│  │                                                                       │ │
│  │  ┌── 业务逻辑层 ──────────────────────────────────────────────────┐   │ │
│  │  │  wechat.rs:  会话管理 / 消息发送 / 控件查找 / 状态检测         │   │ │
│  │  │  chatwnd.rs: 独立聊天窗口管理 (多窗口并行)                     │   │ │
│  │  └────────────────────────────────────────────────────────────────┘   │ │
│  │                                                                       │ │
│  │  ┌── API 层 ──────────────────────────────────────────────────────┐   │ │
│  │  │  api.rs: axum HTTP + WebSocket (CORS + 心跳保活)                │   │ │
│  │  │  main.rs: 启动编排 / 配置 / 消息循环 / 交互式控制台             │   │ │
│  │  └────────────────────────────────────────────────────────────────┘   │ │
│  └───────────────────────────────────────────────────────────────────────┘ │
│                                                                           │
│  ┌─ 辅助脚本 ────────────────────────────────────────────────────────────┐ │
│  │  start.sh:       容器启动编排 (D-Bus → VNC → AT-SPI2 → 微信 → 服务) │ │
│  │  extract_key.py: GDB Python 脚本 — 自动提取 WCDB 加密密钥          │ │
│  └──────────────────────────────────────────────────────────────────────┘ │
└───────────────────────────────────────────────────────────────────────────┘

┌─ 外部对接 ────────────────────────────────────────────────────────────────┐
│  adapter/MimicWX.js: Yunzai-Bot 适配器 (REST + WebSocket)                │
└───────────────────────────────────────────────────────────────────────────┘
```

---

## 📁 项目结构

```
MimicWX-Linux/
├── src/                        # Rust 源代码
│   ├── main.rs                 # 入口: 启动编排、配置加载、消息循环
│   ├── atspi.rs                # AT-SPI2 底层原语 (D-Bus 通信、节点遍历)
│   ├── input.rs                # X11 XTEST 输入引擎 (键鼠注入、窗口管理)
│   ├── wechat.rs               # 微信业务逻辑 (会话管理、消息发送/验证)
│   ├── chatwnd.rs              # 独立聊天窗口 (ChatWnd 模式)
│   ├── db.rs                   # 数据库监听 (SQLCipher + fanotify WAL)
│   └── api.rs                  # HTTP/WebSocket API (axum)
├── docker/
│   ├── start.sh                # 容器启动脚本
│   ├── extract_key.py          # GDB 密钥提取脚本
│   └── dbus-mimicwx.conf       # D-Bus 配置 (允许 eavesdrop)
├── adapter/
│   └── MimicWX.js              # Yunzai-Bot 适配器
├── Cargo.toml                  # Rust 依赖 & 构建配置
├── Dockerfile                  # 多阶段构建 (builder + runtime)
├── docker-compose.yml          # 编排配置
└── config.toml                 # 运行时配置文件
```

---

## 📦 核心模块详解

### `atspi.rs` — AT-SPI2 底层原语

通过 `zbus` 连接 AT-SPI2 D-Bus，封装节点遍历和属性读取：

| 能力 | 说明 |
|------|------|
| **多策略连接** | `org.a11y.Bus` → `AT_SPI_BUS_ADDRESS` 环境变量 → `~/.cache/at-spi/` socket 扫描 |
| **运行时重连** | Registry 持续返回 0 子节点时自动重新发现 AT-SPI2 bus |
| **节点操作** | `child_count` / `child_at` / `name` / `role` / `bbox` / `text` / `parent` / `get_states` |
| **搜索原语** | BFS 广度搜索 + DFS 深度搜索，支持 role/name 过滤 |
| **超时保护** | 所有 D-Bus 调用带 500ms 超时 |

### `input.rs` — X11 XTEST 输入引擎

通过 `x11rb` 使用 XTEST 扩展注入输入事件：

| 能力 | 说明 |
|------|------|
| **键盘** | 单键按下 / 组合键 (`Ctrl+V`, `Ctrl+A` 等) / ASCII 逐字输入 |
| **中文输入** | X11 Selection 协议直接设置剪贴板 → `Ctrl+V` 粘贴 (零外部进程) |
| **图片发送** | `xclip -selection clipboard -t image/png` → `Ctrl+V` 粘贴 |
| **鼠标** | 移动 / 单击 / 双击 / 右键 / 滚轮 |
| **窗口管理** | X11 原生 `_NET_ACTIVE_WINDOW` 激活 / `_NET_CLOSE_WINDOW` 关闭 (替代 xdotool) |

### `db.rs` — 数据库监听

SQLCipher 解密微信 WCDB 数据库 + fanotify 实时监听：

| 能力 | 说明 |
|------|------|
| **SQLCipher 解密** | `rusqlite` + `bundled-sqlcipher-vendored-openssl`，密钥过期自动检测 + 重新初始化 |
| **持久连接池** | 多个 `message_N.db` 保持长连接，避免重复解密握手 |
| **WAL 监听** | `fanotify` + PID 过滤 (只监听微信进程写入)，无需防抖 |
| **增量消息** | 每个消息表维护 `last_local_id` 高水位标记 |
| **联系人缓存** | 从 `contact.db` + `group_contact.db` 加载联系人/群成员 |
| **消息解析** | 16+ 种结构化类型：文本/图片(含尺寸)/语音(含CDN+AES)/视频(含元数据)/文件(含大小)/名片/位置/表情/链接/小程序/转账/红包/系统消息 |
| **WCDB 兼容** | Zstd BLOB 解压 + TEXT/BLOB 自适应读取 |
| **发送验证** | 订阅自发消息广播，事件驱动验证发送结果 |

### `wechat.rs` — 微信业务逻辑

基于 AT-SPI2 的微信 UI 自动化：

| 能力 | 说明 |
|------|------|
| **状态检测** | 通过 `[tool bar] "导航"` 判断登录状态 (未运行/等待扫码/已登录) |
| **控件查找** | 导航栏 / split pane / 会话列表 / 消息列表 / 输入框 |
| **会话管理** | 列表获取 / 精确匹配优先切换 / 新消息检查 / Ctrl+F 搜索回退 |
| **消息发送** | 公共方法提取 → 切换会话 → 粘贴文本 → Enter → DB 验证 |
| **图片发送** | 优先独立窗口，回退主窗口 |
| **独立窗口** | 弹出 (`add_listen`, 3 次重试 + 递增退避) / 关闭 (`remove_listen`) / 存活检测 |

### `chatwnd.rs` — 独立聊天窗口

每个独立弹出的聊天窗口拥有独立的 AT-SPI2 节点：

| 能力 | 说明 |
|------|------|
| **窗口管理** | 创建 / 存活检查 / 销毁 |
| **缓存失效重建** | 输入框/消息列表节点使用前 bbox 校验，失效自动重搜 |
| **消息发送** | 激活窗口 → 发送文本/图片 → 验证 |

### `api.rs` — HTTP + WebSocket API

基于 `axum` 的 REST API + WebSocket 实时推送：

| 端点 | 方法 | 说明 |
|------|------|------|
| `/status` | GET | 服务状态 + DB/联系人/运行时间 (免认证) |
| `/contacts` | GET | 联系人列表 (数据库) |
| `/sessions` | GET | 会话列表 (优先数据库) |
| `/messages/new` | GET | 新消息 (数据库增量) |
| `/send` | POST | 发送文本消息 |
| `/send_image` | POST | 发送图片 (base64) |
| `/chat` | POST | 切换聊天目标 |
| `/listen` | POST | 添加/查看监听目标 |
| `/listen` | DELETE | 移除监听目标 |
| `/command` | POST | 通用命令执行 (微信互通) |
| `/ws` | GET | WebSocket 实时消息推送 |
| `/debug/tree` | GET | AT-SPI2 控件树 (调试) |
| `/debug/session_tree` | GET | 会话容器树 (调试) |

> 认证方式: `Header "Authorization: Bearer <token>"` 或 `Query "?token=<token>"`

---

## 🚀 快速开始

### 环境要求

- Linux 系统 (Ubuntu 22.04+ 推荐)
- Docker + Docker Compose
- 允许 `SYS_ADMIN` / `SYS_PTRACE` 能力 (密钥提取 + fanotify 需要)

### 一键部署

```bash
git clone https://github.com/PigeonCoders/MimicWX-Linux.git
cd MimicWX-Linux
docker compose up -d
```

### 或手动 Docker 构建

```bash
docker build -t mimicwx .
docker run -d --name mimicwx \
  --cap-add SYS_ADMIN \
  --cap-add SYS_PTRACE \
  --security-opt seccomp=unconfined \
  --security-opt apparmor=unconfined \
  -p 5901:5901 \
  -p 6080:6080 \
  -p 8899:8899 \
  --shm-size 512m \
  mimicwx
```

### 首次使用

1. 打开 noVNC: `http://HOST:6080/vnc.html` (密码: `mimicwx`)
2. 在虚拟桌面中扫码登录微信
3. GDB 自动提取数据库密钥 → MimicWX 自动启动
4. 通过 API 接口开始使用

### 访问入口

| 服务 | 地址 | 说明 |
|------|------|------|
| noVNC | `http://HOST:6080/vnc.html` | 浏览器远程桌面 (密码: `mimicwx`) |
| VNC | `vnc://HOST:5901` | VNC 客户端连接 |
| API | `http://HOST:8899` | REST API 接口 |
| WebSocket | `ws://HOST:8899/ws` | 实时消息推送 |

---

## ⚙️ 配置文件

`config.toml` — 配置搜索优先级: `./config.toml` → `/home/wechat/mimicwx-linux/config.toml` → `/etc/mimicwx/config.toml`

```toml
[api]
# API 认证 Token (留空则不启用认证)
# 请求方式: Header "Authorization: Bearer <token>" 或 Query "?token=<token>"
token = "your-secret-token"

[listen]
# 启动后自动弹出独立窗口并监听的对象
# 填入联系人名称或群名称 (与微信显示名一致)
auto = ["文件传输助手", "好友A", "工作群"]
```

---

## 🔧 对接 Yunzai-Bot

项目内置 Yunzai-Bot v3 适配器 (`adapter/MimicWX.js`)，支持：

- WebSocket 实时消息接收
- 自动解析数据库消息 (文本/图片/语音/视频/表情/链接)
- 智能消息分段发送 (文本 + 图片分离)
- 私聊/群聊消息路由
- 好友/群列表自动同步

```bash
# 环境变量
export MIMICWX_URL="http://localhost:8899"      # API 地址
export MIMICWX_TOKEN="your-secret-token"         # 认证 Token
```

---

## 🔑 密钥提取原理

```
WeChat 进程启动
      │
      ▼
extract_key.py (root 后台, start.sh 自动启动)
      │
      ├── 已有密钥? → HMAC 验证 → 有效 → 跳过提取 (秒退)
      │                          → 无效 → 继续等待
      ▼
扫描 /proc/<PID>/maps + mem (无限等待用户扫码)
      │
      ▼
提取 32 字节 AES 密钥 → HMAC 验证 → 保存至 wechat_key.txt + wechat_keys.json
      │
      ▼
MimicWX 检测密钥文件 → 解密数据库 → 密钥过期时监控 mtime 自动重新初始化
```

> 💡 密钥提取基于进程内存扫描 + HMAC 验证，不依赖特定偏移量，微信版本升级无需更新。

---

## 🛠️ 技术栈

| 组件 | 技术 | 说明 |
|------|------|------|
| 语言 | **Rust** | 异步高性能，零运行时开销 |
| 异步运行时 | **Tokio** | 全功能异步运行时 |
| 消息检测 | **SQLCipher** + **fanotify** | 数据库解密 + WAL 实时监听 |
| UI 自动化 | **AT-SPI2** (`atspi-rs` + `zbus`) | D-Bus 无障碍接口控制 |
| 输入注入 | **X11 XTEST** (`x11rb`) | 原生 X11 扩展 + Selection 剪贴板 |
| API 服务 | **axum** | HTTP + WebSocket |
| 序列化 | **serde** + **serde_json** | JSON 序列化/反序列化 |
| XML 解析 | **quick-xml** | 微信消息 XML 解析 |
| 压缩 | **zstd** | WCDB Zstd BLOB 解压 |
| 容器化 | **Docker** (Ubuntu 22.04) | 多阶段构建 |
| 虚拟桌面 | **TigerVNC** + **noVNC** | 远程桌面访问 |
| 密钥提取 | **GDB** + **Python** | 运行时内存断点 |

---

## 📊 启动流程

```
容器启动 (start.sh)
 ├── 0) 系统服务: D-Bus daemon + ptrace 设置 + 权限修复
 ├── 1) D-Bus session bus
 ├── 2) VNC + XFCE 桌面 (1280×720)
 ├── 3) 清理 XFCE 自启的 AT-SPI2 (避免 bus 冲突)
 ├── 4) 启动唯一的 AT-SPI2 bus
 ├── 5) 获取 AT-SPI2 bus 地址 → 保存环境变量
 ├── 6) 启动微信 → 等待窗口就绪
 ├── GDB 密钥提取 (后台, 等待用户扫码)
 ├── 7) noVNC (websockify)
 └── 8) MimicWX 主服务
      ├── AT-SPI2 连接 (带重试)
      ├── X11 XTEST 输入引擎
      ├── 等待微信登录
      ├── 读取密钥 → DbManager 初始化
      ├── InputEngine Actor (mpsc 队列)
      ├── API 服务 (axum :8899)
      ├── 数据库消息监听任务 (fanotify)
      └── 自动监听任务 (config.toml auto)
```

---

## 📝 API 使用示例

### 查询状态
```bash
curl http://localhost:8899/status
```

### 发送消息
```bash
curl -X POST http://localhost:8899/send \
  -H "Authorization: Bearer your-token" \
  -H "Content-Type: application/json" \
  -d '{"to": "文件传输助手", "text": "Hello from MimicWX!"}'
```

### 发送图片 (base64)
```bash
curl -X POST http://localhost:8899/send_image \
  -H "Authorization: Bearer your-token" \
  -H "Content-Type: application/json" \
  -d '{"to": "文件传输助手", "file": "<base64-data>", "name": "test.png"}'
```

### 添加监听
```bash
curl -X POST http://localhost:8899/listen \
  -H "Authorization: Bearer your-token" \
  -H "Content-Type: application/json" \
  -d '{"who": "好友A"}'
```

### WebSocket 连接
```javascript
const ws = new WebSocket("ws://localhost:8899/ws?token=your-token")
ws.onmessage = (e) => console.log(JSON.parse(e.data))
```

---

## 🖥️ 控制台命令

通过 `docker attach mimicwx-linux` 进入交互式控制台：

```
> /help
```

| 命令 | 功能 |
|------|------|
| `/restart` | 优雅重启程序 |
| `/stop` | 正常关闭程序 |
| `/status` | 显示运行时状态 |
| `/refresh` | 手动刷新联系人缓存 |
| `/reload` | 热重载配置文件 |
| `/atmode` | 切换仅@模式 |
| `/send <收件人> <内容>` | 发送消息 |
| `/listen <名称>` | 添加监听 (自动写入 config.toml) |
| `/unlisten <名称>` | 移除监听 (自动写入 config.toml) |
| `/sessions` | 查看会话列表 |
| `/help` | 显示帮助 |

**快捷键**: `↑↓` 历史命令 · `←→` 移动光标 · `Ctrl+U` 清行 · `Ctrl+L` 清屏

> 退出控制台但不停止容器: `Ctrl+P` 然后 `Ctrl+Q`

---

## 📋 更新日志

### v0.5.2

- 📊 **消息类型结构化升级** — Image/Voice/Video 扩展完整元数据 (CDN URL、AES 密钥、尺寸、文件大小)
- 📎 **新增 File 类型** — 从 App 中独立出文件消息，支持 app_type=6/74 + totallen 组合判定
- 👤 **新增 ContactCard 类型** — 名片消息独立解析 (nickname/username/avatar_url)
- 📍 **新增 Location 类型** — 位置消息解析 (经纬度/名称/地址)
- 🔑 **密钥生命周期自愈** — 密钥过期自动检测 + 监控 mtime 变化 + 重新初始化 DbManager
- 🔄 **add_listen 重试机制** — 独立窗口弹出失败时最多重试 3 次 (递增退避 1s/1.5s/2s)
- 🔍 **extract_key.py 增强** — HMAC 验证快速跳过 + 移除超时限制 (无限等待扫码)
- 🔌 **AT-SPI 定期重连** — 等待登录时每 15s 尝试重连 (防止容器重启后 bus 地址变化)
- 🐳 **start.sh 健壮性** — 始终启动密钥提取 + /restart 时清除旧密钥重新提取

### v0.5.1

- 📡 **微信互通命令** — 主人可通过微信私聊 `#` 命令远程控制 Bot (复用 Yunzai 主人系统)
- 🔄 **配置热重载** — `/reload` 命令重读 config.toml，自动 diff 监听列表并增删
- 💾 **监听持久化** — `/listen` `/unlisten` 自动写入 config.toml，重启不丢失
- 🎮 **控制台命令扩展** — 新增 `/send`、`/listen`、`/unlisten`、`/sessions`、`/reload`、`/atmode`
- 🔧 **独立窗口自动恢复** — 发送消息时检测窗口失效自动重建
- ⚡ **AT-SPI2 轮询替代固定延迟** — 会话切换、搜索、独立窗口弹出改用状态轮询
- ⚙️ **@ 延迟可配置** — `config.toml` 新增 `[timing].at_delay_ms`，支持热更新

### v0.5.0

- ♻️ **移除 AT-SPI 消息读取** — 消息检测全面转向数据库通道，更稳定更高效
- 🔧 **send_message/send_image 公共方法提取** — `check_listen_window` + `prepare_main_send` 减少代码重复
- 🎯 **会话精确匹配优先** — `find_session` 改为精确 > starts_with > contains 优先级策略
- 🔄 **ChatWnd 缓存自动刷新** — 输入框/消息列表节点使用前 bbox 校验，失效自动重新搜索
- ⚡ **X11 Selection 剪贴板** — 文本粘贴改用 X11 Selection 协议，消除 xclip 进程开销
- 🧹 **适配器清理** — 删除 AT-SPI 消息处理器死代码，简化 DB 验证日志

---

## License

MIT
