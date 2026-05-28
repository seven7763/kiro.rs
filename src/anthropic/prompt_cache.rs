//! Prompt prefix cache（中转层自实现，多断点精确计费版）
//!
//! ## 为什么需要
//!
//! Kiro 上游协议（CodeWhisperer/Q）原生**不支持** Anthropic 的 prompt caching。
//! 客户端发的 `cache_control` 标记会被 serde 静默丢弃，导致 cache_creation_input_tokens
//! 和 cache_read_input_tokens 永远是 0，客户端 UI 看到的命中率永远是 0%。
//!
//! ## 实现思路（参考 chaogei/Kiro-account-manager 的 promptCacheTracker）
//!
//! 1. 把请求拍平成有序 block 序列：`tools[] → system[] → messages[]`，对每个 block
//!    维护一个**累积 SHA-256 hasher**。
//! 2. 在每个显式 `cache_control` 标记处、以及（出现显式标记后）每个消息结尾处，
//!    记录一个**断点** `(fingerprint=hasher 快照, cumulative_tokens, ttl)`。
//! 3. 命中计算：从后往前找最长的"未过期且 ≥ 最小阈值"的命中断点，精确算出
//!    `cache_read`（命中部分）和 `cache_creation`（新增部分），并按 5m/1h TTL 分桶。
//! 4. 请求成功后把本次所有断点 fingerprint 写入该 account 的缓存桶。
//!
//! ## 与 conversation_id 复用的关系
//!
//! 本模块额外维护一个 `fingerprint → conversation_id` 映射（最稳定断点优先），
//! 命中时复用上次的 conversation_id，让上游 Kiro 的 session 缓存有机会生效。
//! 这与精确计费正交：计费负责给客户端/下游正确的 usage 数字，conversation_id 复用
//! 负责尝试真正的上游加速。
//!
//! ## 对齐 Anthropic 的规则
//!
//! - 最小可缓存：Opus 4096 token / 其它 1024 token，低于阈值的断点不参与缓存。
//! - 命中上限 85%：最新内容不可能 100% 命中，cache_read 封顶 `total_input × 0.85`。
//! - TTL：`cache_control.ttl = "1h"` → 3600s，否则 300s（ephemeral 默认）。
//!
//! ## 限制
//!
//! - 单进程内存，不跨节点（多副本各算各的）。
//! - token 数为本地估算（`token::count_tokens`），非上游精确值。

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 默认 LRU 容量（按 account 桶内 fingerprint 总数粗略控制）
pub const DEFAULT_CAPACITY: usize = 1024;
/// 默认 TTL：5 分钟（对齐 Anthropic ephemeral 规范）
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);
/// 1 小时 TTL
const ONE_HOUR_TTL: Duration = Duration::from_secs(3600);
/// 最小可缓存 token 数（非 Opus）
const DEFAULT_MIN_CACHEABLE_TOKENS: i32 = 1024;
/// Opus 模型最小可缓存 token 数
const OPUS_MIN_CACHEABLE_TOKENS: i32 = 4096;
/// 命中上限比例（最新内容不可能 100% 命中）
const MAX_CACHE_RATIO: f64 = 0.85;
/// 每个 account 最大缓存条目数
const MAX_ENTRIES_PER_ACCOUNT: usize = 200;
/// 后台清理最小间隔
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// 全局桶名（请求未携带可识别 account 时使用）
pub const GLOBAL_ACCOUNT: &str = "_global";

// ============================================================================
// Profile / 断点 / Usage
// ============================================================================

/// 单个缓存断点
#[derive(Debug, Clone)]
pub struct CacheBreakpoint {
    /// 到此断点为止累积内容的 SHA-256（hex）
    pub fingerprint: String,
    /// 到此断点的累积 token 数
    pub cumulative_tokens: i32,
    /// 该断点的 TTL
    pub ttl: Duration,
}

