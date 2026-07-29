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
//! 本模块额外维护一个 `(account, fingerprint) → conversation_id` 映射（最稳定断点优先），
//! 命中时仅在同一 account 内复用上次的 conversation_id，让上游 Kiro 的 session 缓存
//! 有机会生效，同时避免多用户共用一个代理 key 时串会话。
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
//! - token 数为本地估算（`token_count::count_tokens`），非上游精确值。

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
/// 命中上限比例（最新内容不可能 100% 命中）—— 作用于**真实**模拟命中。
pub(crate) const MAX_CACHE_RATIO: f64 = 0.85;
/// 上报命中率系数（运营口径）的上限。
///
/// 与 [`MAX_CACHE_RATIO`] 分离:真实模拟受 0.85 物理约束(最新内容必为全价),
/// 但运营口径允许上报到 0.95(下游计费系统能接受的最高稳定命中率)。
const PERCEIVED_MAX_RATIO: f64 = 0.95;
/// 后台清理最小间隔
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// 测试用桶名；conversation_id 复用仍按此 account 维度隔离（accounting 已全局化）。
#[cfg(test)]
pub const GLOBAL_ACCOUNT: &str = "_global";

fn conversation_key(account: &str, fingerprint: &str) -> String {
    format!("{account}\u{1f}{fingerprint}")
}

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
}

#[derive(Debug, Clone, Copy)]
enum EventKind {
    Hit,
    Miss,
}

/// 内部状态
///
/// **accounting 桶（`entries`）已从 per-account 改为全局内容寻址。** 指纹是
/// `prelude(model/tool_choice) → tools → system → messages` 的累积 SHA-256，
/// 指纹相同即内容逐字节相同，跨请求共享缓存判定**不泄露任何内容**，且能修复
/// "客户端 `metadata.user_id` 经网关漂移 → 每次落空桶 → 只创建不读取" 的问题。
///
/// `conversation_by_fingerprint`（conversation_id 复用）**仍按 account 隔离**：
/// 它会让上游 Kiro 复用同一 session，串号会泄露会话历史，必须按 client session 锁。
struct CacheInner {
    /// 全局 accounting 桶：fingerprint → entry（不再按 account 分桶）
    entries: HashMap<String, CacheEntry>,
    /// `(account, 最稳定断点 fingerprint)` → 上次该 prefix 的 conversation_id
    conversation_by_fingerprint: HashMap<String, (String, Instant)>,
    capacity: usize,
    ttl: Duration,
    last_prune: Instant,
    hit_total: u64,
    miss_total: u64,
    eviction_total: u64,
    /// 滑动窗口事件 `(time, kind, real_saved, reported_saved)`。
    /// `kind`/`real_saved` 是**真实**模拟命中（诚实，运维诊断用）；
    /// `reported_saved` 是应用 perceived 系数后**对客户端上报**的 read（对账用）。
    recent_events: Vec<(Instant, EventKind, i32, i32)>,
}

impl CacheInner {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
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
        self.entries.len()
    }

    /// 记录一次缓存判定。`real_saved` = 真实命中节省；`reported_saved` = 上报口径节省。
    fn record_event(
        &mut self,
        now: Instant,
        kind: EventKind,
        real_saved: i32,
        reported_saved: i32,
    ) {
        if self.recent_events.len() >= 2048 {
            let drain_to = self.recent_events.len() - 1024;
            self.recent_events.drain(..drain_to);
        }
        self.recent_events
            .push((now, kind, real_saved, reported_saved));
    }

    fn stats_in_window(&self, now: Instant, window: Duration) -> WindowStats {
        let mut hits = 0u64;
        let mut misses = 0u64;
        let mut saved_tokens: i64 = 0;
        let mut reported_hits = 0u64;
        let mut reported_saved: i64 = 0;
        for (t, kind, saved, rep) in self.recent_events.iter().rev() {
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
            // 上报口径：reported_saved>0 即视为一次"上报命中"（与真实 kind 无关）
            if *rep > 0 {
                reported_hits += 1;
                reported_saved = reported_saved.saturating_add(*rep as i64);
            }
        }
        WindowStats {
            hits,
            misses,
            saved_input_tokens: saved_tokens,
            reported_hits,
            reported_saved_input_tokens: reported_saved,
        }
    }

    /// 周期性清理过期 entry 与 conversation 映射
    fn prune_if_needed(&mut self, now: Instant) {
        if now.duration_since(self.last_prune) < PRUNE_INTERVAL {
            return;
        }
        self.last_prune = now;
        self.entries.retain(|_, e| e.expires_at > now);
        // conversation 映射用最长 TTL（1h）兜底过期
        self.conversation_by_fingerprint
            .retain(|_, (_, created)| now.duration_since(*created) < ONE_HOUR_TTL);
    }

    /// 缩容：全局 accounting 桶超过 capacity 时按 expires_at 淘汰最旧
    fn enforce_capacity(&mut self) {
        let cap = self.capacity.max(1);
        if self.entries.len() <= cap {
            return;
        }
        let mut sorted: Vec<(String, Instant)> = self
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.expires_at))
            .collect();
        sorted.sort_by_key(|(_, exp)| *exp);
        let to_remove = self.entries.len() - cap;
        for (k, _) in sorted.into_iter().take(to_remove) {
            self.entries.remove(&k);
            self.eviction_total = self.eviction_total.saturating_add(1);
        }
    }
}

