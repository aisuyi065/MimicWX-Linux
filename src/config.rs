//! 配置文件管理
//!
//! 配置文件搜索路径 (按优先级):
//! 1. `./config.toml`
//! 2. `/home/wechat/mimicwx-linux/config.toml`
//! 3. `/etc/mimicwx/config.toml`

use serde::Deserialize;
use std::path::PathBuf;
use tracing::{info, warn};

// =====================================================================
// 配置结构体
// =====================================================================

#[derive(Debug, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub listen: ListenConfig,
    #[serde(default)]
    pub timing: TimingConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct ApiConfig {
    /// API 认证 Token (留空或不配置则不启用认证)
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListenConfig {
    /// 启动后自动弹出独立窗口并监听的对象
    #[serde(default)]
    pub auto: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TimingConfig {
    /// @ 输入流程中每步的等待时间 (毫秒)
    #[serde(default = "default_at_delay")]
    pub at_delay_ms: u64,
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self { at_delay_ms: 300 }
    }
}

fn default_at_delay() -> u64 { 300 }

// =====================================================================
// 配置加载与保存
// =====================================================================

/// 加载配置文件 (搜索多个路径)
/// 返回 (配置, 配置文件路径)
pub fn load_config() -> (AppConfig, Option<PathBuf>) {
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
                        info!("[config] 配置文件已加载: {}", path.display());
                        return (config, Some(path.clone()));
                    }
                    Err(e) => {
                        warn!("[warn] 配置文件解析失败: {} - {}", path.display(), e);
                    }
                },
                Err(e) => {
                    warn!("[warn] 配置文件读取失败: {} - {}", path.display(), e);
                }
            }
        }
    }
    info!("[config] 未找到配置文件, 使用默认配置");
    (AppConfig::default(), None)
}

/// 保存监听列表到 config.toml (仅替换 auto = [...] 行, 保留注释和格式)
pub fn save_listen_list(config_path: &std::path::Path, listen_list: &[String]) {
    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(e) => {
            warn!("[warn] 无法读取配置文件: {e}");
            return;
        }
    };

    // 构造新的 auto 行 (横排格式, 与用户原始风格一致)
    let new_auto = if listen_list.is_empty() {
        "auto = []".to_string()
    } else {
        let items: Vec<_> = listen_list.iter().map(|s| format!("\"{}\"", s)).collect();
        format!("auto = [{}]", items.join(","))
    };

    // 逐行扫描, 找到非注释的 auto = [...] 行并替换
    // (跳过 # 开头的注释行, 避免误匹配 "# 示例: auto = [...]")
    let mut new_lines: Vec<String> = Vec::new();
    let mut found = false;
    let mut skip_continuation = false; // 跨行数组: 跳过后续行直到 ]
    for line in content.lines() {
        if skip_continuation {
            if line.contains(']') {
                skip_continuation = false;
            }
            continue; // 跳过跨行数组的中间行
        }
        let trimmed = line.trim();
        if !trimmed.starts_with('#') && trimmed.starts_with("auto") && trimmed.contains('=') {
            // 这是真正的 auto = [...] 行
            if trimmed.contains('[') && !trimmed.contains(']') {
                // 跨行数组: auto = [\n  "a",\n  "b",\n]
                skip_continuation = true;
            }
            new_lines.push(new_auto.clone());
            found = true;
        } else {
            new_lines.push(line.to_string());
        }
    }
    let new_content = if found {
        new_lines.join("\n")
    } else {
        // 没有 auto 行, 在 [listen] 段后追加
        content.replace("[listen]", &format!("[listen]\n{}", new_auto))
    };

    match std::fs::write(config_path, new_content) {
        Ok(_) => info!("[config] 监听列表已保存到 {}", config_path.display()),
        Err(e) => warn!("[warn] 保存配置失败: {e}"),
    }
}
