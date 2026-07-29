//! Token 计算模块
//!
//! 提供文本 token 数量计算功能。
//!
//! # 计算规则
//! - 非西文字符：每个计 4.0 个字符单位
//! - 西文字符：每个计 1 个字符单位
//! - 4 个字符单位 = 1 token（四舍五入），再按 token 数量分档放大系数补偿

use super::types::{CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;

/// Count Tokens API 配置
#[derive(Clone, Default)]
pub struct CountTokensConfig {
    /// 外部 count_tokens API 地址
    pub api_url: Option<String>,
    /// count_tokens API 密钥
    pub api_key: Option<String>,
    /// count_tokens API 认证类型（"x-api-key" 或 "bearer"）
    pub auth_type: String,
    /// 代理配置
    pub proxy: Option<ProxyConfig>,

    pub tls_backend: TlsBackend,
}

/// 全局配置存储
static COUNT_TOKENS_CONFIG: OnceLock<CountTokensConfig> = OnceLock::new();

/// 西文字符 token 估算权重（见 [`crate::model::config::Config::token_western_char_weight`]）。
/// 启动时由 [`init_western_char_weight`] 设置；未设置时回退默认 1.85。
static WESTERN_CHAR_WEIGHT: OnceLock<f64> = OnceLock::new();

/// 默认西文权重，与 `config::default_western_char_weight` 同源（1.85）。
const DEFAULT_WESTERN_CHAR_WEIGHT: f64 = 1.85;

/// 初始化西文字符权重（应在启动时调用一次）。
///
/// clamp 到 `[0.1, 4.0]`：下限防误配 0 把英文压成 0 token，上限 4.0 等于 CJK 权重
/// （西文不应比 CJK 还重）。
pub fn init_western_char_weight(weight: f64) {
    let clamped = weight.clamp(0.1, 4.0);
    let _ = WESTERN_CHAR_WEIGHT.set(clamped);
}

/// 取当前西文权重，未初始化时回退默认。
fn western_char_weight() -> f64 {
    *WESTERN_CHAR_WEIGHT
        .get()
        .unwrap_or(&DEFAULT_WESTERN_CHAR_WEIGHT)
}

/// 初始化 count_tokens 配置
///
/// 应在应用启动时调用一次
pub fn init_config(config: CountTokensConfig) {
    let _ = COUNT_TOKENS_CONFIG.set(config);
}

/// 获取配置
fn get_config() -> Option<&'static CountTokensConfig> {
    COUNT_TOKENS_CONFIG.get()
}

/// 安全将 `u64` token 计数转换为 `i32`，超出范围时饱和到 `i32::MAX`
///
/// [`count_all_tokens`] 返回 `u64`，但下游 SSE 协议、context window 计算和
/// `CountTokensResponse` 都用 `i32`。直接 `as i32` 在极端大请求下会 wrap 成负数
/// 或被截断。此函数保证结果始终在 `[0, i32::MAX]` 范围内。
pub(crate) fn saturating_to_i32(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// 判断字符是否为非西文字符
///
/// 西文字符包括：
/// - ASCII 字符 (U+0000..U+007F)
/// - 拉丁字母扩展 (U+0080..U+024F)
/// - 拉丁字母扩展附加 (U+1E00..U+1EFF)
///
/// 返回 true 表示该字符是非西文字符（如中文、日文、韩文、阿拉伯文等）
fn is_non_western_char(c: char) -> bool {
    !matches!(c,
        // 基本 ASCII
        '\u{0000}'..='\u{007F}' |
        // 拉丁字母扩展-A (Latin Extended-A)
        '\u{0080}'..='\u{00FF}' |
        // 拉丁字母扩展-B (Latin Extended-B)
        '\u{0100}'..='\u{024F}' |
        // 拉丁字母扩展附加 (Latin Extended Additional)
        '\u{1E00}'..='\u{1EFF}' |
        // 拉丁字母扩展-C/D/E
        '\u{2C60}'..='\u{2C7F}' |
        '\u{A720}'..='\u{A7FF}' |
        '\u{AB30}'..='\u{AB6F}'
    )
}

/// 计算文本的 token 数量
///
/// # 计算规则
/// - 非西文字符（CJK 等）：每个计 4.0 个字符单位
/// - 西文字符：每个计 `western_char_weight()` 个字符单位（默认 1.85，可配）
/// - 4 个字符单位 = 1 token，再按 token 数量分档乘以补偿系数
///   （<100→×1.5, <200→×1.3, <300→×1.25, <800→×1.2, ≥800→×1.0）
///
/// 西文权重默认 1.85 而非历史的 1.0：实测纯英文真实约 2.1 字符/token，旧的
/// 4 字符=1 token 会把英文低估约 1.86×，导致 input/cache_read 系统性少算近半。
pub fn count_tokens(text: &str) -> u64 {
    let western_weight = western_char_weight();
    let char_units: f64 = text
        .chars()
        .map(|c| {
            if is_non_western_char(c) {
                4.0
            } else {
                western_weight
            }
        })
        .sum();

    let tokens = char_units / 4.0;

    (if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    }) as u64
}