/// 从一次请求构建的缓存 profile
#[derive(Debug, Clone)]
pub struct CacheProfile {
    pub breakpoints: Vec<CacheBreakpoint>,
    pub total_input_tokens: i32,
    pub model: String,
    /// 最稳定断点的 fingerprint（system 优先，用于 conversation_id 复用）。
    /// 没有断点时为空。
    pub stable_fingerprint: String,
}

/// 缓存命中计算结果（向客户端上报的 usage 分解）
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheUsage {
    pub cache_creation: i32,
    pub cache_read: i32,
    pub creation_5m: i32,
    pub creation_1h: i32,
}

/// 一个待 flatten 的内容块
struct CacheableBlock {
    value: String,
    tokens: i32,
    /// >0 表示该 block 自带显式 cache_control 断点（值为 TTL）
    ttl: Option<Duration>,
    is_message_end: bool,
    /// 是否来自 system（用于挑选最稳定断点）
    is_system: bool,
}

// ============================================================================
// 缓存条目与内部状态
// ============================================================================

#[derive(Debug, Clone)]
struct CacheEntry {
    expires_at: Instant,
    ttl: Duration,
}

#[derive(Debug, Clone, Copy)]
enum EventKind {
    Hit,
    Miss,
}

/// 内部状态：按 account 分桶
struct CacheInner {
    /// account → (fingerprint → entry)
    entries_by_account: HashMap<String, HashMap<String, CacheEntry>>,
    /// 最稳定断点 fingerprint → 上次该 prefix 的 conversation_id（跨 account 共享）
    conversation_by_fingerprint: HashMap<String, (String, Instant)>,
    capacity: usize,
    ttl: Duration,
    last_prune: Instant,
    hit_total: u64,
    miss_total: u64,
    eviction_total: u64,
    /// 滑动窗口事件 (time, kind, saved_tokens)
    recent_events: Vec<(Instant, EventKind, i32)>,
}

impl CacheInner {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries_by_account: HashMap::new(),
            conversation_by_fingerprint: HashMap::new(),
            capacity,
            ttl,
            last_prune: Instant::now(),
            hit_total: 0,
            miss_total: 0,
            eviction_total: 0,
            recent_events: Vec::with_capacity(2048),
        }
    }

    fn total_entries(&self) -> usize {
        self.entries_by_account.values().map(|m| m.len()).sum()
    }

    fn record_event(&mut self, now: Instant, kind: EventKind, saved_tokens: i32) {
        if self.recent_events.len() >= 2048 {
            let drain_to = self.recent_events.len() - 1024;
            self.recent_events.drain(..drain_to);
        }
        self.recent_events.push((now, kind, saved_tokens));
    }

    fn stats_in_window(&self, now: Instant, window: Duration) -> WindowStats {
        let mut hits = 0u64;
        let mut misses = 0u64;
        let mut saved_tokens: i64 = 0;
        for (t, kind, saved) in self.recent_events.iter().rev() {
            if now.duration_since(*t) > window {
                break;
            }
            match kind {
                EventKind::Hit => {
                    hits += 1;
                    saved_tokens = saved_tokens.saturating_add(*saved as i64);
                }
                EventKind::Miss => misses += 1,
            }
        }
        WindowStats {
            hits,
            misses,
            saved_input_tokens: saved_tokens,
        }
    }

    /// 周期性清理过期 entry 与 conversation 映射
    fn prune_if_needed(&mut self, now: Instant) {
        if now.duration_since(self.last_prune) < PRUNE_INTERVAL {
            return;
        }
        self.last_prune = now;
        self.entries_by_account.retain(|_, entries| {
            entries.retain(|_, e| e.expires_at > now);
            !entries.is_empty()
        });
        // conversation 映射用最长 TTL（1h）兜底过期
        self.conversation_by_fingerprint
            .retain(|_, (_, created)| now.duration_since(*created) < ONE_HOUR_TTL);
    }

    /// 缩容：当某 account 桶超过 capacity 时按 expires_at 淘汰最旧
    fn enforce_account_capacity(&mut self, account: &str) {
        let per_account_cap = self.capacity.clamp(1, MAX_ENTRIES_PER_ACCOUNT);
        if let Some(entries) = self.entries_by_account.get_mut(account) {
            if entries.len() <= per_account_cap {
                return;
            }
            let mut sorted: Vec<(String, Instant)> = entries
                .iter()
                .map(|(k, v)| (k.clone(), v.expires_at))
                .collect();
            sorted.sort_by_key(|(_, exp)| *exp);
            let to_remove = entries.len() - per_account_cap;
            for (k, _) in sorted.into_iter().take(to_remove) {
                entries.remove(&k);
                self.eviction_total = self.eviction_total.saturating_add(1);
            }
        }
    }
}

