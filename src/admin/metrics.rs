//! Admin API 聚合指标视图
//!
//! 把 [`crate::kiro::metrics::MetricsRecorder`] 的原始 ring buffer 聚合成
//! Admin UI / Prometheus exporter 易于消费的结构化数据。计算在请求时执行
//! （`O(buffer_len) ≈ O(2048)`），无需后台任务。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use std::fmt::Write;

use crate::kiro::metrics::{
    DimensionStats, MetricsRecorder, TimeSeriesPoint, compute_dimension_stats,
    compute_time_series_60m, compute_window_stats,
};
use crate::kiro::token_manager::MultiTokenManager;

/// 顶层 metrics 响应
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminMetricsResponse {
    /// 进程已运行秒数
    pub uptime_seconds: u64,
    /// 凭据池总览
    pub credentials: CredentialAggregates,
    /// 请求统计（按多窗口）
    pub requests: RequestStatsByWindow,
    /// 延迟统计（按多窗口）
    pub latency: LatencyStatsByWindow,
    /// cooldown 与 fallback 行为
    pub cooldown: CooldownStats,
    /// 当前 ring buffer 已记录的请求总数（受容量上限）
    pub buffer_size: u64,
    /// 中转层 prompt prefix cache 行为统计（None 表示 admin 未注入 cache 实例）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<PromptCacheStats>,
    /// 最近 1 小时内按 model 切片的请求统计（top 10）
    pub by_model_1h: Vec<DimensionBreakdown>,
    /// 最近 1 小时内按 credential_id 切片的请求统计（top 20）
    pub by_credential_1h: Vec<DimensionBreakdown>,
    /// 过去 60 分钟时间序列（每分钟 1 桶，固定 60 点，索引 0 = 最旧）
    pub time_series_60m: Vec<TimeSeriesPointOut>,
}

/// Prompt prefix cache 统计快照
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PromptCacheStats {
    pub enabled: bool,
    pub entries: u64,
    pub capacity: u64,
    pub ttl_secs: u64,
    pub hit_total: u64,
    pub miss_total: u64,
    pub eviction_total: u64,
    pub hit_rate_1m: f64,
    pub hit_rate_5m: f64,
    pub saved_input_tokens_5m: i64,
    /// 上报口径命中率（应用 perceived 系数后对客户端可见，1min 窗口）
    pub reported_hit_rate_1m: f64,
    /// 上报口径 5min 节省 input tokens（对客户端/下游计费可见）
    pub reported_saved_input_tokens_5m: i64,
    /// 当前生效的 perceived 系数（null=未启用，按真实口径上报）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perceived_cache_hit_ratio: Option<f64>,
}

/// 凭据池聚合
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CredentialAggregates {
    pub total: u64,
    /// 既未禁用也不在 cooldown 的可用凭据数
    pub active: u64,
    /// 当前在 cooldown 中的凭据数
    pub cooling: u64,
    /// 已禁用（手动或自动）的凭据数
    pub disabled: u64,
    /// 累计 success_count（跨进程持久化）
    pub success_count_total: u64,
    /// 累计 transient_failure_count
    pub transient_failure_count_total: u64,
    /// 累计 failure_count（永久失败）
    pub failure_count_total: u64,
}

