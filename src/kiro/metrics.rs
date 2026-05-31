//! 请求级运行时指标记录
//!
//! 在 [`KiroProvider`](crate::kiro::provider::KiroProvider) 的每次外部调用
//! 入口/出口处记录 [`RequestRecord`]，写入固定容量的环形缓冲区。Admin API
//! 通过 [`MetricsRecorder::compute_window_stats`] 在请求时按"过去 N 秒"
//! 滑窗扫一次缓冲区算分位/计数，无需后台任务。
//!
//! 缓冲容量 [`RING_CAPACITY`] = 8192，按 1 req/s 计可容纳 ~2.3 小时历史；
//! 在低 QPS（< 0.1 req/s）场景能完整覆盖 24h 窗口。
//!
//! [`RequestRecord`] 携带 `model` 与 `credential_id`，admin
//! [`compute_by_model_stats`] / [`compute_by_credential_stats`] 可对窗口内
//! 数据按维度切片，便于排查"哪个模型慢/哪个号在打瞬态"。

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// 请求成败分类（与 admin metrics 输出对齐）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// 2xx 成功
    Success,
    /// 上游瞬态错误（429/408/5xx）经多次 retry 仍失败
    TransientFail,
    /// 凭据/请求级永久错误（400/401/403、配置错误等）或所有 retry 都拿不到 token
    Error,
}

/// 单条请求的完整生命周期记录
#[derive(Debug, Clone)]
pub struct RequestRecord {
    /// 单调递增序号（由 recorder 在 push 时分配，调用方传入的占位值会被覆盖）
    ///
    /// 用于 token 后填：record() 返回此 seq，anthropic 层流结束后凭 seq
    /// 调 update_tokens 把 input/output/cache_read 补回对应 slot。
    /// slot 若已被环形覆盖（seq 不匹配）则安全跳过。
    pub seq: u64,
    /// 请求结束时刻（用 Instant 比 unix 更稳定，admin 计算时与 now 比较）
    pub finished_at: Instant,
    /// 端到端耗时
    pub latency: Duration,
    /// 最终成败分类
    pub kind: RequestKind,
    /// 整个请求过程中是否经历了"全员 cooldown 智能等待"路径
    pub waited_for_cooldown: bool,
    /// 整个请求过程中是否被迫用过 fallback 凭据（即 cooldown 中的号）
    pub used_fallback: bool,
    /// 上行模型名（e.g. `claude-sonnet-4-20250514`），admin metrics 用于 by_model 聚合
    ///
    /// 用 `Arc<str>` 而不是 `String`：相同模型名共享同一份内存，2048 条记录的额外开销
    /// ≈ 2048 × 16 bytes（指针+原子计数）。
    pub model: Option<Arc<str>>,
    /// 最后实际命中的凭据 ID（fallback 路径下是 fallback 号 ID）。
    /// 用于 admin metrics 的 by_credential 聚合。
    pub credential_id: Option<u64>,
    /// 首字节延迟（毫秒）：流式请求从 provider.call_api_stream 返回到收到第一个上游 chunk
    ///
    /// 非流式请求填 None；流式请求未抓到首字节即失败时也填 None。
    /// 用于 admin metrics 的 TTFB 分位计算，是流式 UX 核心指标。
    pub ttfb_ms: Option<u64>,
    /// 流式请求是否中途断流（上游 connection broken mid-stream）
    ///
    /// 仅流式请求有意义。检测点：`bytes_stream` 返回 `Some(Err(_))`。
    /// 标记为 true 不意味着客户端看到错误——可能被 R1.3 的 fallback 救活了。
    pub stream_aborted_midway: bool,
    /// 流式中断后通过"换号/非流式 fallback"成功救活（客户端看到完整响应）
    ///
    /// 仅当 `stream_aborted_midway = true` 才有意义。配合两者算救援成功率。
    pub stream_recovered: bool,
    /// 客户端最终看到错误（非 200 / 流式不完整结束）
    ///
    /// 与 `kind = TransientFail/Error` 的区别：
    /// - 内部 retry 全失败但 R1.3 fallback 救活 → kind=Success, client_visible_error=false
    /// - 内部 retry 全失败且救援也失败 → kind=TransientFail, client_visible_error=true
    pub client_visible_error: bool,
    /// 触发了 Tier 化代理 fallback（attempt >= fallback_proxy_after_attempts）
    ///
    /// 标记该请求至少有一次重试走了 mihomo 出口。配合 metrics 看代理触发率。
    pub used_fallback_proxy: bool,
    /// 输入 token 数（来自 Anthropic `usage.input_tokens`）
    ///
    /// 用于 RPM/TPM 时间序列统计。流式从末帧汇总；非流式从 body usage 解析。
    /// 拿不到时为 None（如错误响应、cancellation 等）。
    pub input_tokens: Option<u32>,
    /// 输出 token 数（来自 Anthropic `usage.output_tokens`）
    pub output_tokens: Option<u32>,
    /// 缓存读取 token 数（来自 Anthropic `usage.cache_read_input_tokens`）
    ///
    /// 这部分 token 不计费/计费少，反映 prompt cache 命中节省量。
    pub cache_read_tokens: Option<u32>,
}