/// 单个时间窗口的统计
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowStats {
    pub hits: u64,
    pub misses: u64,
    pub saved_input_tokens: i64,
}

impl WindowStats {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 * 100.0 / total as f64
        }
    }
}

/// 整体快照
#[derive(Debug, Clone)]
pub struct CacheSnapshot {
    pub entries: usize,
    pub capacity: usize,
    pub ttl_secs: u64,
    pub hit_total: u64,
    pub miss_total: u64,
    pub eviction_total: u64,
    pub last1m: WindowStats,
    pub last5m: WindowStats,
}

// ============================================================================
// PromptCache：线程安全包装
// ============================================================================

#[derive(Clone)]
pub struct PromptCache {
    inner: Arc<Mutex<CacheInner>>,
    enabled: Arc<parking_lot::RwLock<bool>>,
}

impl PromptCache {
    pub fn new(capacity: usize, ttl: Duration, enabled: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheInner::new(capacity, ttl))),
            enabled: Arc::new(parking_lot::RwLock::new(enabled)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        *self.enabled.read()
    }

    pub fn set_enabled(&self, v: bool) {
        *self.enabled.write() = v;
    }

    pub fn set_capacity(&self, capacity: usize) {
        let mut inner = self.inner.lock();
        inner.capacity = capacity.max(1);
        let accounts: Vec<String> = inner.entries_by_account.keys().cloned().collect();
        for acc in accounts {
            inner.enforce_account_capacity(&acc);
        }
    }

    pub fn set_ttl(&self, ttl: Duration) {
        let mut inner = self.inner.lock();
        inner.ttl = ttl;
    }

    /// 计算命中情况（不修改命中标记之外的状态；会刷新命中 entry 的过期时间）。
    ///
    /// `account`：当前请求命中的凭据维度（如 credential id），用于缓存隔离。
    pub fn compute(&self, account: &str, profile: &CacheProfile) -> CacheUsage {
        if !self.is_enabled() || profile.breakpoints.is_empty() {
            return CacheUsage::default();
        }
        let now = Instant::now();
        let min_tokens = min_cacheable_tokens(&profile.model);
        let mut inner = self.inner.lock();
        inner.prune_if_needed(now);

        let last = profile.breakpoints.last().unwrap();
        let mut last_tokens = last.cumulative_tokens.min(profile.total_input_tokens);

        let has_entries = inner
            .entries_by_account
            .get(account)
            .map(|m| !m.is_empty())
            .unwrap_or(false);

        if !has_entries {
            // 首次：全部 creation（≥ 阈值才计），无 read
            let effective_creation = if last_tokens >= min_tokens {
                last_tokens
            } else {
                0
            };
            let (c5m, c1h) = compute_ttl_breakdown(profile, 0);
            inner.record_event(now, EventKind::Miss, 0);
            inner.miss_total = inner.miss_total.saturating_add(1);
            return CacheUsage {
                cache_creation: effective_creation,
                cache_read: 0,
                creation_5m: c5m,
                creation_1h: c1h,
            };
        }

        // 命中上限 85%
        let max_cacheable = (profile.total_input_tokens as f64 * MAX_CACHE_RATIO).floor() as i32;
        if last_tokens > max_cacheable {
            last_tokens = max_cacheable;
        }

        // 从后往前找最长命中断点
        let mut matched_tokens = 0i32;
        {
            let entries = inner.entries_by_account.get_mut(account);
            if let Some(entries) = entries {
                for bp in profile.breakpoints.iter().rev() {
                    if bp.cumulative_tokens < min_tokens {
                        continue;
                    }
                    if let Some(entry) = entries.get_mut(&bp.fingerprint) {
                        if entry.expires_at <= now {
                            continue;
                        }
                        // 命中：刷新过期时间
                        entry.expires_at = now + entry.ttl;
                        matched_tokens = bp.cumulative_tokens.min(profile.total_input_tokens);
                        if matched_tokens > last_tokens {
                            matched_tokens = last_tokens;
                        }
                        break;
                    }
                }
            }
        }

        let creation = (last_tokens - matched_tokens).max(0);
        let (c5m, c1h) = compute_ttl_breakdown(profile, matched_tokens);

        if matched_tokens > 0 {
            inner.record_event(now, EventKind::Hit, matched_tokens);
            inner.hit_total = inner.hit_total.saturating_add(1);
        } else {
            inner.record_event(now, EventKind::Miss, 0);
            inner.miss_total = inner.miss_total.saturating_add(1);
        }

        CacheUsage {
            cache_creation: creation,
            cache_read: matched_tokens,
            creation_5m: c5m,
            creation_1h: c1h,
        }
    }

    /// 请求成功后写入断点 fingerprint，并记录 conversation_id 复用映射。
    pub fn update(&self, account: &str, profile: &CacheProfile, conversation_id: &str) {
        if !self.is_enabled() || profile.breakpoints.is_empty() {
            return;
        }
        let now = Instant::now();
        let min_tokens = min_cacheable_tokens(&profile.model);
        let mut inner = self.inner.lock();

        let entries = inner
            .entries_by_account
            .entry(account.to_string())
            .or_default();
        for bp in &profile.breakpoints {
            if bp.cumulative_tokens < min_tokens {
                continue;
            }
            entries.insert(
                bp.fingerprint.clone(),
                CacheEntry {
                    expires_at: now + bp.ttl,
                    ttl: bp.ttl,
                },
            );
        }
        inner.enforce_account_capacity(account);

        // conversation_id 复用映射（最稳定断点）
        if !profile.stable_fingerprint.is_empty() && !conversation_id.is_empty() {
            inner.conversation_by_fingerprint.insert(
                profile.stable_fingerprint.clone(),
                (conversation_id.to_string(), now),
            );
        }
    }

    /// 查询可复用的 conversation_id（最稳定断点命中时返回）。
    pub fn lookup_conversation(&self, profile: &CacheProfile) -> Option<String> {
        if !self.is_enabled() || profile.stable_fingerprint.is_empty() {
            return None;
        }
        let now = Instant::now();
        let inner = self.inner.lock();
        inner
            .conversation_by_fingerprint
            .get(&profile.stable_fingerprint)
            .filter(|(_, created)| now.duration_since(*created) < ONE_HOUR_TTL)
            .map(|(id, _)| id.clone())
    }

    pub fn snapshot(&self) -> CacheSnapshot {
        let now = Instant::now();
        let inner = self.inner.lock();
        CacheSnapshot {
            entries: inner.total_entries(),
            capacity: inner.capacity,
            ttl_secs: inner.ttl.as_secs(),
            hit_total: inner.hit_total,
            miss_total: inner.miss_total,
            eviction_total: inner.eviction_total,
            last1m: inner.stats_in_window(now, Duration::from_secs(60)),
            last5m: inner.stats_in_window(now, Duration::from_secs(300)),
        }
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.entries_by_account.clear();
        inner.conversation_by_fingerprint.clear();
    }
}

