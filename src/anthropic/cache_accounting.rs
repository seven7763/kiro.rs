//! 中转层 Prompt cache 计费策略（per-request accounting）
//!
//! 从 handlers.rs 抽出。与 [`super::prompt_cache`] 的分工：
//! - `prompt_cache`：缓存存储（多断点指纹 LRU、conversation_id 复用表、TTL 分桶）。
//! - 本模块：把缓存存储的查询结果翻译成「本次请求的 cache 决定」+「向客户端上报的
//!   input / cache_creation / cache_read 三字段」，即 per-request 计费口径。
//!
//! handler 在请求前调 [`lookup_prompt_cache`] 拿 [`CacheDecision`]，请求成功后调
//! [`record_cache_outcome`] / [`record_cache_report_only`] 回写统计。

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::converter::extract_session_id;
use super::prompt_cache::{CacheProfile, PromptCache, build_profile_from_request};
use super::token_count::{self as token, saturating_to_i32};
use super::types::MessagesRequest;

/// 构造 Anthropic usage 中的 `cache_creation` 明细对象。
///
/// 5m/1h 分桶：本地只模拟 5m（ephemeral）写入，1h 恒 0。多处 message_start /
/// message_delta / 非流式响应共用此结构，集中一处避免字段漂移。
pub(crate) fn cache_creation_breakdown(cache_creation_input_tokens: i32) -> Value {
    serde_json::json!({
        "ephemeral_5m_input_tokens": cache_creation_input_tokens.max(0),
        "ephemeral_1h_input_tokens": 0
    })
}

/// 中转层 Prompt cache 决定结果
///
/// 决定本次请求要使用哪个 conversation_id，以及向客户端上报的 cache_*_input_tokens。
///
/// **多断点精确计费**：基于 [`super::prompt_cache`] 的多断点 + 最长前缀匹配算法，
/// `cache_creation` / `cache_read` 都是真实值（不再恒 0）。客户端 UI 和下游计费
/// 系统（sub2api）据此看到正确的命中分解。
///
/// **`skipped` 状态**：客户端没打任何 `cache_control: ephemeral` 标记时，整个 cache
/// 模块跳过——不查询、不回写、不上报，行为回到"无 cache"基线。
#[derive(Debug, Clone)]
pub(crate) struct CacheDecision {
    /// 命中时强制用此 conversation_id（让上游 session 缓存复用）
    pub forced_conversation_id: Option<String>,
    /// cache_read_input_tokens：命中前缀的累积 token（向客户端上报）
    pub cache_read_input_tokens: i32,
    /// cache_creation_input_tokens：本次新增写入的 token（向客户端上报）
    pub cache_creation_input_tokens: i32,
    /// 本次请求的 profile（请求成功后用于 update 回写）；skipped 时为 None
    pub profile: Option<CacheProfile>,
    /// 当前请求的隔离账号/session key；无法识别身份时不回写、不复用 conversation_id
    pub account_key: Option<String>,
    /// 是否跳过 cache（客户端没显式启用）
    pub skipped: bool,
}

impl CacheDecision {
    /// 跳过 cache 的默认决定：所有 cache 字段归零，conversation_id 走默认逻辑
    fn skipped() -> Self {
        Self {
            forced_conversation_id: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            profile: None,
            account_key: None,
            skipped: true,
        }
    }
}

fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..16])
}