/// 环形缓冲容量。8192 × ~80 bytes ≈ 640 KB；admin 聚合时一次性 clone。
const RING_CAPACITY: usize = 8192;

/// 进程级请求指标记录器
///
/// 内部用 [`parking_lot::Mutex`] 守护一个简单的 ring buffer。写入是 O(1)；
/// 读端在 admin API 每次访问时复制整个 buffer（最多 2048 个 32-byte 结构 = 64KB）
/// 后释放锁，再在副本上做窗口聚合，避免长持锁影响请求路径。
pub struct MetricsRecorder {
    buf: Mutex<RingBuffer>,
    started_at: Instant,
}

struct RingBuffer {
    data: Vec<RequestRecord>,
    /// 下一个写入位置（始终 < capacity，会环绕）
    head: usize,
    /// 是否已绕回过一圈（决定 data 长度是否已满）
    wrapped: bool,
    /// 下一个分配的单调序号（永不回绕，用于 token 后填定位）
    next_seq: u64,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
            head: 0,
            wrapped: false,
            next_seq: 0,
        }
    }

    /// 写入一条记录，分配并返回其单调序号 `seq`
    fn push(&mut self, mut rec: RequestRecord) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        rec.seq = seq;
        let cap = self.data.capacity();
        if self.data.len() < cap {
            self.data.push(rec);
        } else {
            self.data[self.head] = rec;
        }
        self.head += 1;
        if self.head >= cap {
            self.head = 0;
            self.wrapped = true;
        }
        seq
    }

    /// 按 seq 定位仍存活的 slot 并补填 token（流结束后由 anthropic 层调用）
    ///
    /// seq → slot 的映射：插入顺序与 seq 一致，data 满之前 slot==seq，
    /// 满之后 slot==seq % cap。校验 slot 内记录的 seq 是否匹配，
    /// 不匹配说明该 slot 已被更新的请求覆盖（高并发下旧请求迟到），安全跳过。
    fn update_tokens(
        &mut self,
        seq: u64,
        input: Option<u32>,
        output: Option<u32>,
        cache_read: Option<u32>,
    ) {
        let cap = self.data.capacity();
        if cap == 0 {
            return;
        }
        let slot = (seq % cap as u64) as usize;
        if let Some(rec) = self.data.get_mut(slot) {
            if rec.seq == seq {
                rec.input_tokens = input;
                rec.output_tokens = output;
                rec.cache_read_tokens = cache_read;
            }
        }
    }

    fn snapshot(&self) -> Vec<RequestRecord> {
        // 直接克隆全量；上限 ~64KB，开销可忽略
        self.data.clone()
    }

    fn total(&self) -> usize {
        self.data.len()
    }
}

impl MetricsRecorder {
    pub fn new() -> Self {
        Self {
            buf: Mutex::new(RingBuffer::new(RING_CAPACITY)),
            started_at: Instant::now(),
        }
    }

    /// 进程启动时刻
    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    /// 记录一次请求结果，返回其单调序号 `seq`
    ///
    /// 调用方可凭 seq 在请求后期（如流式 SSE 解析完成）调 [`Self::update_tokens`]
    /// 把最终 token 用量补填回该记录。
    pub fn record(&self, rec: RequestRecord) -> u64 {
        self.buf.lock().push(rec)
    }