impl Default for PromptCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_TTL, true)
    }
}

// ============================================================================
// Profile 构建
// ============================================================================

/// 单个 cache checkpoint 的最小可缓存 token 数。
///
/// 取值来自 2026-05 服务器实测的上游 `ListAvailableModels.promptCaching.
/// minimumTokensPerCacheCheckpoint`：
/// - `claude-opus-*` 与 `claude-haiku-*` → 4096
/// - `claude-sonnet-*` / `auto` 等 → 1024
///
/// 注意：实测 haiku 也是 4096（不是 1024），所以不能简单按"非 opus → 1024"。
fn min_cacheable_tokens(model: &str) -> i32 {
    let m = model.to_lowercase();
    if m.contains("opus") || m.contains("haiku") {
        OPUS_MIN_CACHEABLE_TOKENS
    } else {
        DEFAULT_MIN_CACHEABLE_TOKENS
    }
}

/// 把 `[matched_tokens, last_breakpoint]` 区间按断点 TTL 拆进 5m / 1h 两桶。
fn compute_ttl_breakdown(profile: &CacheProfile, matched_tokens: i32) -> (i32, i32) {
    let mut c5m = 0i32;
    let mut c1h = 0i32;
    let mut previous = matched_tokens;
    for bp in &profile.breakpoints {
        let current = bp.cumulative_tokens.min(profile.total_input_tokens);
        if current <= previous {
            continue;
        }
        let delta = current - previous;
        if bp.ttl >= ONE_HOUR_TTL {
            c1h += delta;
        } else {
            c5m += delta;
        }
        previous = current;
    }
    (c5m, c1h)
}