/// 诊断:把请求按 fingerprint 的组成拆成各部件的短 hash(纯 hash,不泄露内容),
/// 用于定位"同一会话每请求 stable_fp 都变"到底是哪个部件在漂移。
/// 部件:prelude(model+tool_choice) / tools / system(归一化后) / 首条 message。
fn diag_fingerprint_components(payload: &MessagesRequest) -> String {
    let h = |s: &str| {
        let d = Sha256::digest(s.as_bytes());
        hex::encode(&d[..6])
    };
    let tc = payload
        .tool_choice
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_default();
    let n_tools = payload.tools.as_ref().map(|t| t.len()).unwrap_or(0);
    let tools_repr = payload
        .tools
        .as_ref()
        .map(|ts| {
            ts.iter()
                .map(|t| format!("{}\u{1f}{}", t.name, t.description))
                .collect::<Vec<_>>()
                .join("|")
        })
        .unwrap_or_default();
    let n_sys = payload.system.as_ref().map(|s| s.len()).unwrap_or(0);
    let sys_norm = payload
        .system
        .as_ref()
        .map(|ss| {
            ss.iter()
                .map(|s| super::prompt_cache::normalize_system_text(&s.text))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let n_msgs = payload.messages.len();
    let msg0 = payload
        .messages
        .first()
        .map(|m| m.content.to_string())
        .unwrap_or_default();
    format!(
        "model={} h_tc={} n_tools={} h_tools={} n_sys={} h_sys={} sys_len={} n_msgs={} h_msg0={} msg0_len={}",
        payload.model,
        h(&tc),
        n_tools,
        h(&tools_repr),
        n_sys,
        h(&sys_norm),
        sys_norm.len(),
        n_msgs,
        h(&msg0),
        msg0.len(),
    )
}

pub(crate) fn prompt_cache_account(payload: &MessagesRequest) -> Option<String> {
    let user_id = payload
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;

    if let Some(session_id) = extract_session_id(user_id) {
        return Some(format!("session:{}", short_hash(&session_id)));
    }

    Some(format!("metadata:{}", short_hash(user_id)))
}

fn estimate_message_content_tokens(content: &Value) -> u64 {
    match content {
        Value::String(text) => token::count_tokens(text),
        Value::Array(blocks) => blocks.iter().map(estimate_message_content_tokens).sum(),
        Value::Object(block) => {
            if let Some(text) = block
                .get("text")
                .and_then(|v| v.as_str())
                .or_else(|| block.get("thinking").and_then(|v| v.as_str()))
            {
                return token::count_tokens(text);
            }
            if let Some(content) = block.get("content") {
                return estimate_message_content_tokens(content);
            }
            if let Some(input) = block.get("input") {
                return token::count_tokens(&input.to_string());
            }
            token::count_tokens(&Value::Object(block.clone()).to_string())
        }
        _ => 0,
    }
}

pub(crate) fn estimate_incremental_input_tokens(
    payload: &MessagesRequest,
    client_input_tokens: i32,
) -> i32 {
    let total = client_input_tokens.max(0);
    if total == 0 {
        return 0;
    }

    let tokens = payload
        .messages
        .iter()
        .rev()
        .find(|msg| msg.role == "user")
        .map(|msg| estimate_message_content_tokens(&msg.content))
        .unwrap_or(0);

    let estimated = saturating_to_i32(tokens).max(1);
    estimated.min(total)
}

/// 计算 prompt cache 决定（命中查询 + usage 计算，但**不**回写——回写在请求成功后做）
///
/// `total_input_tokens`：本次请求的全量 input tokens（用于 85% 上限与 TTL 分桶）。
pub(crate) fn lookup_prompt_cache(
    cache: &PromptCache,
    payload: &MessagesRequest,
    total_input_tokens: i32,
) -> CacheDecision {
    // 没有任何 cache_control 标记 → 跳过，行为与"无 cache"基线一致
    let Some(profile) = build_profile_from_request(payload, total_input_tokens) else {
        tracing::trace!("prompt_cache: skipped (no cache_control marker on client request)");
        return CacheDecision::skipped();
    };

    let account_key = prompt_cache_account(payload);
    if cache.perceived_ratio().is_some() {
        tracing::debug!(
            "prompt_cache: perceived/fake cache enabled; skip real cache lookup and conversation reuse"
        );
        return CacheDecision {
            forced_conversation_id: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            profile: Some(profile),
            account_key,
            skipped: false,
        };
    }

    let (usage, forced_conversation_id) = match account_key.as_deref() {
        Some(account) => (
            cache.compute(account, &profile),
            cache.lookup_conversation(account, &profile),
        ),
        None => {
            tracing::debug!(
                "prompt_cache: request has cache_control but no metadata.user_id; skip shared cache reuse for multi-user safety"
            );
            (Default::default(), None)
        }
    };

    // 诊断探针（定位 "只创建不读取"）：暴露 account_key 前缀 + 全局桶大小 + 命中断点位置。
    // 仅在 debug 级启用时才执行（zero-cost in prod info 级）：RUST_LOG 含 debug 即可见。
    // bucket=0 → 从未 update；bucket>0 且 matched=None → 指纹漂移。
    if tracing::enabled!(tracing::Level::DEBUG) {
        if let Some(account) = account_key.as_deref() {
            let (bucket_len, matched, total_bp) = cache.debug_probe(account, &profile);
            tracing::debug!(
                "prompt_cache.probe: account={} bucket_entries={} matched_bp={:?}/{} stable_fp={}",
                account,
                bucket_len,
                matched,
                total_bp,
                &profile.stable_fingerprint.get(..12).unwrap_or("")
            );
            tracing::debug!(
                "prompt_cache.diag: account={} {}",
                account,
                diag_fingerprint_components(payload)
            );
        }
    }

    tracing::debug!(
        "prompt_cache: creation={} read={} (5m={} 1h={}) breakpoints={} account_scoped={} conv_reuse={}",
        usage.cache_creation,
        usage.cache_read,
        usage.creation_5m,
        usage.creation_1h,
        profile.breakpoints.len(),
        account_key.is_some(),
        forced_conversation_id.is_some(),
    );

    CacheDecision {
        forced_conversation_id,
        cache_read_input_tokens: usage.cache_read,
        cache_creation_input_tokens: usage.cache_creation,
        profile: Some(profile),
        account_key,
        skipped: false,
    }
}

/// 请求成功后记录 cache 统计；真实 cache 模式会同时写入断点 fingerprint +
/// conversation_id 复用映射。perceived/fake 模式只记录 reported 统计，不维护真实缓存。
pub(crate) fn record_cache_outcome(
    cache: &PromptCache,
    decision: &CacheDecision,
    actual_conversation_id: &str,
    reported_cache_read_input_tokens: i32,
) {
    if decision.skipped {
        return;
    }
    if let Some(profile) = decision.profile.as_ref() {
        cache.record_success(
            decision.cache_read_input_tokens,
            reported_cache_read_input_tokens,
        );
        if cache.perceived_ratio().is_some() {
            return;
        }
        if let Some(account) = decision.account_key.as_deref() {
            cache.update(account, profile, actual_conversation_id);
        }
    }
}

pub(crate) fn record_cache_report_only(
    cache: &PromptCache,
    decision: &CacheDecision,
    reported_cache_read_input_tokens: i32,
) {
    if decision.skipped {
        return;
    }
    if decision.profile.is_some() {
        cache.record_success(
            decision.cache_read_input_tokens,
            reported_cache_read_input_tokens,
        );
    }
}

fn min_visible_cache_tokens(model: &str) -> i32 {
    let m = model.to_lowercase();
    if m.contains("opus") || m.contains("haiku") {
        4096
    } else {
        1024
    }
}

/// 生成客户端可见的 usage 三字段，保证 input / cache_creation / cache_read
/// 互斥且总和不超过客户端实际输入 token。
///
/// `perceived_cache_hit_ratio` 是运营口径：开启后对带 cache_control 且达到阈值
/// 的请求按"本轮新增输入"保留少量普通 input，其余转入 cache_read，creation
/// 置 0，避免大上下文每轮残留 8% input 或 cache write 溢价导致客户计费异常。
pub(crate) fn client_visible_usage(
    cache: &PromptCache,
    decision: &CacheDecision,
    model: &str,
    client_input_tokens: i32,
    incremental_input_tokens: i32,
) -> (i32, i32, i32) {
    let total = client_input_tokens.max(0);
    if total == 0 || decision.skipped || !cache.is_enabled() {
        return (total, 0, 0);
    }

    if let Some(ratio) = cache.perceived_ratio() {
        if total < min_visible_cache_tokens(model) {
            return (total, 0, 0);
        }
        let ratio_input = total.saturating_sub(((total as f64) * ratio).floor() as i32);
        let incremental_input = incremental_input_tokens.clamp(1, total);
        let input = incremental_input.min(ratio_input.max(1)).min(total);
        let cache_read = total.saturating_sub(input);
        return (input, 0, cache_read);
    }

    let cache_read = decision.cache_read_input_tokens.max(0).min(total);
    let remaining = total.saturating_sub(cache_read);
    let cache_creation = decision.cache_creation_input_tokens.max(0).min(remaining);
    let input = remaining.saturating_sub(cache_creation);
    (input, cache_creation, cache_read)
}

// CACHE_ACCOUNTING_PLACEHOLDER

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小可用的 MessagesRequest
    fn make_req(model: &str, thinking: Option<super::super::types::Thinking>) -> MessagesRequest {
        let mut payload: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": model,
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .expect("构造 MessagesRequest 应成功");
        payload.thinking = thinking;
        payload
    }

    fn cache_decision_for_test(skipped: bool, creation: i32, read: i32) -> CacheDecision {
        CacheDecision {
            forced_conversation_id: None,
            cache_read_input_tokens: read,
            cache_creation_input_tokens: creation,
            profile: None,
            account_key: None,
            skipped,
        }
    }

    fn cctest_like_payload(session_id: &str, messages: serde_json::Value) -> MessagesRequest {
        let system_text = "cacheable project context line. ".repeat(5000);
        serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "metadata": {
                "user_id": format!("user_test_account__session_{session_id}")
            },
            "system": [{
                "text": system_text,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": messages
        }))
        .expect("构造 cctest-like MessagesRequest 应成功")
    }

    fn client_total_tokens(payload: &MessagesRequest) -> i32 {
        saturating_to_i32(token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ))
    }

    #[test]
    fn prompt_cache_account_uses_session_metadata() {
        let session_id = "8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let mut req = make_req("claude-sonnet-4-5", None);
        req.metadata = Some(super::super::types::Metadata {
            user_id: Some(format!("user_a_account__session_{session_id}")),
        });
        let first = prompt_cache_account(&req).expect("session metadata should produce key");

        req.metadata = Some(super::super::types::Metadata {
            user_id: Some(format!(
                r#"{{"device_id":"device-b","account_uuid":"","session_id":"{session_id}"}}"#
            )),
        });
        assert_eq!(
            prompt_cache_account(&req).as_deref(),
            Some(first.as_str()),
            "同一个 session 应映射到同一个隔离 key"
        );
    }

    #[test]
    fn prompt_cache_account_separates_different_users() {
        let mut req = make_req("claude-sonnet-4-5", None);
        req.metadata = Some(super::super::types::Metadata {
            user_id: Some("user-a".to_string()),
        });
        let user_a = prompt_cache_account(&req);
        req.metadata = Some(super::super::types::Metadata {
            user_id: Some("user-b".to_string()),
        });
        assert_ne!(user_a, prompt_cache_account(&req));

        req.metadata = None;
        assert!(
            prompt_cache_account(&req).is_none(),
            "无法识别用户时不能落到共享缓存桶"
        );
    }

    #[test]
    fn incremental_input_uses_last_user_message() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "this is a long cached history message"},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "1"}
            ]
        }))
        .expect("构造 MessagesRequest 应成功");

        assert_eq!(estimate_incremental_input_tokens(&req, 10_000), 1);
    }

    #[test]
    fn incremental_input_counts_non_text_current_blocks() {
        let image_data = "a".repeat(400);
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": image_data
                        }
                    },
                    {"type": "text", "text": "describe"}
                ]
            }]
        }))
        .expect("构造 MessagesRequest 应成功");

        assert!(
            estimate_incremental_input_tokens(&req, 10_000) > 20,
            "图片/未知 block 不能被压成 1 token"
        );
    }

    #[test]
    fn cctest_like_multiround_keeps_visible_input_incremental() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            true,
            Some(0.92),
        );
        let session_id = "8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let mut history = Vec::new();

        for round in 1..=7 {
            history.push(serde_json::json!({"role": "user", "content": "1"}));
            let payload =
                cctest_like_payload(session_id, serde_json::Value::Array(history.clone()));
            let total = client_total_tokens(&payload);
            assert!(
                total > 10_000,
                "测试数据需要模拟长上下文，当前 total={total}"
            );

            let decision = lookup_prompt_cache(&cache, &payload, total);
            assert!(
                decision.forced_conversation_id.is_none(),
                "fake cache 模式不应依赖真实 conversation cache，第 {round} 轮也不能强制复用"
            );

            let incremental = estimate_incremental_input_tokens(&payload, total);
            let (input, creation, read) =
                client_visible_usage(&cache, &decision, &payload.model, total, incremental);

            assert_eq!(input, 1, "第 {round} 轮 input 不应回到 1459/1460");
            assert_eq!(creation, 0, "fake cache 模式不应产生 cache_creation 溢价");
            assert_eq!(read, total - input, "cache_read 应吸收除新增输入外的上下文");

            record_cache_outcome(&cache, &decision, "conv-shared", read);
            history.push(serde_json::json!({"role": "assistant", "content": "ok"}));
        }

        let snap = cache.snapshot();
        assert_eq!(
            snap.entries, 0,
            "fake cache 不应写入真实 prompt cache entry"
        );
    }

    #[test]
    fn cctest_like_multi_user_does_not_reuse_conversation() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            true,
            Some(0.92),
        );
        let messages = serde_json::json!([{"role": "user", "content": "1"}]);

        let user_a = cctest_like_payload("8bb5523b-ec7c-4540-a9ca-beb6d79f1552", messages.clone());
        let total_a = client_total_tokens(&user_a);
        let decision_a = lookup_prompt_cache(&cache, &user_a, total_a);
        let (_, _, read_a) = client_visible_usage(
            &cache,
            &decision_a,
            &user_a.model,
            total_a,
            estimate_incremental_input_tokens(&user_a, total_a),
        );
        record_cache_outcome(&cache, &decision_a, "conv-user-a", read_a);

        let user_b = cctest_like_payload("9cc5523b-ec7c-4540-a9ca-beb6d79f1552", messages.clone());
        assert_ne!(prompt_cache_account(&user_a), prompt_cache_account(&user_b));
        let total_b = client_total_tokens(&user_b);
        let decision_b = lookup_prompt_cache(&cache, &user_b, total_b);

        assert!(
            decision_b.forced_conversation_id.is_none(),
            "不同用户/session 不能复用 user A 的 conversation_id"
        );
        assert_eq!(
            decision_b.cache_read_input_tokens, 0,
            "不同用户/session 不能命中 user A 的真实 prompt cache 桶"
        );
    }

    #[test]
    fn cctest_like_without_metadata_never_stores_shared_cache() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            true,
            Some(0.92),
        );
        let mut payload = cctest_like_payload(
            "8bb5523b-ec7c-4540-a9ca-beb6d79f1552",
            serde_json::json!([{"role": "user", "content": "1"}]),
        );
        payload.metadata = None;

        let total = client_total_tokens(&payload);
        let decision = lookup_prompt_cache(&cache, &payload, total);
        assert!(decision.account_key.is_none());
        assert!(decision.forced_conversation_id.is_none());

        let (_, _, read) = client_visible_usage(
            &cache,
            &decision,
            &payload.model,
            total,
            estimate_incremental_input_tokens(&payload, total),
        );
        record_cache_outcome(&cache, &decision, "conv-without-metadata", read);

        let snap = cache.snapshot();
        assert_eq!(snap.entries, 0, "无 metadata 时不能写入共享缓存桶");

        let next = lookup_prompt_cache(&cache, &payload, total);
        assert!(next.forced_conversation_id.is_none());
        assert_eq!(next.cache_read_input_tokens, 0);
    }

    #[test]
    fn websearch_report_only_updates_stats_without_shared_cache_entry() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            true,
            Some(0.92),
        );
        let mut payload = cctest_like_payload(
            "8bb5523b-ec7c-4540-a9ca-beb6d79f1552",
            serde_json::json!([{"role": "user", "content": "Perform a web search for the query: rust"}]),
        );
        payload.tools = Some(vec![super::super::types::Tool {
            tool_type: Some("web_search_20250305".to_string()),
            name: "web_search".to_string(),
            description: String::new(),
            input_schema: Default::default(),
            max_uses: Some(1),
            cache_control: None,
        }]);

        let total = client_total_tokens(&payload);
        let decision = lookup_prompt_cache(&cache, &payload, total);
        let (_, _, read) = client_visible_usage(
            &cache,
            &decision,
            &payload.model,
            total,
            estimate_incremental_input_tokens(&payload, total),
        );
        record_cache_report_only(&cache, &decision, read);

        let snap = cache.snapshot();
        assert_eq!(
            snap.entries, 0,
            "WebSearch 只记对账统计，不写 conversation 缓存"
        );
        assert_eq!(snap.miss_total, 1);
        assert_eq!(snap.last1m.reported_hit_rate(), 100.0);
        assert!(snap.last1m.reported_saved_input_tokens > 0);
    }

    /// 真实模式（无 perceived 系数）多轮对话：R1 全 creation，R2 起必须命中 cache_read。
    /// 复现生产 "只有创建缓存、没有读取缓存" 的诉求 —— 走完整 cache_accounting 路径。
    #[test]
    fn cctest_like_real_mode_multiround_hits_cache_read() {
        let cache = PromptCache::new(1024, std::time::Duration::from_secs(300), true);
        assert!(cache.perceived_ratio().is_none(), "本测试必须是真实模式");
        let session_id = "8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let mut history = Vec::new();

        let mut saw_read = false;
        for round in 1..=4 {
            history.push(serde_json::json!({"role": "user", "content": "1"}));
            let payload =
                cctest_like_payload(session_id, serde_json::Value::Array(history.clone()));
            let total = client_total_tokens(&payload);

            let decision = lookup_prompt_cache(&cache, &payload, total);
            let (input, creation, read) = client_visible_usage(
                &cache,
                &decision,
                &payload.model,
                total,
                estimate_incremental_input_tokens(&payload, total),
            );

            if round == 1 {
                assert!(creation > 0, "R1 应有 cache_creation");
                assert_eq!(read, 0, "R1 不应有 cache_read");
            } else {
                assert!(
                    read > 0,
                    "R{round} 应命中 cache_read（input={input} creation={creation} read={read}）"
                );
                saw_read = true;
            }

            // 模拟上游成功 → 回写断点 + conversation
            record_cache_outcome(&cache, &decision, "conv-real", read);
            history.push(serde_json::json!({"role": "assistant", "content": "ok"}));
        }
        assert!(saw_read, "真实模式多轮必须出现 cache_read>0");
    }

    /// 直击生产根因：客户端 `metadata.user_id` 经 new2api 网关每轮漂移
    /// （session id 每请求都不同 → account_key 每轮不同）。修复前每轮落空桶 →
    /// 只创建不读取；全局内容寻址后,只要 prefix 内容稳定,R2 起照样命中 cache_read。
    #[test]
    fn drifting_account_key_still_hits_cache_read_after_global_fix() {
        let cache = PromptCache::new(1024, std::time::Duration::from_secs(300), true);
        assert!(cache.perceived_ratio().is_none(), "真实模式");
        // 固定的可缓存 system prefix（跨轮内容不变,只有 user_id 在变）
        let system_text = "stable cacheable project context line. ".repeat(5000);
        let mut history = Vec::new();

        let mut saw_read = false;
        for round in 1..=4 {
            history.push(serde_json::json!({"role": "user", "content": "1"}));
            // 关键:每轮一个**全新的 session id**,模拟网关漂移 account_key。
            let drifting_session = format!("{round:08x}-ec7c-4540-a9ca-beb6d79f1552");
            let payload: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 100,
                "metadata": { "user_id": format!("user_x_account__session_{drifting_session}") },
                "system": [{ "text": system_text, "cache_control": {"type": "ephemeral"} }],
                "messages": serde_json::Value::Array(history.clone())
            }))
            .expect("构造请求");

            let total = saturating_to_i32(token::count_all_tokens(
                payload.model.clone(),
                payload.system.clone(),
                payload.messages.clone(),
                payload.tools.clone(),
            ));
            let decision = lookup_prompt_cache(&cache, &payload, total);
            let (_, creation, read) = client_visible_usage(
                &cache,
                &decision,
                &payload.model,
                total,
                estimate_incremental_input_tokens(&payload, total),
            );

            if round == 1 {
                assert!(creation > 0 && read == 0, "R1 全 creation");
            } else {
                assert!(
                    read > 0,
                    "R{round} account_key 漂移仍应命中 cache_read（全局桶）: creation={creation} read={read}"
                );
                saw_read = true;
            }
            record_cache_outcome(&cache, &decision, "conv-real", read);
            history.push(serde_json::json!({"role": "assistant", "content": "ok"}));
        }
        assert!(saw_read, "account_key 漂移场景修复后必须出现 cache_read>0");
    }

    // CACHE_TESTS_PLACEHOLDER

    #[test]
    fn perceived_usage_uses_client_budget_and_zero_creation() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            true,
            Some(0.92),
        );
        let decision = cache_decision_for_test(false, 99_000, 99_000);
        let (input, creation, read) =
            client_visible_usage(&cache, &decision, "claude-opus-4-8", 10_000, 12);
        assert_eq!(read, 9_988);
        assert_eq!(creation, 0);
        assert_eq!(input, 12);
        assert_eq!(input + creation + read, 10_000);
    }

    #[test]
    fn perceived_usage_caps_large_incremental_input_by_ratio_tail() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            true,
            Some(0.92),
        );
        let decision = cache_decision_for_test(false, 0, 0);
        let (input, creation, read) =
            client_visible_usage(&cache, &decision, "claude-opus-4-8", 10_000, 5_000);
        assert_eq!((input, creation, read), (800, 0, 9_200));
    }

    #[test]
    fn perceived_usage_does_not_fake_without_cache_control() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            true,
            Some(0.92),
        );
        let decision = cache_decision_for_test(true, 0, 0);
        let (input, creation, read) =
            client_visible_usage(&cache, &decision, "claude-opus-4-8", 10_000, 1);
        assert_eq!((input, creation, read), (10_000, 0, 0));
    }

    #[test]
    fn perceived_usage_respects_disabled_cache_switch() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            false,
            Some(0.92),
        );
        let decision = cache_decision_for_test(false, 0, 0);
        let (input, creation, read) =
            client_visible_usage(&cache, &decision, "claude-opus-4-8", 10_000, 1);
        assert_eq!((input, creation, read), (10_000, 0, 0));
    }

    #[test]
    fn perceived_usage_respects_visible_min_threshold() {
        let cache = PromptCache::new_with_perceived(
            1024,
            std::time::Duration::from_secs(300),
            true,
            Some(0.92),
        );
        let decision = cache_decision_for_test(false, 0, 0);
        let (input, creation, read) =
            client_visible_usage(&cache, &decision, "claude-opus-4-8", 4_000, 1);
        assert_eq!((input, creation, read), (4_000, 0, 0));
    }

    #[test]
    fn real_usage_is_clamped_to_client_budget() {
        let cache = PromptCache::new(1024, std::time::Duration::from_secs(300), true);
        let decision = cache_decision_for_test(false, 700, 700);
        let (input, creation, read) =
            client_visible_usage(&cache, &decision, "claude-sonnet-4-5", 1_000, 1);
        assert_eq!((input, creation, read), (0, 300, 700));
        assert_eq!(input + creation + read, 1_000);
    }
}
