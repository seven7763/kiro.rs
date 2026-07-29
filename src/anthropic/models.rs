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
        let base = Model {
            id: client_id,
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
        };
        // 只有 Claude 系有 thinking 概念；具体是否派生变体由
        // `thinking_variant_is_meaningful` 决定（强制 adaptive 的型号不派生）。
        if is_claude {
            push_with_thinking_variant(&mut out, base);
        } else {
            out.push(base);
        }
    }
    if out.is_empty() { static_models() } else { out }
}

/// 静态回退表的一行：`(客户端 ID, 显示名, created, max_tokens)`。
///
/// thinking 变体不在这里列——由 [`push_with_thinking_variant`] 自动派生，
/// 与 [`models_from_upstream`] 走同一套规则（对标 Kiro-Go 的
/// `buildAnthropicModelsResponse`：上游列表 + 自动追加后缀）。
type StaticModelRow = (&'static str, &'static str, i64, i32);

/// 上游 `ListAvailableModels` 实测在售型号（2026-07 生产快照，28 条中的 Claude 系）。
/// 只在上游拉取失败时使用。
const STATIC_MODEL_ROWS: &[StaticModelRow] = &[
    ("claude-opus-5", "Claude Opus 5", 1785000000, 128000),
    ("claude-sonnet-5", "Claude Sonnet 5", 1785000000, 128000),
    ("claude-fable-5", "Claude Fable 5", 1785000000, 128000),
    ("claude-opus-4-8", "Claude Opus 4.8", 1781000000, 128000),
    ("claude-opus-4-7", "Claude Opus 4.7", 1778400000, 64000),
    ("claude-opus-4-6", "Claude Opus 4.6", 1770163200, 64000),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6", 1771286400, 64000),
    (
        "claude-opus-4-5-20251101",
        "Claude Opus 4.5",
        1763942400,
        64000,
    ),
    (
        "claude-sonnet-4-5-20250929",
        "Claude Sonnet 4.5",
        1759104000,
        64000,
    ),
    (
        "claude-haiku-4-5-20251001",
        "Claude Haiku 4.5",
        1760486400,
        64000,
    ),
];

/// 这些型号的 `-thinking` 变体是空壳，不对外列出。
///
/// `preprocess.rs` 对 Opus 4.7+ / Opus 5 / Fable 5 **强制** adaptive thinking，
/// 不论请求里带不带 `-thinking` 后缀（见 `preprocess.rs` 的 Case 1）。也就是说
/// `claude-opus-4-7` 与 `claude-opus-4-7-thinking` 行为完全一致。同时列出两个
/// 会让人以为能关掉思考，实际关不掉。
///
/// 其他型号（4.6 / 4.5 / sonnet-4 / haiku）的后缀是真实开关，照常派生。
fn thinking_variant_is_meaningful(client_id: &str) -> bool {
    let lower = client_id.to_lowercase();
    let forced_adaptive = lower.contains("fable")
        || lower.contains("opus-5")
        || (lower.contains("opus")
            && (lower.contains("4-7")
                || lower.contains("4.7")
                || lower.contains("4-8")
                || lower.contains("4.8")));
    !forced_adaptive
}

/// push 基础型号，并在 thinking 后缀有实际作用时追加变体。
fn push_with_thinking_variant(out: &mut Vec<Model>, base: Model) {
    if thinking_variant_is_meaningful(&base.id) {
        out.push(Model {
            id: format!("{}-thinking", base.id),
            display_name: format!("{} (Thinking)", base.display_name),
            ..base.clone()
        });
    }
    out.push(base);
}

/// 内置静态模型列表（上游获取失败时的回退，也是过滤的元数据来源）。
pub(crate) fn static_models() -> Vec<Model> {
    let mut out = Vec::with_capacity(STATIC_MODEL_ROWS.len() * 2);
    for (id, display_name, created, max_tokens) in STATIC_MODEL_ROWS {
        push_with_thinking_variant(
            &mut out,
            Model {
                id: (*id).to_string(),
                object: "model".to_string(),
                created: *created,
                owned_by: "anthropic".to_string(),
                display_name: (*display_name).to_string(),
                model_type: "chat".to_string(),
                max_tokens: *max_tokens,
            },
        );
    }
    out
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
            mk("claude-opus-4.6", "Claude Opus 4.6", Some(64000)),
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
            models.iter().any(|m| m.id == "claude-opus-4-6-thinking"),
            "thinking 后缀有实际作用的 claude 型号应有变体（连字符形式）"
        );
        // 非 Claude 不属于 Anthropic 命名空间，原样保留
        let ds = models.iter().find(|m| m.id == "deepseek-3.2").unwrap();
        assert_eq!(ds.owned_by, "kiro");
        assert!(
            !models.iter().any(|m| m.id == "deepseek-3.2-thinking"),
            "非 claude 不应有 thinking 变体"
        );
    }

    /// 强制 adaptive 的型号不列 `-thinking` 空壳。
    ///
    /// `preprocess.rs` 对 Opus 4.7 / 4.8 / Opus 5 / Fable 5 无条件强制 adaptive，
    /// 带不带后缀行为一致。列出两个等价 ID 会让客户端以为能关掉思考。
    #[test]
    fn forced_adaptive_models_have_no_thinking_shell() {
        use crate::kiro::provider::UpstreamModel;
        let mk = |id: &str| UpstreamModel {
            model_id: id.to_string(),
            model_name: id.to_string(),
            max_output_tokens: Some(128000),
        };
        let upstream: Vec<_> = [
            "claude-opus-5",
            "claude-fable-5",
            "claude-opus-4.7",
            "claude-opus-4.8",
        ]
        .iter()
        .map(|id| mk(id))
        .collect();
        let models = models_from_upstream(&upstream);
        for id in [
            "claude-opus-5-thinking",
            "claude-fable-5-thinking",
            "claude-opus-4-7-thinking",
            "claude-opus-4-8-thinking",
        ] {
            assert!(
                !models.iter().any(|m| m.id == id),
                "{id} 是空壳变体，不应对外列出"
            );
        }
        // 基础型号本身必须在
        assert!(models.iter().any(|m| m.id == "claude-opus-5"));
        assert!(models.iter().any(|m| m.id == "claude-opus-4-7"));

        // 静态回退表同样不含空壳
        for id in ["claude-opus-5-thinking", "claude-opus-4-7-thinking"] {
            assert!(
                !static_models().iter().any(|m| m.id == id),
                "静态表里的 {id} 也应移除"
            );
        }
        // sonnet-5 不在强制列表里（preprocess 只强制 opus-5 / fable），保留变体
        assert!(
            static_models()
                .iter()
                .any(|m| m.id == "claude-sonnet-4-6-thinking")
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
