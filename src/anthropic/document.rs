//! PDF / document 块的文本提取
//!
//! Kiro 上游不接受 Anthropic 的 `document` 块格式，所以我们把客户端发来的
//! PDF base64 解码 + 抽文字，再当成普通 text 塞进 user message。
//! 这样：
//! 1. PDF 内容真的进了对话上下文，不会被静默丢弃
//! 2. 检测「PDF 文档识别」的探针（通常会问「这份 PDF 第二页讲了什么」）能拿到答案
//! 3. 失败时退化成 placeholder，模型至少知道用户**发过** PDF

use base64::{Engine, engine::general_purpose::STANDARD};

const MAX_PDF_BYTES: usize = 32 * 1024 * 1024; // 32MB
const MAX_EXTRACTED_CHARS: usize = 200_000;

/// 从 base64 字符串提取 PDF 文本，失败返回 None
pub fn extract_pdf_text_from_base64(b64: &str) -> Option<String> {
    // base64 偶尔带换行/空白
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD.decode(cleaned.as_bytes()).ok()?;
    if bytes.is_empty() || bytes.len() > MAX_PDF_BYTES {
        return None;
    }
    extract_pdf_text(&bytes)
}

/// 从原始 PDF 字节流提取文本
pub fn extract_pdf_text(bytes: &[u8]) -> Option<String> {
    let doc = lopdf::Document::load_mem(bytes).ok()?;
    let page_count = doc.get_pages().len() as u32;
    if page_count == 0 {
        return None;
    }

    let mut out = String::new();
    for page_num in 1..=page_count {
        if let Ok(page_text) = doc.extract_text(&[page_num]) {
            if !page_text.trim().is_empty() {
                out.push_str(&format!("\n--- Page {} ---\n", page_num));
                out.push_str(page_text.trim());
                out.push('\n');
            }
        }
        if out.len() >= MAX_EXTRACTED_CHARS {
            out.push_str("\n[...truncated...]\n");
            break;
        }
    }

    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 从任意 base64 编码块返回一个安全的 placeholder，
/// 用于无法解析 PDF 的情况（受损文件、扫描件、版本不支持等）。
pub fn document_placeholder(media_type: &str, byte_len: usize) -> String {
    format!(
        "[Attached document of type {} ({} bytes). Inline text could not be extracted server-side; treat as opaque user-provided document.]",
        media_type, byte_len
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_contains_size_and_type() {
        let s = document_placeholder("application/pdf", 12345);
        assert!(s.contains("application/pdf"));
        assert!(s.contains("12345"));
    }

    #[test]
    fn invalid_base64_returns_none() {
        assert!(extract_pdf_text_from_base64("not!base64!!!").is_none());
    }

    #[test]
    fn empty_pdf_returns_none() {
        assert!(extract_pdf_text(&[]).is_none());
    }
}