    /// 按 seq 补填 token 用量（input / output / cache_read）
    ///
    /// 用于 token 在记录时尚不可知的路径：provider 层流式请求在**建连完成**时
    /// 就 record 了（此时 token 还没产生），由 anthropic 层在流结束后回填。
    /// slot 若已被环形覆盖则静默跳过（见 [`RingBuffer::update_tokens`]）。
    pub fn update_tokens(
        &self,
        seq: u64,
        input: Option<u32>,
        output: Option<u32>,
        cache_read: Option<u32>,
    ) {
        self.buf.lock().update_tokens(seq, input, output, cache_read);
    }

    /// 当前缓冲已记录的请求总数（被环形覆盖前）
    pub fn buffer_len(&self) -> usize {
        self.buf.lock().total()
    }

    /// 复制当前 buffer 全量（admin API 用，无需长持锁）
    pub fn snapshot(&self) -> Vec<RequestRecord> {
        self.buf.lock().snapshot()
    }
}

impl Default for MetricsRecorder {
    fn default() -> Self {
        Self::new()
    }
}

/// token 后填句柄
///
/// 由 provider 层在 record 后返回，携带 recorder 与该记录的 seq。
/// anthropic 层在流式/非流式响应处理完成、拿到最终 token 后调
/// [`Self::update_tokens`] 把用量补回。
#[derive(Clone)]
pub struct RecordHandle {
    recorder: Arc<MetricsRecorder>,
    seq: u64,
}

impl RecordHandle {
    /// 构造一个携带 seq 的句柄
    pub fn new(recorder: Arc<MetricsRecorder>, seq: u64) -> Self {
        Self { recorder, seq }
    }

    /// 补填最终 token 用量
    pub fn update_tokens(&self, input: Option<u32>, output: Option<u32>, cache_read: Option<u32>) {
        self.recorder
            .update_tokens(self.seq, input, output, cache_read);
    }
}

/// 给定一组 record 副本，按"过去 `window`"过滤并计算窗口统计
///
/// 包含端到端延迟分位、TTFB 分位、各类计数（fallback / waited / stream abort
/// / client visible error / fallback proxy）与 token 总量（input/output/cache_read）。
/// 所有分位用于"成功 + 失败"全集的端到端耗时（更接近用户感知）。
pub fn compute_window_stats(
    records: &[RequestRecord],
    now: Instant,
    window: Duration,
) -> WindowStats {
    let cutoff = now.checked_sub(window).unwrap_or(now);
    let mut latencies: Vec<u64> = Vec::with_capacity(records.len());
    let mut ttfb_samples: Vec<u64> = Vec::new();
    let mut count = 0u64;
    let mut success = 0u64;
    let mut transient = 0u64;
    let mut error = 0u64;
    let mut fallback_used = 0u64;
    let mut waited = 0u64;
    let mut stream_aborts = 0u64;
    let mut stream_recovers = 0u64;
    let mut client_visible_errors = 0u64;
    let mut fallback_proxy_used = 0u64;
    let mut input_tokens_total = 0u64;
    let mut output_tokens_total = 0u64;
    let mut cache_read_tokens_total = 0u64;
    for r in records {
        if r.finished_at < cutoff {
            continue;
        }
        count += 1;
        latencies.push(r.latency.as_millis() as u64);
        match r.kind {
            RequestKind::Success => success += 1,
            RequestKind::TransientFail => transient += 1,
            RequestKind::Error => error += 1,
        }
        if r.used_fallback {
            fallback_used += 1;
        }
        if r.waited_for_cooldown {
            waited += 1;
        }
        if let Some(ttfb) = r.ttfb_ms {
            ttfb_samples.push(ttfb);
        }
        if r.stream_aborted_midway {
            stream_aborts += 1;
        }
        if r.stream_recovered {
            stream_recovers += 1;
        }
        if r.client_visible_error {
            client_visible_errors += 1;
        }
        if r.used_fallback_proxy {
            fallback_proxy_used += 1;
        }
        if let Some(t) = r.input_tokens {
            input_tokens_total = input_tokens_total.saturating_add(t as u64);
        }
        if let Some(t) = r.output_tokens {
            output_tokens_total = output_tokens_total.saturating_add(t as u64);
        }
        if let Some(t) = r.cache_read_tokens {
            cache_read_tokens_total = cache_read_tokens_total.saturating_add(t as u64);
        }
    }
    latencies.sort_unstable();
    let p_of = |samples: &[u64], q: f64| -> u64 {
        if samples.is_empty() {
            return 0;
        }
        let idx = (((samples.len() as f64) - 1.0) * q).round() as usize;
        samples[idx.min(samples.len() - 1)]
    };
    let p = |q: f64| p_of(&latencies, q);
    ttfb_samples.sort_unstable();
    let pt = |q: f64| p_of(&ttfb_samples, q);

    WindowStats {
        count,
        success,
        transient_fail: transient,
        error,
        latency_p50_ms: p(0.50),
        latency_p95_ms: p(0.95),
        latency_p99_ms: p(0.99),
        fallback_used,
        waited_for_cooldown: waited,
        ttfb_p50_ms: pt(0.50),
        ttfb_p95_ms: pt(0.95),
        ttfb_p99_ms: pt(0.99),
        ttfb_samples: ttfb_samples.len() as u64,
        stream_aborts,
        stream_recovers,
        client_visible_errors,
        fallback_proxy_used,
        input_tokens_total,
        output_tokens_total,
        cache_read_tokens_total,
    }
}

