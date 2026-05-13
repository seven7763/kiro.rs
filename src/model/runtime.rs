//! 运行时共享配置
//!
//! 这些配置可在运行时被 Admin API 修改并即时生效（无需重启）。
//! 与 `Config` 不同，运行时配置使用 `Arc<RwLock<...>>` 在 Anthropic
//! 请求处理器和 Admin 服务之间共享。
//!
//! 当 Admin API 写入这些配置时，会同步回写到 `config.json`，确保下次重启
//! 也能保留更改。

use std::sync::Arc;

use parking_lot::RwLock;

use super::config::{Config, SystemPromptPosition, UserPreset};

/// Prompt 注入运行时配置
///
/// 字段含义：
/// - `enabled`：注入总开关；关闭后所有 preset + 自定义文本都不注入
/// - `enabled_presets`：启用的 preset id 列表（混合内置 + 用户自定义）
/// - `user_presets`：用户自定义预设清单（与内置 `PRESETS` 并列）
/// - `custom_content`：自由文本补充（追加到所有 preset 之后）
/// - `position`：拼接结果在 system role 中的插入位置
/// - `strip_system_restrictions`：是否剥离客户端发来的安全限制指令（独立开关）
#[derive(Debug, Clone)]
pub struct PromptRuntimeConfig {
    pub enabled: bool,
    pub enabled_presets: Vec<String>,
    pub user_presets: Vec<UserPreset>,
    pub custom_content: Option<String>,
    pub position: SystemPromptPosition,
    pub strip_system_restrictions: bool,
}

impl PromptRuntimeConfig {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.system_prompt_enabled,
            enabled_presets: cfg.enabled_presets.clone(),
            user_presets: cfg.user_presets.clone(),
            custom_content: cfg.system_prompt.clone(),
            position: cfg.system_prompt_position,
            strip_system_restrictions: cfg.strip_system_restrictions,
        }
    }

    /// 计算最终要注入的文本。返回 `None` 表示无需注入。
    ///
    /// 拼接顺序：
    /// 1. 内置 preset（按 `PRESETS` 数组顺序）
    /// 2. 用户 preset（按 `user_presets` 顺序）
    /// 3. `custom_content`
    /// 各段之间用空行连接。
    pub fn build_injection_text(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }

        let mut parts: Vec<String> = Vec::new();

        // 1) 内置
        for p in crate::anthropic::prompt_presets::PRESETS {
            if self.enabled_presets.iter().any(|id| id == p.id) {
                parts.push(p.content.trim().to_string());
            }
        }
        // 2) 用户
        for up in &self.user_presets {
            if self.enabled_presets.iter().any(|id| id == &up.id) {
                let trimmed = up.content.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        // 3) 自定义补充
        if let Some(c) = self.custom_content.as_deref() {
            let t = c.trim();
            if !t.is_empty() {
                parts.push(t.to_string());
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }
}

/// 跨模块共享的可变 Prompt 配置句柄
pub type SharedPromptConfig = Arc<RwLock<PromptRuntimeConfig>>;

/// 从 `Config` 构建共享句柄
pub fn shared_from_config(cfg: &Config) -> SharedPromptConfig {
    Arc::new(RwLock::new(PromptRuntimeConfig::from_config(cfg)))
}