/// 请求计数（按 1min/5min/1h/buffer-total 四个窗口）
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RequestStatsByWindow {
    pub last_1m: WindowRequestCounts,
    pub last_5m: WindowRequestCounts,
    pub last_1h: WindowRequestCounts,
    /// 整个 ring buffer 范围（最多 RING_CAPACITY 条请求）
    pub all_buffer: WindowRequestCounts,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WindowRequestCounts {
    pub total: u64,
    pub success: u64,
    pub transient_fail: u64,
    pub error: u64,
    /// 成功率百分比（0~100），分母为 total；total=0 时为 None
    pub success_rate: Option<f64>,
    /// 客户端最终看到错误的请求数（区别于内部 transient，已重试仍失败）
    pub client_visible_errors: u64,
    /// 流式中断次数（mid-stream abort）
    pub stream_aborts: u64,
    /// 流式中断后救活次数
    pub stream_recovers: u64,
    /// 输入 token 总数
    pub input_tokens_total: u64,
    /// 输出 token 总数
    pub output_tokens_total: u64,
    /// cache_read token 总数（prompt cache 命中节省）
    pub cache_read_tokens_total: u64,
}

/// 延迟分位（毫秒）
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LatencyStatsByWindow {
    pub last_1m: WindowLatency,
    pub last_5m: WindowLatency,
    pub last_1h: WindowLatency,
    pub all_buffer: WindowLatency,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WindowLatency {
    /// 样本数（同 RequestStats.total）
    pub samples: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    /// 流式首字节延迟分位（毫秒），样本仅含抓到首字节的流式请求
    pub ttfb_p50_ms: u64,
    pub ttfb_p95_ms: u64,
    pub ttfb_p99_ms: u64,
    /// TTFB 样本数
    pub ttfb_samples: u64,
}

/// cooldown 行为统计
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CooldownStats {
    /// 当前在 cooldown 中的凭据数（同 credentials.cooling）
    pub currently_cooling: u64,
    /// 最近 1 分钟内 fallback 路径触发次数（端到端请求计数）
    pub fallback_used_1m: u64,
    /// 最近 1 分钟内"全员 cooldown 智能等待"触发次数
    pub waited_for_cooldown_1m: u64,
    /// 最近 5 分钟内 fallback 触发次数
    pub fallback_used_5m: u64,
    /// 最近 5 分钟内 wait 触发次数
    pub waited_for_cooldown_5m: u64,
    /// 最近 1 小时内 fallback 触发次数
    pub fallback_used_1h: u64,
    /// 最近 1 小时内 wait 触发次数
    pub waited_for_cooldown_1h: u64,
    /// 最近 1 分钟内 tier 化代理 fallback 触发次数
    pub fallback_proxy_used_1m: u64,
    /// 最近 5 分钟内 tier 化代理 fallback 触发次数
    pub fallback_proxy_used_5m: u64,
    /// 最近 1 小时内 tier 化代理 fallback 触发次数
    pub fallback_proxy_used_1h: u64,
}

/// 单条按维度切片的窗口统计（admin 输出）
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DimensionBreakdown {
    /// 维度键（model 名 / 凭据 id 字符串）
    pub key: String,
    pub count: u64,
    pub success: u64,
    pub transient_fail: u64,
    pub error: u64,
    pub success_rate: Option<f64>,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
}

impl From<DimensionStats> for DimensionBreakdown {
    fn from(d: DimensionStats) -> Self {
        let rate = if d.count > 0 {
            Some(d.success as f64 / d.count as f64 * 100.0)
        } else {
            None
        };
        Self {
            key: d.key,
            count: d.count,
            success: d.success,
            transient_fail: d.transient_fail,
            error: d.error,
            success_rate: rate,
            p50_ms: d.latency_p50_ms,
            p95_ms: d.latency_p95_ms,
            p99_ms: d.latency_p99_ms,
        }
    }
}

/// 时间序列单点（admin 输出，对应前端趋势图的一个 1 分钟桶）
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TimeSeriesPointOut {
    /// 桶相对 now 的秒偏移（负值，越小越旧）
    pub ts_offset_secs: i64,
    pub request_count: u64,
    pub success_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub p50_ms: u64,
    pub ttfb_p50_ms: u64,
    pub fallback_proxy_count: u64,
}

impl From<TimeSeriesPoint> for TimeSeriesPointOut {
    fn from(p: TimeSeriesPoint) -> Self {
        Self {
            ts_offset_secs: p.ts_offset_secs,
            request_count: p.request_count,
            success_count: p.success_count,
            input_tokens: p.input_tokens,
            output_tokens: p.output_tokens,
            cache_read_tokens: p.cache_read_tokens,
            p50_ms: p.p50_ms,
            ttfb_p50_ms: p.ttfb_p50_ms,
            fallback_proxy_count: p.fallback_proxy_count,
        }
    }
}

/// 计算并组装 metrics 响应
pub fn compute_admin_metrics(
    metrics: &Arc<MetricsRecorder>,
    token_manager: &Arc<MultiTokenManager>,
) -> AdminMetricsResponse {
    let now = Instant::now();
    let uptime = now
        .saturating_duration_since(metrics.started_at())
        .as_secs();

    // 凭据池聚合（直接读 token_manager 快照）
    let snap = token_manager.snapshot();
    let mut active = 0u64;
    let mut cooling = 0u64;
    let mut disabled = 0u64;
    let mut success_total = 0u64;
    let mut transient_total = 0u64;
    let mut failure_total = 0u64;
    for e in &snap.entries {
        success_total += e.success_count;
        transient_total += e.transient_failure_count;
        failure_total += e.failure_count as u64;
        if e.disabled {
            disabled += 1;
        } else if e.cooldown_remaining_seconds > 0 {
            cooling += 1;
        } else {
            active += 1;
        }
    }
    let credentials = CredentialAggregates {
        total: snap.entries.len() as u64,
        active,
        cooling,
        disabled,
        success_count_total: success_total,
        transient_failure_count_total: transient_total,
        failure_count_total: failure_total,
    };

    // 请求/延迟窗口聚合：复制一次 buffer 后多窗口扫描
    let records = metrics.snapshot();
    let s1m = compute_window_stats(&records, now, Duration::from_secs(60));
    let s5m = compute_window_stats(&records, now, Duration::from_secs(300));
    let s1h = compute_window_stats(&records, now, Duration::from_secs(3600));
    let s_all = compute_window_stats(&records, now, Duration::from_secs(u64::MAX / 2));

    let to_counts = |w: &crate::kiro::metrics::WindowStats| WindowRequestCounts {
        total: w.count,
        success: w.success,
        transient_fail: w.transient_fail,
        error: w.error,
        success_rate: if w.count > 0 {
            Some(w.success as f64 / w.count as f64 * 100.0)
        } else {
            None
        },
        client_visible_errors: w.client_visible_errors,
        stream_aborts: w.stream_aborts,
        stream_recovers: w.stream_recovers,
        input_tokens_total: w.input_tokens_total,
        output_tokens_total: w.output_tokens_total,
        cache_read_tokens_total: w.cache_read_tokens_total,
    };
    let to_latency = |w: &crate::kiro::metrics::WindowStats| WindowLatency {
        samples: w.count,
        p50_ms: w.latency_p50_ms,
        p95_ms: w.latency_p95_ms,
        p99_ms: w.latency_p99_ms,
        ttfb_p50_ms: w.ttfb_p50_ms,
        ttfb_p95_ms: w.ttfb_p95_ms,
        ttfb_p99_ms: w.ttfb_p99_ms,
        ttfb_samples: w.ttfb_samples,
    };

    let requests = RequestStatsByWindow {
        last_1m: to_counts(&s1m),
        last_5m: to_counts(&s5m),
        last_1h: to_counts(&s1h),
        all_buffer: to_counts(&s_all),
    };
    let latency = LatencyStatsByWindow {
        last_1m: to_latency(&s1m),
        last_5m: to_latency(&s5m),
        last_1h: to_latency(&s1h),
        all_buffer: to_latency(&s_all),
    };
    let cooldown = CooldownStats {
        currently_cooling: cooling,
        fallback_used_1m: s1m.fallback_used,
        waited_for_cooldown_1m: s1m.waited_for_cooldown,
        fallback_used_5m: s5m.fallback_used,
        waited_for_cooldown_5m: s5m.waited_for_cooldown,
        fallback_used_1h: s1h.fallback_used,
        waited_for_cooldown_1h: s1h.waited_for_cooldown,
        fallback_proxy_used_1m: s1m.fallback_proxy_used,
        fallback_proxy_used_5m: s5m.fallback_proxy_used,
        fallback_proxy_used_1h: s1h.fallback_proxy_used,
    };

    // 维度切片：1h 窗口内按 model / credential_id 聚合
    let by_model_1h: Vec<DimensionBreakdown> = compute_dimension_stats(
        &records,
        now,
        Duration::from_secs(3600),
        |r| r.model.as_deref().map(String::from),
        10,
    )
    .into_iter()
    .map(DimensionBreakdown::from)
    .collect();
    let by_credential_1h: Vec<DimensionBreakdown> = compute_dimension_stats(
        &records,
        now,
        Duration::from_secs(3600),
        |r| r.credential_id.map(|id| id.to_string()),
        20,
    )
    .into_iter()
    .map(DimensionBreakdown::from)
    .collect();

    // 过去 60 分钟时间序列（前端趋势图数据源）
    let time_series_60m: Vec<TimeSeriesPointOut> = compute_time_series_60m(&records, now)
        .into_iter()
        .map(TimeSeriesPointOut::from)
        .collect();

    AdminMetricsResponse {
        uptime_seconds: uptime,
        credentials,
        requests,
        latency,
        cooldown,
        buffer_size: metrics.buffer_len() as u64,
        prompt_cache: None,
        by_model_1h,
        by_credential_1h,
        time_series_60m,
    }
}

/// 把 [`AdminMetricsResponse`] 渲染为 Prometheus / OpenMetrics 文本
///
/// 不引入额外依赖（`prometheus` crate ≈ 200 KB），手写约 80 行渲染逻辑。
/// 输出每个 metric family 都带 `# HELP` 与 `# TYPE`，符合
/// `text/plain; version=0.0.4` 规范。
pub fn render_prometheus(resp: &AdminMetricsResponse) -> String {
    let mut s = String::with_capacity(2048);

    // ---- uptime ----
    let _ = writeln!(s, "# HELP kiro_uptime_seconds Process uptime");
    let _ = writeln!(s, "# TYPE kiro_uptime_seconds counter");
    let _ = writeln!(s, "kiro_uptime_seconds {}", resp.uptime_seconds);

    // ---- credentials gauge ----
    let _ = writeln!(
        s,
        "# HELP kiro_credentials_total Total configured credentials"
    );
    let _ = writeln!(s, "# TYPE kiro_credentials_total gauge");
    let _ = writeln!(s, "kiro_credentials_total {}", resp.credentials.total);
    let _ = writeln!(s, "# HELP kiro_credentials_state Credentials in each state");
    let _ = writeln!(s, "# TYPE kiro_credentials_state gauge");
    let _ = writeln!(
        s,
        "kiro_credentials_state{{state=\"active\"}} {}",
        resp.credentials.active
    );
    let _ = writeln!(
        s,
        "kiro_credentials_state{{state=\"cooling\"}} {}",
        resp.credentials.cooling
    );
    let _ = writeln!(
        s,
        "kiro_credentials_state{{state=\"disabled\"}} {}",
        resp.credentials.disabled
    );

    // ---- credential lifetime counters ----
    let _ = writeln!(
        s,
        "# HELP kiro_credentials_outcome_total Cumulative request outcomes across all credentials"
    );
    let _ = writeln!(s, "# TYPE kiro_credentials_outcome_total counter");
    let _ = writeln!(
        s,
        "kiro_credentials_outcome_total{{outcome=\"success\"}} {}",
        resp.credentials.success_count_total
    );
    let _ = writeln!(
        s,
        "kiro_credentials_outcome_total{{outcome=\"transient\"}} {}",
        resp.credentials.transient_failure_count_total
    );
    let _ = writeln!(
        s,
        "kiro_credentials_outcome_total{{outcome=\"failure\"}} {}",
        resp.credentials.failure_count_total
    );

    // ---- per-window request counts + latency ----
    let _ = writeln!(s, "# HELP kiro_requests Window request counts by outcome");
    let _ = writeln!(s, "# TYPE kiro_requests gauge");
    let _ = writeln!(
        s,
        "# HELP kiro_latency_milliseconds Window latency quantiles (ms)"
    );
    let _ = writeln!(s, "# TYPE kiro_latency_milliseconds gauge");
    let _ = writeln!(
        s,
        "# HELP kiro_cooldown_events Window cooldown/fallback event counts"
    );
    let _ = writeln!(s, "# TYPE kiro_cooldown_events gauge");
    for (label, counts, lat, fb, wait) in [
        (
            "1m",
            &resp.requests.last_1m,
            &resp.latency.last_1m,
            resp.cooldown.fallback_used_1m,
            resp.cooldown.waited_for_cooldown_1m,
        ),
        (
            "5m",
            &resp.requests.last_5m,
            &resp.latency.last_5m,
            resp.cooldown.fallback_used_5m,
            resp.cooldown.waited_for_cooldown_5m,
        ),
        (
            "1h",
            &resp.requests.last_1h,
            &resp.latency.last_1h,
            resp.cooldown.fallback_used_1h,
            resp.cooldown.waited_for_cooldown_1h,
        ),
    ] {
        let _ = writeln!(
            s,
            "kiro_requests{{window=\"{label}\",outcome=\"success\"}} {}",
            counts.success
        );
        let _ = writeln!(
            s,
            "kiro_requests{{window=\"{label}\",outcome=\"transient\"}} {}",
            counts.transient_fail
        );
        let _ = writeln!(
            s,
            "kiro_requests{{window=\"{label}\",outcome=\"error\"}} {}",
            counts.error
        );
        let _ = writeln!(
            s,
            "kiro_latency_milliseconds{{window=\"{label}\",quantile=\"0.5\"}} {}",
            lat.p50_ms
        );
        let _ = writeln!(
            s,
            "kiro_latency_milliseconds{{window=\"{label}\",quantile=\"0.95\"}} {}",
            lat.p95_ms
        );
        let _ = writeln!(
            s,
            "kiro_latency_milliseconds{{window=\"{label}\",quantile=\"0.99\"}} {}",
            lat.p99_ms
        );
        let _ = writeln!(
            s,
            "kiro_cooldown_events{{window=\"{label}\",event=\"fallback_used\"}} {fb}"
        );
        let _ = writeln!(
            s,
            "kiro_cooldown_events{{window=\"{label}\",event=\"waited\"}} {wait}"
        );
    }

    // ---- prompt cache ----
    if let Some(pc) = &resp.prompt_cache {
        let _ = writeln!(
            s,
            "# HELP kiro_prompt_cache_state Prompt cache enable + occupancy"
        );
        let _ = writeln!(s, "# TYPE kiro_prompt_cache_state gauge");
        let _ = writeln!(
            s,
            "kiro_prompt_cache_state{{field=\"enabled\"}} {}",
            if pc.enabled { 1 } else { 0 }
        );
        let _ = writeln!(
            s,
            "kiro_prompt_cache_state{{field=\"entries\"}} {}",
            pc.entries
        );
        let _ = writeln!(
            s,
            "kiro_prompt_cache_state{{field=\"capacity\"}} {}",
            pc.capacity
        );
        let _ = writeln!(s, "# HELP kiro_prompt_cache_total Cumulative cache events");
        let _ = writeln!(s, "# TYPE kiro_prompt_cache_total counter");
        let _ = writeln!(
            s,
            "kiro_prompt_cache_total{{event=\"hit\"}} {}",
            pc.hit_total
        );
        let _ = writeln!(
            s,
            "kiro_prompt_cache_total{{event=\"miss\"}} {}",
            pc.miss_total
        );
        let _ = writeln!(
            s,
            "kiro_prompt_cache_total{{event=\"eviction\"}} {}",
            pc.eviction_total
        );
        let _ = writeln!(
            s,
            "# HELP kiro_prompt_cache_hit_rate_percent Prompt cache hit rate by accounting view"
        );
        let _ = writeln!(s, "# TYPE kiro_prompt_cache_hit_rate_percent gauge");
        let _ = writeln!(
            s,
            "kiro_prompt_cache_hit_rate_percent{{window=\"1m\",view=\"real\"}} {}",
            pc.hit_rate_1m
        );
        let _ = writeln!(
            s,
            "kiro_prompt_cache_hit_rate_percent{{window=\"5m\",view=\"real\"}} {}",
            pc.hit_rate_5m
        );
        let _ = writeln!(
            s,
            "kiro_prompt_cache_hit_rate_percent{{window=\"1m\",view=\"reported\"}} {}",
            pc.reported_hit_rate_1m
        );
        let _ = writeln!(
            s,
            "# HELP kiro_prompt_cache_saved_input_tokens Prompt cache saved input tokens by accounting view"
        );
        let _ = writeln!(s, "# TYPE kiro_prompt_cache_saved_input_tokens gauge");
        let _ = writeln!(
            s,
            "kiro_prompt_cache_saved_input_tokens{{window=\"5m\",view=\"real\"}} {}",
            pc.saved_input_tokens_5m
        );
        let _ = writeln!(
            s,
            "kiro_prompt_cache_saved_input_tokens{{window=\"5m\",view=\"reported\"}} {}",
            pc.reported_saved_input_tokens_5m
        );
        let _ = writeln!(
            s,
            "# HELP kiro_prompt_cache_perceived_ratio Configured reported cache hit ratio, 0 when disabled"
        );
        let _ = writeln!(s, "# TYPE kiro_prompt_cache_perceived_ratio gauge");
        let _ = writeln!(
            s,
            "kiro_prompt_cache_perceived_ratio {}",
            pc.perceived_cache_hit_ratio.unwrap_or(0.0)
        );
    }

    // ---- by-model and by-credential 1h breakdown ----
    let _ = writeln!(
        s,
        "# HELP kiro_requests_by_model_total 1h request counts by model"
    );
    let _ = writeln!(s, "# TYPE kiro_requests_by_model_total gauge");
    for d in &resp.by_model_1h {
        let _ = writeln!(
            s,
            "kiro_requests_by_model_total{{model=\"{}\"}} {}",
            escape_label(&d.key),
            d.count
        );
    }
    let _ = writeln!(
        s,
        "# HELP kiro_requests_by_credential_total 1h request counts by credential id"
    );
    let _ = writeln!(s, "# TYPE kiro_requests_by_credential_total gauge");
    for d in &resp.by_credential_1h {
        let _ = writeln!(
            s,
            "kiro_requests_by_credential_total{{credential_id=\"{}\"}} {}",
            escape_label(&d.key),
            d.count
        );
    }
    let _ = writeln!(
        s,
        "# HELP kiro_buffer_size Records currently in the metrics ring buffer"
    );
    let _ = writeln!(s, "# TYPE kiro_buffer_size gauge");
    let _ = writeln!(s, "kiro_buffer_size {}", resp.buffer_size);

    s
}

/// 转义 Prometheus label value 中的特殊字符 `\` `"` `\n`
fn escape_label(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 4);
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::metrics::{RequestKind, RequestRecord};
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    #[test]
    fn empty_metrics_returns_zeros() {
        let metrics = Arc::new(MetricsRecorder::new());
        let cred = KiroCredentials::default();
        let tm = Arc::new(
            MultiTokenManager::new(Config::default(), vec![cred], None, None, false).unwrap(),
        );
        let resp = compute_admin_metrics(&metrics, &tm);
        assert_eq!(resp.requests.last_1m.total, 0);
        assert_eq!(resp.requests.last_1m.success_rate, None);
        assert_eq!(resp.credentials.total, 1);
        assert_eq!(resp.credentials.active, 1);
    }

    #[test]
    fn metrics_aggregates_recent_requests() {
        let metrics = Arc::new(MetricsRecorder::new());
        let now = Instant::now();
        for i in 0..10 {
            // 一半 opus，一半 sonnet：测 by_model_1h 切片
            let model: Arc<str> = if i % 2 == 0 {
                Arc::from("opus")
            } else {
                Arc::from("sonnet")
            };
            metrics.record(RequestRecord {
                seq: 0,
                finished_at: now,
                latency: Duration::from_millis(100 + i * 50),
                kind: if i < 8 {
                    RequestKind::Success
                } else {
                    RequestKind::TransientFail
                },
                used_fallback: i == 9,
                waited_for_cooldown: i == 8,
                model: Some(model),
                credential_id: Some((i % 3) + 1),
                ttfb_ms: None,
                stream_aborted_midway: false,
                stream_recovered: false,
                client_visible_error: false,
                used_fallback_proxy: false,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
            });
        }
        let cred = KiroCredentials::default();
        let tm = Arc::new(
            MultiTokenManager::new(Config::default(), vec![cred], None, None, false).unwrap(),
        );
        let resp = compute_admin_metrics(&metrics, &tm);
        assert_eq!(resp.requests.last_1m.total, 10);
        assert_eq!(resp.requests.last_1m.success, 8);
        assert_eq!(resp.requests.last_1m.transient_fail, 2);
        let rate = resp.requests.last_1m.success_rate.unwrap();
        assert!((rate - 80.0).abs() < 1e-6);
        assert_eq!(resp.cooldown.fallback_used_1m, 1);
        assert_eq!(resp.cooldown.waited_for_cooldown_1m, 1);
        // last_1h 应包含同样的 10 条
        assert_eq!(resp.requests.last_1h.total, 10);
        // by_model_1h: opus 5 条 + sonnet 5 条
        assert_eq!(resp.by_model_1h.len(), 2);
        let m_total: u64 = resp.by_model_1h.iter().map(|d| d.count).sum();
        assert_eq!(m_total, 10);
        // by_credential_1h: 3 个 ID（1/2/3）
        assert_eq!(resp.by_credential_1h.len(), 3);
        let c_total: u64 = resp.by_credential_1h.iter().map(|d| d.count).sum();
        assert_eq!(c_total, 10);
    }

    #[test]
    fn prometheus_exporter_emits_required_metrics() {
        let metrics = Arc::new(MetricsRecorder::new());
        let now = Instant::now();
        metrics.record(RequestRecord {
            seq: 0,
            finished_at: now,
            latency: Duration::from_millis(123),
            kind: RequestKind::Success,
            used_fallback: false,
            waited_for_cooldown: false,
            model: Some(Arc::from("opus")),
            credential_id: Some(7),
            ttfb_ms: None,
            stream_aborted_midway: false,
            stream_recovered: false,
            client_visible_error: false,
            used_fallback_proxy: false,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
        });
        let cred = KiroCredentials::default();
        let tm = Arc::new(
            MultiTokenManager::new(Config::default(), vec![cred], None, None, false).unwrap(),
        );
        let resp = compute_admin_metrics(&metrics, &tm);
        let text = render_prometheus(&resp);
        // 关键 family 都出现
        assert!(text.contains("# TYPE kiro_uptime_seconds counter"));
        assert!(text.contains("kiro_credentials_total 1"));
        assert!(text.contains("kiro_credentials_state{state=\"active\"} 1"));
        assert!(text.contains("kiro_requests{window=\"1m\",outcome=\"success\"} 1"));
        assert!(text.contains("kiro_latency_milliseconds{window=\"1m\",quantile=\"0.5\"} 123"));
        assert!(text.contains("kiro_requests_by_model_total{model=\"opus\"} 1"));
        assert!(text.contains("kiro_requests_by_credential_total{credential_id=\"7\"} 1"));
        // 无 prompt cache 时不输出该 family
        assert!(!text.contains("kiro_prompt_cache_total"));
    }

    #[test]
    fn time_series_60m_has_fixed_60_points_and_aggregates() {
        let metrics = Arc::new(MetricsRecorder::new());
        let now = Instant::now();
        // 当前分钟桶记 3 条成功
        for _ in 0..3 {
            metrics.record(RequestRecord {
                seq: 0,
                finished_at: now,
                latency: Duration::from_millis(100),
                kind: RequestKind::Success,
                used_fallback: false,
                waited_for_cooldown: false,
                model: Some(Arc::from("opus")),
                credential_id: Some(1),
                ttfb_ms: Some(50),
                stream_aborted_midway: false,
                stream_recovered: false,
                client_visible_error: false,
                used_fallback_proxy: false,
                input_tokens: Some(10),
                output_tokens: Some(20),
                cache_read_tokens: Some(5),
            });
        }
        let cred = KiroCredentials::default();
        let tm = Arc::new(
            MultiTokenManager::new(Config::default(), vec![cred], None, None, false).unwrap(),
        );
        let resp = compute_admin_metrics(&metrics, &tm);
        // 固定 60 个点
        assert_eq!(resp.time_series_60m.len(), 60);
        // 最新桶（索引 59 = 当前分钟）应聚合到 3 条请求
        let latest = resp.time_series_60m.last().unwrap();
        assert_eq!(latest.request_count, 3);
        assert_eq!(latest.success_count, 3);
        assert_eq!(latest.input_tokens, 30);
        assert_eq!(latest.output_tokens, 60);
        assert_eq!(latest.cache_read_tokens, 15);
        // TTFB / token 窗口字段也应暴露
        assert_eq!(resp.latency.last_1m.ttfb_samples, 3);
        assert_eq!(resp.requests.last_1m.input_tokens_total, 30);
    }

    #[test]
    fn prometheus_label_value_escaping() {
        // 内联测试 escape_label 行为
        assert_eq!(escape_label("simple"), "simple");
        assert_eq!(escape_label("a\"b"), "a\\\"b");
        assert_eq!(escape_label("a\\b"), "a\\\\b");
        assert_eq!(escape_label("a\nb"), "a\\nb");
    }
}
