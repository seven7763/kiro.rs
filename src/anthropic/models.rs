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
/// 把 Kiro 上游的 Claude model ID 规范化成 Anthropic 惯用的连字符形式。
///
/// 上游用点号表示小版本（实测 `/listAvailableModels` 返回 `claude-opus-4.7`），
/// 但那不是合法的 Anthropic model ID 格式 —— 官方一律用连字符（`claude-opus-4-7`）。
/// 直接把上游形态透传给客户端会让模型列表里出现 `claude-opus-4.7`，UI 显示别扭，
/// 且做 model 名格式校验的客户端会拒。
///
/// **只作用于对外暴露的 ID**。发给上游的 model ID 仍由
/// [`crate::anthropic::converter::map_model`] 产出点号形式 —— 上游只认点号，
/// 换成连字符会得到「未知模型」。两套命名空间必须分开。
///
/// 非 Claude 模型（`deepseek-3.2` 等）不属于 Anthropic 命名空间，原样保留。
fn client_facing_model_id(upstream_id: &str) -> String {
    if upstream_id.starts_with("claude") {
        upstream_id.replace('.', "-")
    } else {
        upstream_id.to_string()
    }
}

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
        let client_id = client_facing_model_id(&m.model_id);
        out.push(Model {
            id: client_id.clone(),
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
                id: format!("{client_id}-thinking"),
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
            id: "claude-opus-5".to_string(),
            object: "model".to_string(),
            created: 1785000000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128000,
        },
        Model {
            id: "claude-opus-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1785000000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128000,
        },
        Model {
            id: "claude-fable-5".to_string(),
            object: "model".to_string(),
            created: 1785000000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128000,
        },
        Model {
            id: "claude-fable-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1785000000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128000,
        },
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
        // 上游报 claude-opus-4.7（点号），对客户端必须暴露成连字符形式
        let opus = models.iter().find(|m| m.id == "claude-opus-4-7").unwrap();
        assert_eq!(opus.max_tokens, 128000, "用上游 maxOutputTokens 真值");
        assert_eq!(opus.owned_by, "anthropic");
        assert!(
            !models
                .iter()
                .any(|m| m.id.starts_with("claude") && m.id.contains('.')),
            "对客户端暴露的 Claude ID 不得含点号"
        );
        assert!(
            models.iter().any(|m| m.id == "claude-opus-4-7-thinking"),
            "claude 应有 thinking 变体（同样是连字符形式）"
        );
        // 非 Claude 不属于 Anthropic 命名空间，原样保留
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

    /// 上游各种点号形态都要规范化；非 Claude 保持原样。
    #[test]
    fn client_facing_ids_use_hyphens_for_claude_only() {
        assert_eq!(client_facing_model_id("claude-opus-4.7"), "claude-opus-4-7");
        assert_eq!(
            client_facing_model_id("claude-sonnet-4.5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            client_facing_model_id("claude-haiku-4.5"),
            "claude-haiku-4-5"
        );
        // 已经是连字符的不受影响
        assert_eq!(client_facing_model_id("claude-opus-5"), "claude-opus-5");
        // 非 Claude 原样保留（deepseek 的 3.2 不是 Anthropic 命名空间）
        assert_eq!(client_facing_model_id("deepseek-3.2"), "deepseek-3.2");
    }

    /// 静态回退列表本身也不得含点号（它直接对客户端暴露）。
    #[test]
    fn static_models_have_no_dotted_ids() {
        for m in static_models() {
            assert!(!m.id.contains('.'), "静态列表 ID 含点号: {}", m.id);
        }
    }
}