fn hash_chunk(hasher: &mut Sha256, chunk: &str) {
    hasher.update(chunk.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(chunk.as_bytes());
    hasher.update(b"\0");
}

/// 从 MessagesRequest 构建缓存 profile。无任何 cache_control 标记时返回 None。
pub fn build_profile_from_request(
    payload: &super::types::MessagesRequest,
    total_input_tokens: i32,
) -> Option<CacheProfile> {
    let blocks = flatten_cache_blocks(payload);
    if blocks.is_empty() {
        return None;
    }

    let mut hasher = Sha256::new();
    let mut breakpoints: Vec<CacheBreakpoint> = Vec::new();
    let mut cumulative_tokens = 0i32;
    let mut active_ttl: Option<Duration> = None;
    let mut stable_fingerprint = String::new();

    for block in &blocks {
        hash_chunk(&mut hasher, &block.value);
        cumulative_tokens = cumulative_tokens.saturating_add(block.tokens);

        let breakpoint_ttl = if let Some(ttl) = block.ttl {
            active_ttl = Some(ttl);
            Some(ttl)
        } else if block.is_message_end && active_ttl.is_some() {
            // 隐式断点：出现显式断点之后，每个消息结尾都是断点
            active_ttl
        } else {
            None
        };

        let Some(ttl) = breakpoint_ttl else {
            continue;
        };

        let fingerprint = hex::encode(hasher.clone().finalize());
        // 最稳定断点：第一个出现的（system 区段优先，因为顺序在前）
        if stable_fingerprint.is_empty() {
            stable_fingerprint = fingerprint.clone();
        } else if block.is_system {
            // system 断点比 message 断点更稳定，优先覆盖
            stable_fingerprint = fingerprint.clone();
        }

        breakpoints.push(CacheBreakpoint {
            fingerprint,
            cumulative_tokens,
            ttl,
        });
    }

    if breakpoints.is_empty() {
        return None;
    }

    Some(CacheProfile {
        breakpoints,
        total_input_tokens: total_input_tokens.max(cumulative_tokens),
        model: payload.model.clone(),
        stable_fingerprint,
    })
}

fn flatten_cache_blocks(payload: &super::types::MessagesRequest) -> Vec<CacheableBlock> {
    let mut blocks = Vec::new();
    let model = &payload.model;

    // tools
    if let Some(tools) = payload.tools.as_ref() {
        for tool in tools {
            let value = serde_json::json!({
                "kind": "tool",
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
            .to_string();
            let tokens = crate::token::count_tokens(&value) as i32;
            blocks.push(CacheableBlock {
                value,
                tokens,
                ttl: tool
                    .cache_control
                    .as_ref()
                    .filter(|c| c.is_ephemeral())
                    .map(|c| Duration::from_secs(c.ttl_secs())),
                is_message_end: false,
                is_system: false,
            });
        }
    }

    // system
    if let Some(system_blocks) = payload.system.as_ref() {
        for sm in system_blocks {
            let normalized = normalize_system_text(&sm.text);
            let value = format!("system\0{normalized}");
            let tokens = crate::token::count_tokens(&normalized) as i32;
            blocks.push(CacheableBlock {
                value,
                tokens,
                ttl: sm
                    .cache_control
                    .as_ref()
                    .filter(|c| c.is_ephemeral())
                    .map(|c| Duration::from_secs(c.ttl_secs())),
                is_message_end: false,
                is_system: true,
            });
        }
    }

    // messages
    for (idx, msg) in payload.messages.iter().enumerate() {
        flatten_message_blocks(&mut blocks, msg, idx, model);
    }

    blocks
}

fn flatten_message_blocks(
    blocks: &mut Vec<CacheableBlock>,
    msg: &super::types::Message,
    msg_index: usize,
    _model: &str,
) {
    match &msg.content {
        serde_json::Value::String(text) => {
            let value = format!("msg\0{}\0{}\0{}", msg.role, msg_index, text);
            let tokens = crate::token::count_tokens(text) as i32;
            blocks.push(CacheableBlock {
                value,
                tokens,
                ttl: None,
                is_message_end: true,
                is_system: false,
            });
        }
        serde_json::Value::Array(arr) => {
            let last_idx = arr.len().saturating_sub(1);
            for (i, block) in arr.iter().enumerate() {
                let text = block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .or_else(|| block.get("thinking").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| block.to_string());
                let value = format!("msg\0{}\0{}\0{}\0{}", msg.role, msg_index, i, block);
                let tokens = crate::token::count_tokens(&text) as i32;
                let ttl = extract_block_ttl(block);
                blocks.push(CacheableBlock {
                    value,
                    tokens,
                    ttl,
                    is_message_end: i == last_idx,
                    is_system: false,
                });
            }
        }
        _ => {}
    }
}

/// 从 message content block（serde_json::Value）提取 cache_control TTL。
fn extract_block_ttl(block: &serde_json::Value) -> Option<Duration> {
    let cc = block.get("cache_control")?;
    let cache_type = cc.get("type").and_then(|t| t.as_str())?;
    if cache_type.to_lowercase() != "ephemeral" {
        return None;
    }
    let secs = match cc.get("ttl").and_then(|t| t.as_str()) {
        Some("1h") | Some("1H") => 3600,
        _ => 300,
    };
    Some(Duration::from_secs(secs))
}

// ============================================================================
// system prompt 归一化（剥离动态字段，提高命中率）
// ============================================================================

/// 剥离 Claude Code CLI 客户端在 system prompt 中注入的动态字段
///
/// Claude Code（claude-cli）每次启动会把以下动态字段拼到 system prompt 里，
/// 中转层用精确 SHA256 hash 时这些动态字段导致 100% MISS。本函数把它们替换为
/// 稳定占位符 `<DYNAMIC>`，让相同 instruction prompt 在不同 session/版本/工作
/// 目录下能哈希到同一 key。
///
/// 实现：按行扫描的简单状态机，O(N)，N = system_text.len()。
pub fn normalize_system_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    const SECTION_MARKERS: &[&str] = &[
        "# environment",
        "# auto memory",
        "# memory",
        "gitstatus:",
        "<command-message>",
        "<command-name>",
    ];
    const LINE_KV_PREFIXES: &[&str] = &[
        "cc_version",
        "x-anthropic-billing-header",
        "session_id:",
        "request_id:",
        "platform:",
        "working directory:",
        "current directory:",
        "shell:",
        "os version:",
        "cwd:",
    ];

    let mut out = String::with_capacity(text.len());
    let mut in_drop_block = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        let lower = trimmed.to_lowercase();

        if in_drop_block {
            let is_blank = trimmed.is_empty();
            let is_new_section =
                trimmed.starts_with("# ") && !SECTION_MARKERS.iter().any(|m| lower.starts_with(m));
            if is_blank || is_new_section {
                in_drop_block = false;
                if is_new_section {
                    out.push_str(line);
                    out.push('\n');
                }
                continue;
            }
            continue;
        }

        if SECTION_MARKERS.iter().any(|m| lower.starts_with(m)) {
            out.push_str("<DYNAMIC>\n");
            in_drop_block = true;
            continue;
        }
        if LINE_KV_PREFIXES.iter().any(|p| lower.starts_with(p)) {
            out.push_str("<DYNAMIC>\n");
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
    use super::*;

    fn mk_request(system: Option<Vec<SystemMessage>>, messages: Vec<Message>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4-5".to_string(),
            max_tokens: 100,
            stream: false,
            system,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            messages,
        }
    }

    fn sys(text: &str, cache: bool) -> SystemMessage {
        SystemMessage {
            text: text.to_string(),
            cache_control: cache.then(|| CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        }
    }

    /// 构造一个 token 数足够大（≥1024）的文本
    fn big_text(repeat: usize) -> String {
        "word ".repeat(repeat)
    }

    #[test]
    fn no_cache_control_returns_none() {
        let payload = mk_request(
            Some(vec![sys("you are helpful", false)]),
            vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
        );
        assert!(build_profile_from_request(&payload, 100).is_none());
    }

    #[test]
    fn system_breakpoint_builds_profile() {
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        assert_eq!(profile.breakpoints.len(), 1);
        assert!(!profile.stable_fingerprint.is_empty());
        assert!(profile.breakpoints[0].cumulative_tokens > 0);
    }

    #[test]
    fn first_request_is_all_creation() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        let usage = cache.compute("acc1", &profile);
        assert!(usage.cache_creation > 0, "首次应全是 creation");
        assert_eq!(usage.cache_read, 0, "首次无 read");
    }

    #[test]
    fn second_request_hits_cache_read() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        // 首次 compute + update
        let _ = cache.compute("acc1", &profile);
        cache.update("acc1", &profile, "conv-1");
        // 第二次相同 prefix → 命中 read
        let usage = cache.compute("acc1", &profile);
        assert!(usage.cache_read > 0, "第二次应命中 cache_read");
    }

    #[test]
    fn account_isolation() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        let _ = cache.compute("acc1", &profile);
        cache.update("acc1", &profile, "conv-1");
        // 不同 account 不应命中
        let usage = cache.compute("acc2", &profile);
        assert_eq!(usage.cache_read, 0, "不同 account 应隔离，不命中");
    }

    #[test]
    fn min_token_threshold_skips_small_prefix() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        // 小 prefix（远低于 1024 token）
        let payload = mk_request(Some(vec![sys("short", true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5).unwrap();
        let _ = cache.compute("acc1", &profile);
        cache.update("acc1", &profile, "conv-1");
        let usage = cache.compute("acc1", &profile);
        assert_eq!(usage.cache_read, 0, "低于最小阈值不应命中");
        assert_eq!(usage.cache_creation, 0, "低于最小阈值不计 creation");
    }

    #[test]
    fn opus_uses_higher_threshold() {
        let mut payload = mk_request(Some(vec![sys(&big_text(2000), true)]), vec![]);
        payload.model = "claude-opus-4-7".to_string();
        // 2000 词约 2000 token，> 1024 但 < 4096（Opus 阈值）
        let profile = build_profile_from_request(&payload, 2000).unwrap();
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let usage = cache.compute("acc1", &profile);
        assert_eq!(usage.cache_creation, 0, "Opus 下 2000 token 低于 4096 阈值");
    }

    /// 实测：haiku 的 minimumTokensPerCacheCheckpoint 也是 4096（不是 1024）。
    #[test]
    fn haiku_uses_4096_threshold() {
        let mut payload = mk_request(Some(vec![sys(&big_text(2000), true)]), vec![]);
        payload.model = "claude-haiku-4-5".to_string();
        let profile = build_profile_from_request(&payload, 2000).unwrap();
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let usage = cache.compute("acc1", &profile);
        assert_eq!(
            usage.cache_creation, 0,
            "haiku 下 2000 token 低于 4096 阈值（实测 haiku=4096）"
        );
    }

    /// sonnet 阈值是 1024：2000 token 应可缓存。
    #[test]
    fn sonnet_uses_1024_threshold() {
        let mut payload = mk_request(Some(vec![sys(&big_text(2000), true)]), vec![]);
        payload.model = "claude-sonnet-4-5".to_string();
        let profile = build_profile_from_request(&payload, 2000).unwrap();
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let usage = cache.compute("acc1", &profile);
        assert!(
            usage.cache_creation > 0,
            "sonnet 下 2000 token 高于 1024 阈值，应可缓存"
        );
    }

    #[test]
    fn ttl_breakdown_1h() {
        let payload = mk_request(
            Some(vec![SystemMessage {
                text: big_text(5000),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some("1h".to_string()),
                }),
            }]),
            vec![],
        );
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let usage = cache.compute("acc1", &profile);
        assert!(usage.creation_1h > 0, "1h 标记应进 1h 桶");
        assert_eq!(usage.creation_5m, 0);
    }

    #[test]
    fn conversation_id_reuse() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        assert!(cache.lookup_conversation(&profile).is_none());
        cache.update("acc1", &profile, "conv-xyz");
        assert_eq!(
            cache.lookup_conversation(&profile).as_deref(),
            Some("conv-xyz")
        );
    }

    #[test]
    fn disabled_cache_returns_empty() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), false);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        let usage = cache.compute("acc1", &profile);
        assert_eq!(usage.cache_creation, 0);
        assert_eq!(usage.cache_read, 0);
    }

    #[test]
    fn max_cache_ratio_caps_read() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        // profile.total_input_tokens = max(传入值, 实际累积) —— 用它做 85% 基准
        let total = profile.total_input_tokens;
        let _ = cache.compute("acc1", &profile);
        cache.update("acc1", &profile, "conv-1");
        let usage = cache.compute("acc1", &profile);
        let cap = (total as f64 * MAX_CACHE_RATIO).floor() as i32;
        assert!(
            usage.cache_read <= cap,
            "cache_read 应被 85% 上限封顶: read={} cap={} total={}",
            usage.cache_read,
            cap,
            total
        );
        assert!(usage.cache_read > 0, "应有命中");
    }

    #[test]
    fn normalize_strips_environment_block() {
        let text = "You are Claude.\n\
                    # Environment\n\
                    Working directory: /home/user/project\n\
                    Platform: linux\n\
                    \n\
                    # Instructions\n\
                    Be helpful.\n";
        let n = normalize_system_text(text);
        assert!(n.contains("You are Claude."));
        assert!(n.contains("# Instructions"));
        assert!(n.contains("Be helpful."));
        assert!(!n.contains("Working directory"));
        assert!(n.contains("<DYNAMIC>"));
    }

    #[test]
    fn normalize_collapses_two_clients_to_same_profile() {
        let text_a = "x-anthropic-billing-header: cc_version=2.1.0;\n\
                      Be precise.\n\
                      # Environment\n\
                      Working directory: /home/alice\n";
        let text_b = "x-anthropic-billing-header: cc_version=2.1.144;\n\
                      Be precise.\n\
                      # Environment\n\
                      Working directory: /home/bob/project\n";
        let pa = mk_request(Some(vec![sys(text_a, true)]), vec![]);
        let pb = mk_request(Some(vec![sys(text_b, true)]), vec![]);
        let fa = build_profile_from_request(&pa, 5000)
            .unwrap()
            .stable_fingerprint;
        let fb = build_profile_from_request(&pb, 5000)
            .unwrap()
            .stable_fingerprint;
        assert_eq!(fa, fb, "归一化后两个客户端应得到相同 fingerprint");
    }
}