/// 估算请求的输入 tokens
///
/// 优先调用远程 API，失败时回退到本地计算
pub(crate) fn count_all_tokens(
    model: String,
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    // 检查是否配置了远程 API
    if let Some(config) = get_config() {
        if let Some(api_url) = &config.api_url {
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    api_url, config, model, &system, &messages, &tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!("远程 count_tokens API 返回: {}", tokens);
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!("远程 count_tokens API 调用失败，回退到本地计算: {}", e);
                }
            }
        }
    }

    // 本地计算
    count_all_tokens_local(system, messages, tools)
}

/// 调用远程 count_tokens API
async fn call_remote_count_tokens(
    api_url: &str,
    config: &CountTokensConfig,
    model: String,
    system: &Option<Vec<SystemMessage>>,
    messages: &[Message],
    tools: &Option<Vec<Tool>>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = build_client(config.proxy.as_ref(), 300, config.tls_backend)?;

    // 构建请求体
    let request = CountTokensRequest {
        model, // 模型名称用于 token 计算
        messages: messages.to_vec(),
        system: system.clone(),
        tools: tools.clone(),
    };

    // 构建请求
    let mut req_builder = client.post(api_url);

    // 设置认证头
    if let Some(api_key) = &config.api_key {
        if config.auth_type == "bearer" {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            req_builder = req_builder.header("x-api-key", api_key);
        }
    }

    // 发送请求
    let response = req_builder
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("API 返回错误状态: {}", response.status()).into());
    }

    let result: CountTokensResponse = response.json().await?;
    Ok(result.input_tokens as u64)
}

/// 本地计算请求的输入 tokens
fn count_all_tokens_local(
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    let mut total = 0;

    // 系统消息
    if let Some(ref system) = system {
        for msg in system {
            total += count_tokens(&msg.text);
        }
    }

    // 用户消息
    for msg in &messages {
        if let serde_json::Value::String(s) = &msg.content {
            total += count_tokens(s);
        } else if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    total += count_tokens(text);
                }
            }
        }
    }

    // 工具定义
    if let Some(ref tools) = tools {
        for tool in tools {
            total += count_tokens(&tool.name);
            total += count_tokens(&tool.description);
            let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            total += count_tokens(&input_schema_json);
        }
    }

    total.max(1)
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            total += count_tokens(text) as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            // 工具调用开销
            if let Some(input) = block.get("input") {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                total += count_tokens(&input_str) as i32;
            }
        }
    }

    total.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturating_to_i32_normal() {
        assert_eq!(saturating_to_i32(0), 0);
        assert_eq!(saturating_to_i32(100), 100);
        assert_eq!(saturating_to_i32(1_000_000), 1_000_000);
    }

    #[test]
    fn saturating_to_i32_max_boundary() {
        assert_eq!(saturating_to_i32(i32::MAX as u64), i32::MAX);
        // 边界 +1 应饱和
        assert_eq!(saturating_to_i32(i32::MAX as u64 + 1), i32::MAX);
    }

    #[test]
    fn saturating_to_i32_overflow_saturates() {
        // 之前的 `as i32` 会把 u64::MAX wrap 成 -1（i32 视图）
        // saturating 版本应饱和到 i32::MAX，永不为负
        assert_eq!(saturating_to_i32(u64::MAX), i32::MAX);
        assert!(saturating_to_i32(u64::MAX) >= 0, "结果不应为负");
        // 模拟大请求场景
        assert_eq!(saturating_to_i32(5_000_000_000), i32::MAX);
    }

    /// 校准回归：实测点 19200 英文字符 → Anthropic 真实 8944 token。
    /// 默认西文权重 1.85 下，本地估算应落在真实值 ±8% 内（旧的 1.0 权重只有 ~4800，差 1.86×）。
    ///
    /// 注：权重是进程级 OnceLock，测试未显式 init 时 `western_char_weight()` 回退默认 1.85，
    /// 与生产默认一致；此处直接断言默认行为。
    #[test]
    fn english_magnitude_calibrated_to_real_anthropic_count() {
        let text = "a".repeat(19_200);
        let est = count_tokens(&text);
        // 19200 × 1.85 / 4 = 8880（≥800 档 ×1.0）→ 贴近真实 8944
        let real = 8944.0;
        let err = (est as f64 - real).abs() / real;
        assert!(
            err < 0.08,
            "英文估算 {est} 应在真实 {real} 的 ±8% 内（误差 {:.1}%）",
            err * 100.0
        );
    }

    /// 旧权重 1.0 的低估必须被新默认显著改善：同文本新估算应 ≥ 旧估算的 1.7×。
    #[test]
    fn default_weight_fixes_english_undercount() {
        let text = "hello world ".repeat(2000); // ~24000 西文字符
        let est_default = count_tokens(&text);
        // 手算旧 1.0 权重：char_units = len×1.0，tokens = /4，≥800 档 ×1.0
        let old_est = (text.chars().count() as f64 / 4.0) as u64;
        assert!(
            est_default as f64 >= old_est as f64 * 1.7,
            "新默认估算 {est_default} 应 ≥ 旧估算 {old_est} 的 1.7×"
        );
    }

    /// CJK 不受西文权重影响：纯中文估算口径不变（无 CJK 实测数据，不动）。
    #[test]
    fn cjk_unaffected_by_western_weight() {
        let text = "测试".repeat(500); // 1000 个 CJK 字符
        // CJK 权重恒 4.0：char_units = 1000×4 = 4000，tokens = 1000，≥800 档 ×1.0
        assert_eq!(count_tokens(&text), 1000);
    }
}