/// 时间序列：过去 60 分钟，每分钟 1 个桶
///
/// 用于前端绘制 RPM/TPM 趋势图。每桶聚合该分钟窗口内的请求计数、token 用量、
/// 成功数、p50 延迟、TTFB 分位、fallback proxy 触发数。
///
/// 返回长度恒为 60 的 Vec，索引 0 = 当前分钟（最右侧），索引 59 = 59 分钟前。
/// `ts_offset_secs` 字段标记每桶距 `now` 的秒偏移（负值，桶的 "新近度"）。
pub fn compute_time_series_60m(records: &[RequestRecord], now: Instant) -> Vec<TimeSeriesPoint> {
    const BUCKET_COUNT: usize = 60;
    const BUCKET_SECS: u64 = 60;
    let mut buckets: Vec<BucketAccumulator> = (0..BUCKET_COUNT)
        .map(|_| BucketAccumulator::default())
        .collect();

    for r in records {
        // 桶索引 = 距 now 的秒数 / 60；超过 60 分钟则忽略
        let elapsed = now.saturating_duration_since(r.finished_at).as_secs();
        if elapsed >= BUCKET_COUNT as u64 * BUCKET_SECS {
            continue;
        }
        let bucket_idx = (elapsed / BUCKET_SECS) as usize;
        let bucket = &mut buckets[bucket_idx];
        bucket.count += 1;
        if matches!(r.kind, RequestKind::Success) {
            bucket.success += 1;
        }
        bucket.latency_samples.push(r.latency.as_millis() as u64);
        if let Some(ttfb) = r.ttfb_ms {
            bucket.ttfb_samples.push(ttfb);
        }
        if r.used_fallback_proxy {
            bucket.fallback_proxy_count += 1;
        }
        if let Some(t) = r.input_tokens {
            bucket.input_tokens = bucket.input_tokens.saturating_add(t as u64);
        }
        if let Some(t) = r.output_tokens {
            bucket.output_tokens = bucket.output_tokens.saturating_add(t as u64);
        }
        if let Some(t) = r.cache_read_tokens {
            bucket.cache_read_tokens = bucket.cache_read_tokens.saturating_add(t as u64);
        }
    }

    // 索引 0 = 0 分钟前（当前桶），输出顺序按时间从旧到新（更适合前端绘图）
    buckets
        .into_iter()
        .enumerate()
        .map(|(idx, b)| TimeSeriesPoint {
            ts_offset_secs: -((idx as i64) * BUCKET_SECS as i64),
            request_count: b.count,
            success_count: b.success,
            input_tokens: b.input_tokens,
            output_tokens: b.output_tokens,
            cache_read_tokens: b.cache_read_tokens,
            p50_ms: percentile_u64(&b.latency_samples, 0.50),
            ttfb_p50_ms: percentile_u64(&b.ttfb_samples, 0.50),
            fallback_proxy_count: b.fallback_proxy_count,
        })
        .rev() // 翻转使 0 = 60 分钟前（左侧），59 = 当前（右侧）
        .collect()
}