/// 单个时间窗口的统计
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowStats {
    pub hits: u64,
    pub misses: u64,
    pub saved_input_tokens: i64,
    /// 上报口径命中次数（应用 perceived 系数后 reported read>0 的请求数）
    pub reported_hits: u64,
    /// 上报口径节省 input tokens（对客户端/下游计费可见）
    pub reported_saved_input_tokens: i64,
}

impl WindowStats {
    /// 真实命中率（运维诊断口径）：真实命中 / 总请求。
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 * 100.0 / total as f64
        }
    }

    /// 上报命中率（业务对账口径）：上报命中 / 总请求。
    /// 总请求分母与真实口径相同（hits+misses），保证两个口径可比。
    pub fn reported_hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.reported_hits as f64 * 100.0 / total as f64
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
    /// 上报命中率系数（运营/计费口径）。`None` = 不干预，按真实模拟值上报。
    /// `Some(r)`（夹到 `[0.0, PERCEIVED_MAX_RATIO]`）：用于 Admin 对账指标；
    /// 真正返回给客户端的 fake usage 在 handler 的 client-visible usage 层生成。详见
    /// [`Config::perceived_cache_hit_ratio`](crate::model::config::Config)。
    perceived_ratio: Arc<parking_lot::RwLock<Option<f64>>>,
}

