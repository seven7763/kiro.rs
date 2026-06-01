//! `/v1/models` 模型列表数据
//!
//! 从 handlers.rs 抽出：上游模型映射 + 内置静态回退列表。
//! `get_models` handler 仍在 handlers.rs，调用此处的 [`models_from_upstream`] / [`static_models`]。

use super::types::Model;

/// 用上游真实元数据直接构造 `/v1/models` 列表。
///
/// 上游 [`crate::kiro::provider::UpstreamModel`] 已带 `modelId`/`modelName`/`maxOutputTokens`，
/// 直接映射为 Anthropic 格式的 [`Model`]。对支持 thinking 的 Claude 系模型额外追加 `-thinking`
/// 变体（沿用 kiro-rs 私有约定）。`max_tokens` 用上游 `maxOutputTokens` 真值（如 Opus 4.7 =
/// 128000），缺失时回退 64000。
pub(crate) fn models_from_upstream(
    upstream: &[crate::kiro::provider::UpstreamModel],
) -> Vec<Model> {
    // 上游已逝时间戳无意义，统一用一个固定 created（不影响客户端使用）
    const CREATED: i64 = 1778400000;
    let mut out = Vec::with_capacity(upstream.len() * 2);
    for m in upstream {
        // `auto` 是路由别名，不作为可选模型对外暴露
        if m.model_id == "auto" {
            continue;
        }
        let max_tokens = m.max_output_tokens.unwrap_or(64000);
        let is_claude = m.model_id.starts_with("claude");
        out.push(Model {
            id: m.model_id.clone(),
            object: "model".to_string(),
            created: CREATED,
            owned_by: if is_claude {
                "anthropic".to_string()
            } else {
                "kiro".to_string()
            },
            display_name: m.model_name.clone(),
            model_type: "chat".to_string(),
            max_tokens,
        });
        // Claude 系支持 thinking：追加 -thinking 变体
        if is_claude {
            out.push(Model {
                id: format!("{}-thinking", m.model_id),
                object: "model".to_string(),
                created: CREATED,
                owned_by: "anthropic".to_string(),
                display_name: format!("{} (Thinking)", m.model_name),
                model_type: "chat".to_string(),
                max_tokens,
            });
        }
    }
    if out.is_empty() { static_models() } else { out }
}

/// 内置静态模型列表（上游获取失败时的回退，也是过滤的元数据来源）。
pub(crate) fn static_models() -> Vec<Model> {
    vec![
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1778400000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1778400000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_models_map_to_anthropic_format() {
        use crate::kiro::provider::UpstreamModel;
        let mk = |id: &str, name: &str, max_out: Option<i32>| UpstreamModel {
            model_id: id.to_string(),
            model_name: name.to_string(),
            max_output_tokens: max_out,
        };
        let upstream = vec![
            mk("auto", "Auto", Some(64000)),
            mk("claude-opus-4.7", "Claude Opus 4.7", Some(128000)),
            mk("deepseek-3.2", "Deepseek v3.2", Some(64000)),
        ];
        let models = models_from_upstream(&upstream);
        // auto 被剔除；claude 有 thinking 变体；deepseek 无 thinking
        assert!(!models.iter().any(|m| m.id == "auto"));
        let opus = models.iter().find(|m| m.id == "claude-opus-4.7").unwrap();
        assert_eq!(opus.max_tokens, 128000, "用上游 maxOutputTokens 真值");
        assert_eq!(opus.owned_by, "anthropic");
        assert!(
            models.iter().any(|m| m.id == "claude-opus-4.7-thinking"),
            "claude 应有 thinking 变体"
        );
        let ds = models.iter().find(|m| m.id == "deepseek-3.2").unwrap();
        assert_eq!(ds.owned_by, "kiro");
        assert!(
            !models.iter().any(|m| m.id == "deepseek-3.2-thinking"),
            "非 claude 不应有 thinking 变体"
        );
    }

    #[test]
    fn upstream_empty_falls_back_to_static() {
        let models = models_from_upstream(&[]);
        assert_eq!(models.len(), static_models().len());
    }
}
