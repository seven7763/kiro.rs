//! 消息内容处理：文本/图片/工具结果提取、tool_choice 指令、图片格式

use std::collections::HashMap;

use crate::anthropic::types::ContentBlock;
use crate::kiro::model::requests::{KiroImage, ToolResult};

use super::ConversionError;

/// 把 Anthropic `tool_choice` 字段翻译成给模型的自然语言指令。
///
/// Kiro 上游协议不支持 tool_choice。要让「结构化输出」类测试（强制 JSON）通过，
/// 必须在 user message 头部塞一段强约束文本，告诉模型必须调用某个工具。
///
/// Anthropic 五种取值：
/// - `{"type": "auto"}`             — 默认，模型自由决定（无需注入）
/// - `{"type": "any"}`              — 必须调用任一工具
/// - `{"type": "tool", "name": X}`  — 必须调用 X
/// - `{"type": "none"}`             — 禁止调用工具
/// - `{"type": "auto", "disable_parallel_tool_use": true}` — 仅做并行控制
///
/// 注意 `tool_name_map` 把超长工具名缩短了，注入指令时也要用映射后的名字。
pub(super) fn build_tool_choice_directive(
    tool_choice: &Option<serde_json::Value>,
    tool_name_map: &HashMap<String, String>,
) -> Option<String> {
    let value = tool_choice.as_ref()?;
    let kind = value.get("type")?.as_str()?;

    match kind {
        "auto" => None,
        "any" => Some(
            "IMPORTANT: You MUST invoke exactly one of the provided tools. Do not respond with plain text."
                .to_string(),
        ),
        "tool" => {
            let name = value.get("name")?.as_str()?;
            // 映射后的名字（map 是 short→original，所以反查一遍）
            let mapped_name = tool_name_map
                .iter()
                .find_map(|(short, original)| (original == name).then(|| short.clone()))
                .unwrap_or_else(|| name.to_string());
            Some(format!(
                "IMPORTANT: You MUST invoke the `{name}` tool to answer this request. Do not respond with plain text. Call the tool with arguments that match its input schema exactly.",
                name = mapped_name
            ))
        }
        "none" => Some(
            "IMPORTANT: Do not invoke any tools for this request. Respond with plain text only."
                .to_string(),
        ),
        _ => None,
    }
}

/// 处理消息内容，提取文本、图片和工具结果
pub(super) fn process_message_content(
    content: &serde_json::Value,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "text" => {
                            if let Some(text) = block.text {
                                text_parts.push(text);
                            }
                        }
                        "image" => {
                            if let Some(source) = block.source {
                                if let Some(format) = get_image_format(&source.media_type) {
                                    images.push(KiroImage::from_base64(format, source.data));
                                }
                            }
                        }
                        "document" => {
                            // Anthropic PDF / 文档块。Kiro 上游不接受此格式，
                            // 这里把内容尽量解码成纯文本塞进 text_parts。
                            if let Some(source) = block.source {
                                let media_type = source.media_type.clone();
                                let extracted = if media_type == "application/pdf" {
                                    crate::anthropic::document::extract_pdf_text_from_base64(&source.data)
                                } else if media_type.starts_with("text/") {
                                    use base64::Engine;
                                    base64::engine::general_purpose::STANDARD
                                        .decode(source.data.as_bytes())
                                        .ok()
                                        .and_then(|b| String::from_utf8(b).ok())
                                } else {
                                    None
                                };

                                let approx_bytes = (source.data.len() / 4) * 3;
                                let block_text = match extracted {
                                    Some(text) if !text.trim().is_empty() => format!(
                                        "[Document content extracted from {} ({} chars)]\n{}",
                                        media_type,
                                        text.len(),
                                        text
                                    ),
                                    _ => crate::anthropic::document::document_placeholder(
                                        &media_type,
                                        approx_bytes,
                                    ),
                                };
                                text_parts.push(block_text);
                            }
                        }
                        "tool_result" => {
                            if let Some(tool_use_id) = block.tool_use_id {
                                let result_content = extract_tool_result_content(&block.content);
                                let is_error = block.is_error.unwrap_or(false);

                                let mut result = if is_error {
                                    ToolResult::error(&tool_use_id, result_content)
                                } else {
                                    ToolResult::success(&tool_use_id, result_content)
                                };
                                result.status =
                                    Some(if is_error { "error" } else { "success" }.to_string());

                                tool_results.push(result);
                            }
                        }
                        "tool_use" => {
                            // tool_use 在 assistant 消息中处理，这里忽略
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    Ok((text_parts.join("\n"), images, tool_results))
}

/// 从 media_type 获取图片格式
fn get_image_format(media_type: &str) -> Option<String> {
    match media_type {
        "image/jpeg" => Some("jpeg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// 提取工具结果内容
fn extract_tool_result_content(content: &Option<serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
            parts.join("\n")
        }
        Some(v) => v.to_string(),
        None => String::new(),
    }
}