impl PromptCache {
    pub fn new(capacity: usize, ttl: Duration, enabled: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheInner::new(capacity, ttl))),
            enabled: Arc::new(parking_lot::RwLock::new(enabled)),
            perceived_ratio: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// 带「上报命中率系数」的构造（运营口径，详见 [`Config::perceived_cache_hit_ratio`]）。
    pub fn new_with_perceived(
        capacity: usize,
        ttl: Duration,
        enabled: bool,
        perceived_ratio: Option<f64>,
    ) -> Self {
        let cache = Self::new(capacity, ttl, enabled);
        cache.set_perceived_ratio(perceived_ratio);
        cache
    }

    pub fn is_enabled(&self) -> bool {
        *self.enabled.read()
    }

    pub fn set_enabled(&self, v: bool) {
        *self.enabled.write() = v;
    }

    /// 当前上报命中率系数（已夹到合法范围）。
    pub fn perceived_ratio(&self) -> Option<f64> {
        *self.perceived_ratio.read()
    }

    /// 设置上报命中率系数。`Some(r)` 夹到 `[0.0, PERCEIVED_MAX_RATIO]`；
    /// `<= 0.0` 视为关闭（None 语义）。
    pub fn set_perceived_ratio(&self, ratio: Option<f64>) {
        let normalized = ratio.and_then(|r| {
            if r <= 0.0 {
                None
            } else {
                Some(r.min(PERCEIVED_MAX_RATIO))
            }
        });
        *self.perceived_ratio.write() = normalized;
    }

    pub fn set_capacity(&self, capacity: usize) {
        let mut inner = self.inner.lock();
        inner.capacity = capacity.max(1);
        inner.enforce_capacity();
    }

    pub fn set_ttl(&self, ttl: Duration) {
        let mut inner = self.inner.lock();
        inner.ttl = ttl;
    }

    /// 计算命中情况（纯读路径，不写 entry、不刷新 TTL、不记录统计）。
    ///
    /// 只有上游成功接受请求后，handler 才会调用 [`Self::record_success`] 与
    /// [`Self::update`]，避免 429/5xx 失败请求污染真实缓存诊断和命中率窗口。
    ///
    /// `account`：保留入参以兼容调用点与 conversation_id 复用维度；**accounting
    /// 命中判定已全局内容寻址**，不再按 account 隔离（指纹相同即内容逐字节相同，
    /// 跨请求共享不泄露内容，且修复 metadata.user_id 经网关漂移导致的"只创建不读取"）。
    pub fn compute(&self, _account: &str, profile: &CacheProfile) -> CacheUsage {
        if !self.is_enabled() || profile.breakpoints.is_empty() {
            return CacheUsage::default();
        }
        let now = Instant::now();
        let min_tokens = min_cacheable_tokens(&profile.model);
        let perceived = self.perceived_ratio();
        let mut inner = self.inner.lock();
        inner.prune_if_needed(now);

        let last = profile.breakpoints.last().unwrap();
        // 完整可缓存前缀（未经 85% 封顶）——运营口径系数的基数。
        let full_prefix = last.cumulative_tokens.min(profile.total_input_tokens);

        // 低于最小阈值的前缀完全不缓存（cache_read=0 且 cache_creation=0，全部计入
        // input_tokens）；强行上报任一非零会与官方行为矛盾、暴露中转身份。
        if full_prefix < min_tokens {
            return CacheUsage::default();
        }

        // 从后往前找最长命中断点（全局内容寻址桶）。命中与否决定这是"首次/未命中"
        // 还是"命中" —— 不再依赖全局桶是否非空（否则别的会话填了桶会让本会话首请求
        // 的 creation 被 85% 误封顶）。
        let mut matched_tokens = 0i32;
        for bp in profile.breakpoints.iter().rev() {
            if bp.cumulative_tokens < min_tokens {
                continue;
            }
            if let Some(entry) = inner.entries.get(&bp.fingerprint) {
                if entry.expires_at <= now {
                    continue;
                }
                matched_tokens = bp.cumulative_tokens.min(profile.total_input_tokens);
                break;
            }
        }

        if matched_tokens == 0 {
            // 首次/未命中：真实命中为 0，全部 creation（不封顶）。不能用运营系数把
            // MISS 伪装成 cache_read，否则首请求就显示 90%+ 命中，违反 Anthropic
            // prompt cache 语义,也会让下游对账误以为上游已真实复用。
            return finalize_usage(profile, full_prefix, 0, 0, None);
        }

        // 命中：85% 封顶仅约束真实模拟 read（最新内容不可能 100% 命中）。
        let max_cacheable = (profile.total_input_tokens as f64 * MAX_CACHE_RATIO).floor() as i32;
        let last_tokens = full_prefix.min(max_cacheable);
        if matched_tokens > last_tokens {
            matched_tokens = last_tokens;
        }

        // 命中路径才允许 perceived 抬高 read 比例；handler 会在最终客户端 usage 层
        // 做 fake billing（无匹配的大请求不会被从 MISS 翻成 HIT）。
        finalize_usage(profile, last_tokens, matched_tokens, full_prefix, perceived)
    }

    /// 上游成功后记录本次缓存统计。
    ///
    /// - `real_cache_read`：真实模拟命中的 token，用于真实 hit/miss 诊断。
    /// - `reported_cache_read`：最终对客户端上报的 cache_read，用于计费/对账口径。
    pub fn record_success(&self, real_cache_read: i32, reported_cache_read: i32) {
        if !self.is_enabled() {
            return;
        }
        let now = Instant::now();
        let real_read = real_cache_read.max(0);
        let reported_read = reported_cache_read.max(0);
        let mut inner = self.inner.lock();
        if real_read > 0 {
            inner.record_event(now, EventKind::Hit, real_read, reported_read);
            inner.hit_total = inner.hit_total.saturating_add(1);
        } else {
            inner.record_event(now, EventKind::Miss, 0, reported_read);
            inner.miss_total = inner.miss_total.saturating_add(1);
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

        // accounting 断点写入全局内容寻址桶（不再按 account 分桶）。
        for bp in &profile.breakpoints {
            if bp.cumulative_tokens < min_tokens {
                continue;
            }
            inner.entries.insert(
                bp.fingerprint.clone(),
                CacheEntry {
                    expires_at: now + bp.ttl,
                },
            );
        }
        inner.enforce_capacity();

        // conversation_id 复用映射（最稳定断点），必须按 account 隔离 —— 串号会泄露会话历史。
        if !profile.stable_fingerprint.is_empty() && !conversation_id.is_empty() {
            inner.conversation_by_fingerprint.insert(
                conversation_key(account, &profile.stable_fingerprint),
                (conversation_id.to_string(), now),
            );
        }
    }

    /// 查询可复用的 conversation_id（最稳定断点命中时返回）。
    pub fn lookup_conversation(&self, account: &str, profile: &CacheProfile) -> Option<String> {
        if !self.is_enabled() || profile.stable_fingerprint.is_empty() {
            return None;
        }
        let now = Instant::now();
        let inner = self.inner.lock();
        inner
            .conversation_by_fingerprint
            .get(&conversation_key(account, &profile.stable_fingerprint))
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
        inner.entries.clear();
        inner.conversation_by_fingerprint.clear();
    }

    /// 诊断探针：返回 `(全局桶内条目数, 命中的断点序号 from-end, 断点总数)`。
    ///
    /// 用于定位生产 "只创建不读取" 问题：
    /// - 桶内条目数恒为 0 → 从未 update（走 perceived/skip 路径，或上游一直失败）。
    /// - 桶内有条目但 matched=None → 指纹漂移（system 动态字段没归一化干净）。
    ///
    /// `matched` 为命中断点距末尾的偏移（0=最后一个断点命中），None=无命中。
    /// `account` 入参保留以兼容调用点；accounting 已全局化，命中判定与 account 无关。
    pub fn debug_probe(
        &self,
        _account: &str,
        profile: &CacheProfile,
    ) -> (usize, Option<usize>, usize) {
        let now = Instant::now();
        let min_tokens = min_cacheable_tokens(&profile.model);
        let inner = self.inner.lock();
        let bucket_len = inner.entries.len();
        let total_bp = profile.breakpoints.len();
        let mut matched = None;
        for (i, bp) in profile.breakpoints.iter().rev().enumerate() {
            if bp.cumulative_tokens < min_tokens {
                continue;
            }
            if let Some(e) = inner.entries.get(&bp.fingerprint) {
                if e.expires_at > now {
                    matched = Some(i);
                    break;
                }
            }
        }
        (bucket_len, matched, total_bp)
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

/// 把一次 compute 的结果组装成对外 [`CacheUsage`]，并（可选）应用「上报命中率系数」。
///
/// - `cacheable_total`：真实模拟口径的可缓存总量（命中上限 85% 后的 last_tokens）。
///   非 perceived 路径下 `read + creation = cacheable_total`。
/// - `real_read`：真实命中的 read token 数（首次为 0）。
/// - `perceived_base`：运营口径系数作用的基数 —— 用**未经 85% 封顶**的完整可缓存
///   前缀（`min(last_breakpoint, total_input)`）。这样 `perceived=0.9` 能真正把上报
///   read 抬到前缀的 90%，而不被真实模拟的 0.85 物理上限卡住。
/// - `perceived`：运营口径系数。`Some(r)` 时把 read 抬到
///   `max(real_read, perceived_base × r)`，并让 `read + creation = perceived_base`
///   （三字段仍互斥不重叠）。`None` 时退回真实模拟口径（基数 = cacheable_total）。
///
/// TTL 分桶基于最终 read 之后的区间，使 5m/1h creation 与上报 read 自洽。
fn finalize_usage(
    profile: &CacheProfile,
    cacheable_total: i32,
    real_read: i32,
    perceived_base: i32,
    perceived: Option<f64>,
) -> CacheUsage {
    let (base, read) = match perceived {
        Some(r) if perceived_base > 0 => {
            let floor = (perceived_base as f64 * r).floor() as i32;
            (perceived_base, real_read.max(floor).min(perceived_base))
        }
        _ => (cacheable_total, real_read),
    };
    let creation = (base - read).max(0);
    let (c5m, c1h) = compute_ttl_breakdown(profile, read);
    CacheUsage {
        cache_creation: creation,
        cache_read: read,
        creation_5m: c5m,
        creation_1h: c1h,
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

/// 把 JSON 值序列化成**键序规范化**(对象 key 递归字典序排序)的字符串,用于指纹。
///
/// 为什么必须规范化:本 crate 的 serde_json 开启了 `preserve_order` feature,
/// `Value::Object` 保留**传入的 key 顺序**。客户端/网关(new2api 是 Go,跨进程
/// 反序列化-再序列化会改变 key 顺序)每请求送来的 `input_schema` / tool_use.input /
/// tool_result 等对象 key 顺序可能不同 → 同一内容 `.to_string()` 出不同字节 →
/// 指纹漂移 → 累积链全毒化 → 每个断点都 MISS(线上实测 matched_bp 恒 None)。
///
/// 排序后键序与传入无关,相同内容必得相同指纹。对齐 chaogei/Quorinex 的
/// `canonicalize` / `writeCanonicalJSON`(它们正是靠这个稳定命中)。
fn canonical_json(value: &serde_json::Value) -> String {
    let mut buf = String::new();
    write_canonical(&mut buf, value);
    buf
}

fn write_canonical(buf: &mut String, v: &serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            buf.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                // key 本身按 JSON 字符串转义
                buf.push_str(&serde_json::Value::String((*k).clone()).to_string());
                buf.push(':');
                write_canonical(buf, &map[*k]);
            }
            buf.push('}');
        }
        serde_json::Value::Array(arr) => {
            buf.push('[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                write_canonical(buf, item);
            }
            buf.push(']');
        }
        // 标量(string/number/bool/null)无 key 顺序问题,直接用 serde 标准序列化
        other => buf.push_str(&other.to_string()),
    }
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
    let mut stable_fingerprint = String::new();

    // 请求前导（prelude）：先把改变上游 prefix-cache 身份的请求级字段混入指纹，
    // 再叠加内容块。否则同一段 system+history 在不同 model / tool_choice 下会共用
    // 同一指纹，导致跨模型误命中（cache_read 虚高）并可能复用错误的 conversation_id。
    // 不计入断点（无 cumulative_tokens / ttl），只影响后续所有断点的指纹值。
    let tool_choice_repr = payload
        .tool_choice
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_default();
    hash_chunk(&mut hasher, &format!("prelude\0model={}", payload.model));
    hash_chunk(&mut hasher, &format!("tool_choice={tool_choice_repr}"));

    // 全局缓存意图 TTL：请求里任意 cache_control 的 TTL（取首个出现的）。
    // 只要请求存在 cache_control，就把**每个 message 边界**都设为隐式断点 ——
    // 对齐 Anthropic「一个 cache_control 缓存整个 prefix、下一轮命中最长公共 prefix」的语义。
    // 这样多轮对话中 cache_control 滑到最后一条 message 时，前面历史 message 的边界
    // 仍是断点，能命中上一轮缓存的 prefix（否则 cache_read 恒为 0）。
    let global_ttl: Option<Duration> = blocks.iter().find_map(|b| b.ttl);

    for block in &blocks {
        hash_chunk(&mut hasher, &block.value);
        cumulative_tokens = cumulative_tokens.saturating_add(block.tokens);

        let breakpoint_ttl = if let Some(ttl) = block.ttl {
            Some(ttl)
        } else if block.is_message_end {
            // 每个 message 边界都是断点（前提是请求有缓存意图）
            global_ttl
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
            // 键序规范化:input_schema 是客户端送来的任意对象,key 顺序可能每请求不同
            // （preserve_order 会原样保留）。规范化后同一 schema 必得同一指纹。
            let value = canonical_json(&serde_json::json!({
                "kind": "tool",
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            }));
            let tokens = super::token_count::count_tokens(&value) as i32;
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
            let tokens = super::token_count::count_tokens(&normalized) as i32;
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
            let tokens = super::token_count::count_tokens(text) as i32;
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
                // fingerprint 只对“内容”敏感：剔除 cache_control 字段再序列化。
                // cache_control 是缓存指令而非内容，且多轮对话中它会“滑动”到最后一条
                // message —— 同一条历史 message 在不同轮里 cache_control 有无不同，
                // 若把它 hash 进 fingerprint，相同内容的 prefix 在不同轮 fingerprint 就不同，
                // 导致 cache_read 永远为 0（无法命中之前轮缓存的 prefix）。
                let fp_value = strip_cache_control(block);
                let value = format!("msg\0{}\0{}\0{}\0{}", msg.role, msg_index, i, fp_value);
                let tokens = super::token_count::count_tokens(&text) as i32;
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

/// 返回去掉 `cache_control` 字段后的 block **键序规范化**序列化字符串,用于稳定
/// fingerprint。键序规范化原因见 [`canonical_json`]:message block(tool_use.input /
/// tool_result content 等)的对象 key 顺序经网关可能每请求漂移,不规范化会让相同
/// 历史 message 在不同轮指纹不同 → 永远 MISS。非 object 也走规范化(标量原样)。
fn strip_cache_control(block: &serde_json::Value) -> String {
    match block.as_object() {
        Some(obj) if obj.contains_key("cache_control") => {
            let mut cloned = obj.clone();
            cloned.remove("cache_control");
            canonical_json(&serde_json::Value::Object(cloned))
        }
        _ => canonical_json(block),
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

    /// 回归(线上根因):`input_schema` 是 `HashMap<String,Value>`,Rust HashMap
    /// **每个实例随机迭代序**。即便客户端每次送来完全相同的 schema,每请求反序列化成
    /// 新 HashMap → `.to_string()` 出不同 key 顺序 → tool 指纹每请求漂移 → 累积链
    /// 毒化 → 所有断点 MISS(线上实测 matched_bp 恒 None,即便 name/desc/系统都稳定)。
    /// canonical_json 排序 key 后,无论 HashMap 迭代序如何,相同 schema 必得相同指纹。
    #[test]
    fn tool_input_schema_key_order_does_not_drift_fingerprint() {
        let big = big_text(5000);
        // 两次独立从 JSON 反序列化(input_schema → 两个不同随机种子的 HashMap),
        // 且源 JSON key 顺序也不同 —— 模拟跨请求的真实情况。
        let parse = |schema_json: serde_json::Value| -> MessagesRequest {
            serde_json::from_value(serde_json::json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 100,
                "system": [{"text": big, "cache_control": {"type": "ephemeral"}}],
                "tools": [{
                    "name": "edit",
                    "description": "edit a file",
                    "input_schema": schema_json
                }],
                "messages": []
            }))
            .unwrap()
        };
        let a = parse(serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
            "required": ["path", "content"]
        }));
        let b = parse(serde_json::json!({
            "required": ["path", "content"],
            "properties": {"content": {"type": "string"}, "path": {"type": "string"}},
            "type": "object"
        }));
        let fa = build_profile_from_request(&a, 5000).unwrap();
        let fb = build_profile_from_request(&b, 5000).unwrap();
        assert_eq!(
            fa.stable_fingerprint, fb.stable_fingerprint,
            "input_schema key 顺序/HashMap 迭代序不同但内容相同,规范化后指纹必须一致"
        );
        assert_eq!(
            fa.breakpoints.last().unwrap().fingerprint,
            fb.breakpoints.last().unwrap().fingerprint,
            "末断点指纹也必须一致(累积链不被 key 顺序毒化)"
        );
    }

    /// message block 的 key 顺序漂移(tool_use.input 等)同样不能破坏 prefix 命中。
    #[test]
    fn message_block_key_order_does_not_drift_fingerprint() {
        let ctx = big_text(6000);
        let mk = |blk: serde_json::Value| {
            mk_request(
                None,
                vec![Message {
                    role: "user".to_string(),
                    content: serde_json::json!([blk]),
                }],
            )
        };
        let a = mk(serde_json::json!({
            "type": "tool_result", "tool_use_id": "x", "content": ctx,
            "cache_control": {"type": "ephemeral"}
        }));
        // 同内容,object key 顺序打乱
        let b = mk(serde_json::json!({
            "cache_control": {"type": "ephemeral"},
            "content": ctx, "tool_use_id": "x", "type": "tool_result"
        }));
        let fa = build_profile_from_request(&a, 6000).unwrap();
        let fb = build_profile_from_request(&b, 6000).unwrap();
        assert_eq!(
            fa.breakpoints.last().unwrap().fingerprint,
            fb.breakpoints.last().unwrap().fingerprint,
            "message block key 顺序不同但内容相同,指纹必须一致"
        );
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
    fn compute_does_not_record_stats_until_success() {
        let cache =
            PromptCache::new_with_perceived(1024, Duration::from_secs(300), true, Some(0.92));
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();

        let first = cache.compute("acc1", &profile);
        let snap = cache.snapshot();
        assert_eq!(snap.hit_total, 0, "单纯 compute 不应记录 hit");
        assert_eq!(snap.miss_total, 0, "单纯 compute 不应记录 miss");
        assert_eq!(snap.last1m.reported_saved_input_tokens, 0);

        cache.record_success(first.cache_read, 4_600);
        let snap = cache.snapshot();
        assert_eq!(snap.hit_total, 0);
        assert_eq!(snap.miss_total, 1);
        assert_eq!(snap.last1m.hit_rate(), 0.0);
        assert_eq!(snap.last1m.reported_hit_rate(), 100.0);
        assert_eq!(snap.last1m.reported_saved_input_tokens, 4_600);

        cache.update("acc1", &profile, "conv-1");
        let second = cache.compute("acc1", &profile);
        assert!(second.cache_read > 0);
        let snap = cache.snapshot();
        assert_eq!(
            snap.hit_total, 0,
            "第二次 compute 命中也必须等上游成功后才记 hit"
        );
        cache.record_success(second.cache_read, second.cache_read);
        let snap = cache.snapshot();
        assert_eq!(snap.hit_total, 1);
        assert_eq!(snap.miss_total, 1);
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

    /// accounting 已全局内容寻址：相同 prefix 跨 account 应命中 cache_read（安全 ——
    /// 指纹相同即内容逐字节相同，不泄露内容）。但 conversation_id 复用仍按 account 隔离。
    #[test]
    fn accounting_is_global_but_conversation_isolated() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        let _ = cache.compute("acc1", &profile);
        cache.update("acc1", &profile, "conv-1");

        // accounting：不同 account、相同内容 → 命中 cache_read（全局内容寻址）。
        // 这正是修复点：客户端 metadata.user_id 经网关漂移成 acc2 也照样读到缓存。
        let usage = cache.compute("acc2", &profile);
        assert!(
            usage.cache_read > 0,
            "全局 accounting：相同 prefix 跨 account 应命中 cache_read"
        );

        // conversation_id 复用：仍按 account 隔离，acc2 不能复用 acc1 的 conversation_id。
        assert_eq!(
            cache.lookup_conversation("acc1", &profile).as_deref(),
            Some("conv-1")
        );
        assert!(
            cache.lookup_conversation("acc2", &profile).is_none(),
            "conversation_id 复用必须按 account 隔离（串号会泄露会话历史）"
        );
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
        // big_text(865) 在校准后(西文权重 1.85)约 2000 token，> 1024 但 < 4096（Opus 阈值）
        let mut payload = mk_request(Some(vec![sys(&big_text(865), true)]), vec![]);
        payload.model = "claude-opus-4-7".to_string();
        let profile = build_profile_from_request(&payload, 2000).unwrap();
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let usage = cache.compute("acc1", &profile);
        assert_eq!(usage.cache_creation, 0, "Opus 下 2000 token 低于 4096 阈值");
    }

    /// 实测：haiku 的 minimumTokensPerCacheCheckpoint 也是 4096（不是 1024）。
    #[test]
    fn haiku_uses_4096_threshold() {
        // big_text(865) 校准后约 2000 token，低于 haiku 的 4096 阈值
        let mut payload = mk_request(Some(vec![sys(&big_text(865), true)]), vec![]);
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
    fn perceived_ratio_does_not_lift_first_request() {
        // 首次请求真实 read=0；perceived 只能抬高真实命中，不能伪造 R1 命中。
        let cache =
            PromptCache::new_with_perceived(1024, Duration::from_secs(300), true, Some(0.9));
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        let total = profile
            .total_input_tokens
            .min(profile.breakpoints.last().unwrap().cumulative_tokens);
        let usage = cache.compute("acc1", &profile);
        assert_eq!(usage.cache_read, 0, "首次请求不能上报 cache_read");
        // 三字段互斥：read + creation == cacheable 总量
        assert_eq!(usage.cache_read + usage.cache_creation, total);
        assert!(usage.cache_creation > 0);
    }

    #[test]
    fn perceived_ratio_lifts_only_real_hits() {
        let cache =
            PromptCache::new_with_perceived(1024, Duration::from_secs(300), true, Some(0.9));
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        let total = profile
            .total_input_tokens
            .min(profile.breakpoints.last().unwrap().cumulative_tokens);
        let first = cache.compute("acc1", &profile);
        assert_eq!(first.cache_read, 0);
        cache.update("acc1", &profile, "conv-real-hit");

        let second = cache.compute("acc1", &profile);
        assert!(
            second.cache_read >= (total as f64 * 0.9).floor() as i32,
            "perceived=0.9 应只在真实命中后把 read 抬到 ≥90%: read={} total={}",
            second.cache_read,
            total
        );
        assert_eq!(second.cache_read + second.cache_creation, total);
    }

    #[test]
    fn perceived_ratio_does_not_lift_unmatched_prompt() {
        let cache =
            PromptCache::new_with_perceived(1024, Duration::from_secs(300), true, Some(0.9));
        let cached = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let pcached = build_profile_from_request(&cached, 5000).unwrap();
        let _ = cache.compute("acc1", &pcached);
        cache.update("acc1", &pcached, "conv-cached");

        let unrelated_text = format!("different {}", "term ".repeat(5000));
        let unrelated = mk_request(Some(vec![sys(&unrelated_text, true)]), vec![]);
        let punrelated = build_profile_from_request(&unrelated, 5000).unwrap();
        let usage = cache.compute("acc1", &punrelated);
        assert_eq!(usage.cache_read, 0, "无匹配断点时不能被 perceived 虚抬");
        assert!(
            usage.cache_creation > 0,
            "大请求未命中时应计入 cache creation"
        );
    }

    #[test]
    fn perceived_ratio_none_keeps_real_value() {
        // 不设系数 → 首次真实 read=0
        let cache = PromptCache::new_with_perceived(1024, Duration::from_secs(300), true, None);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        let usage = cache.compute("acc1", &profile);
        assert_eq!(usage.cache_read, 0, "无系数时首次 read 应为 0");
        assert!(usage.cache_creation > 0);
    }

    #[test]
    fn perceived_ratio_clamped_and_disabled() {
        // 设 0.99 应被夹到 PERCEIVED_MAX_RATIO(0.95)
        let cache =
            PromptCache::new_with_perceived(1024, Duration::from_secs(300), true, Some(0.99));
        assert_eq!(cache.perceived_ratio(), Some(PERCEIVED_MAX_RATIO));
        // 设 <=0 视为关闭
        cache.set_perceived_ratio(Some(0.0));
        assert_eq!(cache.perceived_ratio(), None);
        cache.set_perceived_ratio(Some(-1.0));
        assert_eq!(cache.perceived_ratio(), None);
    }

    #[test]
    fn perceived_ratio_respects_min_threshold_on_hit_path() {
        // 回归：桶里已有条目时，低于最小阈值的小请求不应被运营系数虚抬到 92%。
        // 真端点低于 minimumTokensPerCacheCheckpoint 报 cache=0，强行上报会暴露中转。
        let cache =
            PromptCache::new_with_perceived(1024, Duration::from_secs(300), true, Some(0.92));
        // 先用大 prompt 填充桶（sonnet 阈值 1024）
        let big = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let pbig = build_profile_from_request(&big, 5000).unwrap();
        let _ = cache.compute(GLOBAL_ACCOUNT, &pbig);
        cache.update(GLOBAL_ACCOUNT, &pbig, "conv-big");
        // 再发一个低于阈值的小请求（带 cache_control），桶非空 → 走 hit 路径
        let small = mk_request(Some(vec![sys("short prompt here", true)]), vec![]);
        let psmall = build_profile_from_request(&small, 50).unwrap();
        let usage = cache.compute(GLOBAL_ACCOUNT, &psmall);
        assert_eq!(
            usage.cache_read, 0,
            "低于最小阈值的小请求不应被运营系数虚抬 read，实际 read={}",
            usage.cache_read
        );
        assert_eq!(usage.cache_creation, 0, "低于阈值也不计 creation");
    }

    #[test]
    fn conversation_id_reuse() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let payload = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let profile = build_profile_from_request(&payload, 5000).unwrap();
        assert!(cache.lookup_conversation("acc1", &profile).is_none());
        cache.update("acc1", &profile, "conv-xyz");
        assert_eq!(
            cache.lookup_conversation("acc1", &profile).as_deref(),
            Some("conv-xyz")
        );
        assert!(
            cache.lookup_conversation("acc2", &profile).is_none(),
            "conversation_id 不能跨 account 复用"
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

    /// 回归：多轮对话中 cache_control 在 message 之间“滑动”，相同历史 message 的
    /// fingerprint 必须稳定（不含 cache_control），否则 R2 无法命中 R1 缓存的 prefix，
    /// 表现为 cache_read 恒为 0。
    #[test]
    fn sliding_cache_control_does_not_break_prefix_hit() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let ctx = big_text(6000); // >4096，确保超 opus/sonnet 阈值

        // R1: 单条 user message（数组形式），cache_control 打在该 block 上
        let r1 = mk_request(
            None,
            vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type":"text","text":ctx,"cache_control":{"type":"ephemeral"}}
                ]),
            }],
        );
        let p1 = build_profile_from_request(&r1, 6000).unwrap();
        let u1 = cache.compute(GLOBAL_ACCOUNT, &p1);
        cache.update(GLOBAL_ACCOUNT, &p1, "conv-1");
        assert!(
            u1.cache_creation > 0 && u1.cache_read == 0,
            "R1 首次全 creation"
        );

        // R2: 历史 message 内容完全相同，但 cache_control 已“滑”到新的 user message。
        // 第一条 user message 现在**不带** cache_control。
        let r2 = mk_request(
            None,
            vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type":"text","text":ctx}  // 同内容，cache_control 已移除
                    ]),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("ok"),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type":"text","text":"next question","cache_control":{"type":"ephemeral"}}
                    ]),
                },
            ],
        );
        let p2 = build_profile_from_request(&r2, 6100).unwrap();
        let u2 = cache.compute(GLOBAL_ACCOUNT, &p2);
        assert!(
            u2.cache_read > 0,
            "R2 应命中 R1 缓存的 prefix（cache_read>0），实际 read={} creation={}",
            u2.cache_read,
            u2.cache_creation
        );
    }

    /// 回归（审计 P1）：指纹包含请求前导 model/tool_choice。
    /// 同一段可缓存 prefix 在不同 model 下指纹必须不同，避免跨模型误命中。
    #[test]
    fn fingerprint_differs_by_model() {
        let mut pa = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let mut pb = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        pa.model = "claude-opus-4-5".to_string();
        pb.model = "claude-sonnet-4-5".to_string();

        let fa = build_profile_from_request(&pa, 5000).unwrap();
        let fb = build_profile_from_request(&pb, 5000).unwrap();
        assert_ne!(
            fa.stable_fingerprint, fb.stable_fingerprint,
            "不同 model 的相同 prefix 必须有不同 stable_fingerprint"
        );
        assert_ne!(
            fa.breakpoints[0].fingerprint, fb.breakpoints[0].fingerprint,
            "不同 model 的断点指纹也必须不同"
        );
    }

    /// 跨模型不应在同一 account 桶里互相命中（指纹隔离的端到端验证）。
    #[test]
    fn no_cross_model_cache_hit() {
        let cache = PromptCache::new(1024, Duration::from_secs(300), true);
        let mut opus = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        opus.model = "claude-opus-4-5".to_string();

        // opus 先写入缓存
        let p_opus = build_profile_from_request(&opus, 5000).unwrap();
        let _ = cache.compute("acc1", &p_opus);
        cache.update("acc1", &p_opus, "conv-opus");

        // 同 account、同 prefix，但模型换成 sonnet：不应命中 opus 的缓存
        let mut sonnet = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        sonnet.model = "claude-sonnet-4-5".to_string();
        let p_sonnet = build_profile_from_request(&sonnet, 5000).unwrap();
        let usage = cache.compute("acc1", &p_sonnet);
        assert_eq!(
            usage.cache_read, 0,
            "不同模型不应命中对方缓存桶，read 应为 0"
        );
        assert!(
            cache.lookup_conversation("acc1", &p_sonnet).is_none(),
            "不同模型不应复用对方的 conversation_id"
        );
    }

    /// 不同 tool_choice 也应区分指纹（防止工具决策语义不同的请求误命中）。
    #[test]
    fn fingerprint_differs_by_tool_choice() {
        let mut none_tc = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        let mut auto_tc = mk_request(Some(vec![sys(&big_text(5000), true)]), vec![]);
        none_tc.tool_choice = None;
        auto_tc.tool_choice = Some(serde_json::json!({"type": "auto"}));

        let fa = build_profile_from_request(&none_tc, 5000).unwrap();
        let fb = build_profile_from_request(&auto_tc, 5000).unwrap();
        assert_ne!(
            fa.stable_fingerprint, fb.stable_fingerprint,
            "不同 tool_choice 的相同 prefix 应有不同指纹"
        );
    }
}