#[derive(Default)]
struct BucketAccumulator {
    count: u64,
    success: u64,
    fallback_proxy_count: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    latency_samples: Vec<u64>,
    ttfb_samples: Vec<u64>,
}

fn percentile_u64(samples: &[u64], q: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let idx = (((sorted.len() as f64) - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// 时间序列单个数据点（1 分钟桶）
#[derive(Debug, Clone, Default)]
pub struct TimeSeriesPoint {
    /// 桶相对 `now` 的秒偏移（负值，越小越旧）
    pub ts_offset_secs: i64,
    /// 该分钟内请求数（RPM 直接值）
    pub request_count: u64,
    /// 该分钟内成功请求数
    pub success_count: u64,
    /// 输入 token 数
    pub input_tokens: u64,
    /// 输出 token 数
    pub output_tokens: u64,
    /// cache_read token 数
    pub cache_read_tokens: u64,
    /// 该桶内 p50 延迟
    pub p50_ms: u64,
    /// 该桶内 TTFB p50（流式样本）
    pub ttfb_p50_ms: u64,
    /// Tier 化代理 fallback 触发数
    pub fallback_proxy_count: u64,
}

/// 滑窗内的聚合统计
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowStats {
    pub count: u64,
    pub success: u64,
    pub transient_fail: u64,
    pub error: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub fallback_used: u64,
    pub waited_for_cooldown: u64,
    /// 流式首字节延迟分位（毫秒）。样本仅含 `ttfb_ms = Some(_)` 的记录。
    pub ttfb_p50_ms: u64,
    pub ttfb_p95_ms: u64,
    pub ttfb_p99_ms: u64,
    /// TTFB 样本数（仅流式且抓到首字节）
    pub ttfb_samples: u64,
    /// 流式中断次数（mid-stream abort）
    pub stream_aborts: u64,
    /// 流式中断后救活次数
    pub stream_recovers: u64,
    /// 客户端最终看到错误的请求数
    pub client_visible_errors: u64,
    /// Tier 化代理 fallback 触发次数
    pub fallback_proxy_used: u64,
    /// 输入 token 总数（仅含 `input_tokens = Some(_)` 记录）
    pub input_tokens_total: u64,
    /// 输出 token 总数
    pub output_tokens_total: u64,
    /// cache_read token 总数（prompt cache 命中节省）
    pub cache_read_tokens_total: u64,
}

/// 按某个维度（model / credential）切片后的窗口统计
#[derive(Debug, Clone, Default)]
pub struct DimensionStats {
    /// 维度键（e.g. 模型名 / 凭据 id 字符串）
    pub key: String,
    pub count: u64,
    pub success: u64,
    pub transient_fail: u64,
    pub error: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
}

/// 通用维度聚合：按 `key_of` 提取的键分组聚合，已过滤窗口外记录。
///
/// 返回 `Vec<DimensionStats>`，按 count 降序，最多 `top_n` 条；
/// `None` key 的记录被忽略。
///
/// 复杂度 `O(N + K log K)`（N = records.len()，K = 唯一 key 数量）。
pub fn compute_dimension_stats<F>(
    records: &[RequestRecord],
    now: Instant,
    window: Duration,
    key_of: F,
    top_n: usize,
) -> Vec<DimensionStats>
where
    F: Fn(&RequestRecord) -> Option<String>,
{
    use std::collections::HashMap;

    let cutoff = now.checked_sub(window).unwrap_or(now);
    let mut groups: HashMap<String, Vec<&RequestRecord>> = HashMap::new();
    for r in records {
        if r.finished_at < cutoff {
            continue;
        }
        if let Some(k) = key_of(r) {
            groups.entry(k).or_default().push(r);
        }
    }
    let mut out: Vec<DimensionStats> = groups
        .into_iter()
        .map(|(key, rs)| {
            let mut latencies: Vec<u64> = rs.iter().map(|r| r.latency.as_millis() as u64).collect();
            latencies.sort_unstable();
            let p = |q: f64| -> u64 {
                if latencies.is_empty() {
                    return 0;
                }
                let idx = (((latencies.len() as f64) - 1.0) * q).round() as usize;
                latencies[idx.min(latencies.len() - 1)]
            };
            let count = rs.len() as u64;
            let mut success = 0u64;
            let mut transient = 0u64;
            let mut error = 0u64;
            for r in &rs {
                match r.kind {
                    RequestKind::Success => success += 1,
                    RequestKind::TransientFail => transient += 1,
                    RequestKind::Error => error += 1,
                }
            }
            DimensionStats {
                key,
                count,
                success,
                transient_fail: transient,
                error,
                latency_p50_ms: p(0.50),
                latency_p95_ms: p(0.95),
                latency_p99_ms: p(0.99),
            }
        })
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.count));
    out.truncate(top_n);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(
        secs_ago: u64,
        latency_ms: u64,
        kind: RequestKind,
        fb: bool,
        wait: bool,
    ) -> RequestRecord {
        make_with(secs_ago, latency_ms, kind, fb, wait, None, None)
    }

    fn make_with(
        secs_ago: u64,
        latency_ms: u64,
        kind: RequestKind,
        fb: bool,
        wait: bool,
        model: Option<&str>,
        credential_id: Option<u64>,
    ) -> RequestRecord {
        RequestRecord {
            seq: 0,
            finished_at: Instant::now() - Duration::from_secs(secs_ago),
            latency: Duration::from_millis(latency_ms),
            kind,
            used_fallback: fb,
            waited_for_cooldown: wait,
            model: model.map(Arc::from),
            credential_id,
            ttfb_ms: None,
            stream_aborted_midway: false,
            stream_recovered: false,
            client_visible_error: false,
            used_fallback_proxy: false,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
        }
    }

    #[test]
    fn ring_buffer_overwrites_oldest() {
        let mut rb = RingBuffer::new(3);
        rb.push(make(0, 100, RequestKind::Success, false, false));
        rb.push(make(0, 200, RequestKind::Success, false, false));
        rb.push(make(0, 300, RequestKind::Success, false, false));
        rb.push(make(0, 400, RequestKind::Success, false, false));
        let snap = rb.snapshot();
        assert_eq!(snap.len(), 3);
        let lats: Vec<_> = snap.iter().map(|r| r.latency.as_millis()).collect();
        // 容量 3，第 4 条覆盖第 1 条。snapshot 顺序按物理索引。
        // 写入序列 head: 0,1,2,0 → 第 4 次写到 idx=0
        // 所以 data[0] = 400ms, data[1] = 200ms, data[2] = 300ms
        assert!(lats.contains(&200));
        assert!(lats.contains(&300));
        assert!(lats.contains(&400));
        assert!(!lats.contains(&100), "最老的 100ms 应被覆盖");
    }

    #[test]
    fn record_assigns_monotonic_seq() {
        let rec = MetricsRecorder::new();
        let s0 = rec.record(make(0, 100, RequestKind::Success, false, false));
        let s1 = rec.record(make(0, 100, RequestKind::Success, false, false));
        let s2 = rec.record(make(0, 100, RequestKind::Success, false, false));
        assert_eq!((s0, s1, s2), (0, 1, 2));
    }

    #[test]
    fn update_tokens_patches_matching_record() {
        let rec = MetricsRecorder::new();
        let seq = rec.record(make(0, 100, RequestKind::Success, false, false));
        // 记录时 token 为 None（provider 层占位）
        assert!(rec.snapshot()[0].input_tokens.is_none());

        rec.update_tokens(seq, Some(120), Some(45), Some(8));
        let snap = rec.snapshot();
        assert_eq!(snap[0].input_tokens, Some(120));
        assert_eq!(snap[0].output_tokens, Some(45));
        assert_eq!(snap[0].cache_read_tokens, Some(8));
    }

    #[test]
    fn update_tokens_skips_overwritten_slot() {
        // 容量 2：seq 0/1 占满后，seq 2 覆盖 seq 0 的物理 slot。
        // 用迟到的 seq 0 回填不应污染现在住在该 slot 的 seq 2。
        let rb_recorder = MetricsRecorder {
            buf: Mutex::new(RingBuffer::new(2)),
            started_at: Instant::now(),
        };
        let s0 = rb_recorder.record(make(0, 100, RequestKind::Success, false, false));
        let _s1 = rb_recorder.record(make(0, 100, RequestKind::Success, false, false));
        let s2 = rb_recorder.record(make(0, 100, RequestKind::Success, false, false));
        assert_eq!((s0, s2), (0, 2)); // s2 与 s0 落在同一 slot（2 % 2 == 0）

        // 迟到的 s0 回填：slot 0 现在是 s2，seq 不匹配 → 跳过
        rb_recorder.update_tokens(s0, Some(999), Some(999), Some(999));
        let slot0 = &rb_recorder.snapshot()[0];
        assert_eq!(slot0.seq, 2);
        assert!(slot0.input_tokens.is_none(), "迟到 seq 不应污染已覆盖的 slot");

        // 当前 s2 的回填正常生效
        rb_recorder.update_tokens(s2, Some(50), Some(20), Some(0));
        assert_eq!(rb_recorder.snapshot()[0].input_tokens, Some(50));
    }

    #[test]
    fn time_series_reflects_patched_tokens() {
        let rec = MetricsRecorder::new();
        let seq = rec.record(make(5, 100, RequestKind::Success, false, false));
        rec.update_tokens(seq, Some(200), Some(60), Some(10));

        let series = compute_time_series_60m(&rec.snapshot(), Instant::now());
        let totals: (u64, u64, u64) = series.iter().fold((0, 0, 0), |acc, p| {
            (
                acc.0 + p.input_tokens,
                acc.1 + p.output_tokens,
                acc.2 + p.cache_read_tokens,
            )
        });
        assert_eq!(totals, (200, 60, 10), "回填的 token 应进入 60min 时间序列");
    }

    #[test]
    fn record_handle_patches_via_seq() {
        let rec = Arc::new(MetricsRecorder::new());
        let seq = rec.record(make(0, 100, RequestKind::Success, false, false));
        let handle = RecordHandle::new(rec.clone(), seq);
        handle.update_tokens(Some(11), Some(22), Some(3));
        let snap = rec.snapshot();
        assert_eq!(snap[0].input_tokens, Some(11));
        assert_eq!(snap[0].output_tokens, Some(22));
        assert_eq!(snap[0].cache_read_tokens, Some(3));
    }

    #[test]
    fn window_filters_old_records() {
        let recs = vec![
            make(10, 100, RequestKind::Success, false, false),
            make(40, 200, RequestKind::Success, false, false),
            make(120, 300, RequestKind::Success, false, false),
        ];        // 过去 60s 窗口：只 10s 和 40s 这两条进
        let stats = compute_window_stats(&recs, Instant::now(), Duration::from_secs(60));
        assert_eq!(stats.count, 2);
        assert_eq!(stats.success, 2);
        // 中位数（p50）取排序后 idx=round(1*0.5)=1，即 200ms
        assert_eq!(stats.latency_p50_ms, 200);
    }

    #[test]
    fn percentile_calculation_handles_diverse_latencies() {
        let mut recs = Vec::new();
        for i in 1..=100 {
            recs.push(make(0, i * 10, RequestKind::Success, false, false));
        }
        let stats = compute_window_stats(&recs, Instant::now(), Duration::from_secs(3600));
        assert_eq!(stats.count, 100);
        // p50: idx = 99 * 0.5 = 49.5 → round 50 → 第 51 大 = 510ms
        assert_eq!(stats.latency_p50_ms, 510);
        // p95: idx = 99 * 0.95 = 94.05 → round 94 → 第 95 大 = 950ms
        assert_eq!(stats.latency_p95_ms, 950);
        // p99: idx = 99 * 0.99 = 98.01 → round 98 → 第 99 大 = 990ms
        assert_eq!(stats.latency_p99_ms, 990);
    }

    #[test]
    fn fallback_and_waited_counted() {
        let recs = vec![
            make(1, 1000, RequestKind::Success, true, false),
            make(2, 1500, RequestKind::Success, false, true),
            make(3, 2000, RequestKind::TransientFail, true, true),
        ];
        let stats = compute_window_stats(&recs, Instant::now(), Duration::from_secs(60));
        assert_eq!(stats.fallback_used, 2);
        assert_eq!(stats.waited_for_cooldown, 2);
        assert_eq!(stats.transient_fail, 1);
        assert_eq!(stats.success, 2);
    }

    #[test]
    fn empty_window_returns_zeros() {
        let stats = compute_window_stats(&[], Instant::now(), Duration::from_secs(60));
        assert_eq!(stats.count, 0);
        assert_eq!(stats.latency_p50_ms, 0);
    }

    #[test]
    fn recorder_basic_record_and_snapshot() {
        let rec = MetricsRecorder::new();
        rec.record(make(0, 100, RequestKind::Success, false, false));
        rec.record(make(0, 200, RequestKind::Error, false, false));
        let snap = rec.snapshot();
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn dimension_stats_groups_by_model_and_picks_top_n() {
        let now = Instant::now();
        let recs = vec![
            make_with(
                0,
                1000,
                RequestKind::Success,
                false,
                false,
                Some("opus"),
                None,
            ),
            make_with(
                1,
                2000,
                RequestKind::Success,
                false,
                false,
                Some("opus"),
                None,
            ),
            make_with(
                2,
                1500,
                RequestKind::Success,
                false,
                false,
                Some("sonnet"),
                None,
            ),
            make_with(3, 100, RequestKind::Success, false, false, None, None), // 无 model: 忽略
        ];
        let by_model = compute_dimension_stats(
            &recs,
            now,
            Duration::from_secs(60),
            |r| r.model.as_deref().map(String::from),
            10,
        );
        assert_eq!(by_model.len(), 2);
        // Top by count: opus(2) > sonnet(1)
        assert_eq!(by_model[0].key, "opus");
        assert_eq!(by_model[0].count, 2);
        assert_eq!(by_model[1].key, "sonnet");
        assert_eq!(by_model[1].count, 1);
        // 无 model 的那条不计入
        let total: u64 = by_model.iter().map(|d| d.count).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn dimension_stats_top_n_truncates() {
        let now = Instant::now();
        let mut recs = Vec::new();
        for i in 0..5 {
            for _ in 0..(5 - i) {
                let model = format!("m{}", i);
                recs.push(make_with(
                    0,
                    100,
                    RequestKind::Success,
                    false,
                    false,
                    Some(&model),
                    None,
                ));
            }
        }
        // 5 个模型 m0..m4，count 各 5,4,3,2,1。top_n=3 → 取前 3。
        let stats = compute_dimension_stats(
            &recs,
            now,
            Duration::from_secs(60),
            |r| r.model.as_deref().map(String::from),
            3,
        );
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].key, "m0");
        assert_eq!(stats[0].count, 5);
        assert_eq!(stats[2].count, 3);
    }

    #[test]
    fn dimension_stats_credential_id_grouping() {
        let now = Instant::now();
        let recs = vec![
            make_with(0, 100, RequestKind::Success, false, false, None, Some(1)),
            make_with(
                1,
                200,
                RequestKind::TransientFail,
                false,
                false,
                None,
                Some(1),
            ),
            make_with(2, 50, RequestKind::Success, false, false, None, Some(2)),
        ];
        let by_cred = compute_dimension_stats(
            &recs,
            now,
            Duration::from_secs(60),
            |r| r.credential_id.map(|id| id.to_string()),
            10,
        );
        let cred1 = by_cred.iter().find(|d| d.key == "1").unwrap();
        assert_eq!(cred1.count, 2);
        assert_eq!(cred1.success, 1);
        assert_eq!(cred1.transient_fail, 1);
        let cred2 = by_cred.iter().find(|d| d.key == "2").unwrap();
        assert_eq!(cred2.count, 1);
        assert_eq!(cred2.success, 1);
    }
}
