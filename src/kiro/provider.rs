//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use reqwest::header::HeaderMap;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::time::{sleep, timeout};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::metrics::{MetricsRecorder, RecordHandle, RequestKind, RequestRecord};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::{
    MultiTokenManager, TransientFailureKind, extract_suspicious_directory_key,
};
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// 短 Retry-After 视为无效的阈值（秒）
///
/// Kiro 上游在 429 时偶尔返回 `Retry-After: 0/1`，等同于"立即重试"。
/// 但客户端 1s 后立即重试该号仍在 Kiro 速率窗口内 → 又 429，
/// 9 次重试在 1-2 秒内全部失败，把 429 抛给客户端形成风暴循环。
/// 小于此阈值视为无效，让 Token Manager 走默认 cooldown (60s + jitter)。
const RETRY_AFTER_MIN_VALID_SECS: u64 = 5;

/// 解析上游响应的 `Retry-After` 头
///
/// 仅支持 delta-seconds（如 `Retry-After: 120`）；HTTP-date 格式较少见，
/// 暂返回 None（依赖 Token Manager 的默认 cooldown 时长）。
/// 上限由 Token Manager 的 RETRY_AFTER_MAX_CLAMP 兜底，本函数不做夹断。
///
/// 短 Retry-After（< `RETRY_AFTER_MIN_VALID_SECS` 秒）视为无效返回 None，
/// 防止"号池在 1-2 秒内被打废"的风暴循环（见常量注释）。
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    if raw < RETRY_AFTER_MIN_VALID_SECS {
        tracing::debug!(
            "忽略短 Retry-After: {}s（< {}s 视为无效，避免风暴循环）",
            raw,
            RETRY_AFTER_MIN_VALID_SECS
        );
        return None;
    }
    Some(Duration::from_secs(raw))
}

/// 上游 `ListAvailableModels` 返回的单个模型元数据（取我们关心的字段）。
#[derive(Debug, Clone)]
pub struct UpstreamModel {
    /// 模型 ID（如 `claude-opus-4.7`、`auto`、`deepseek-3.2`）
    pub model_id: String,
    /// 展示名（如 `Claude Opus 4.7`）
    pub model_name: String,
    /// 最大输出 token（用于 `/v1/models` 的 max_tokens；如 Opus 4.7 = 128000）
    pub max_output_tokens: Option<i32>,
}

/// 解析 `ListAvailableModels` 响应体为模型元数据列表（去重、保序）。
///
/// 上游真实响应（2026-05 实测）形如：
/// ```json
/// {"models":[{"modelId":"claude-opus-4.7","modelName":"Claude Opus 4.7",
///   "promptCaching":{"supportsPromptCaching":true,"minimumTokensPerCacheCheckpoint":4096},
///   "tokenLimits":{"maxInputTokens":1000000,"maxOutputTokens":128000}}], ...}
/// ```
/// 字段名优先 `modelId`/`modelName`，兼容 `id`/`name` 兜底。
fn parse_available_models(body: &str) -> anyhow::Result<Vec<UpstreamModel>> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("ListAvailableModels 响应非合法 JSON: {}", e))?;

    let models = value
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("ListAvailableModels 响应缺少 models 数组"))?;

    let mut out = Vec::with_capacity(models.len());
    let mut seen = HashSet::new();
    for m in models {
        let model_id = m
            .get("modelId")
            .or_else(|| m.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if model_id.is_empty() || !seen.insert(model_id.to_string()) {
            continue;
        }
        let model_name = m
            .get("modelName")
            .or_else(|| m.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or(model_id)
            .to_string();
        let max_output_tokens = m
            .get("tokenLimits")
            .and_then(|t| t.get("maxOutputTokens"))
            .and_then(|x| x.as_i64())
            .map(|n| n as i32);

        out.push(UpstreamModel {
            model_id: model_id.to_string(),
            model_name,
            max_output_tokens,
        });
    }
    Ok(out)
}

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
///
/// 9 → 18 → 6：上游按 `directory_id` 维度风控（实测所有 429 的 account 前缀
/// `d-9067c98495` 完全相同），每客户端请求 retry 18 次 = 18× 上游流量打到
/// 同一 directory，反而加重雪崩。降到 6 让上游压力 ÷3，配合 60s 总超时 + 客户端
/// 端的 Anthropic SDK 内置 retry，把分散流量交给客户端层完成。
const MAX_TOTAL_RETRIES: usize = 6;

/// 单次客户端请求的最大端到端时长（含所有 retry + cooldown 等待）
///
/// **CF 524 防御**：用户的 Claude Code 走 Cloudflare 反代到 kiro-rs，
/// Cloudflare 默认 `Proxy Read Timeout = 100s`（用户当前接 .cf 域名实测 120s）。
/// 如果 kiro-rs 内部 retry + cooldown 等待累计超过 CF 上限，CF 会直接给
/// 客户端发 `524 timeout`，客户端只能 retry，体验极差。
///
/// 设 60s 留 60s 缓冲：60s 内必须给客户端结论（成功响应或 429+Retry-After）。
/// 超时则视为"号池暂时打废"，让 `map_provider_error` 回 429+Retry-After:60
/// 让客户端 60s 后再来，期间号池有时间从 cooldown 恢复。
///
/// 仅作用于 retry 总时长——一旦 reqwest::Response 拿到（流式建连完成），
/// 后续 SSE 内容传输不在此 timeout 内（用 Body 自身的 read timeout 控制）。
///
/// **CF 524 实测**（08:14-08:17 23 个请求）：
/// - p90 = 90s（旧 timeout 触发）
/// - max = 120.5s（CF 切断 524 触发）
/// - 90s acquire + 30s stream = 120s，正好超 CF 120s read timeout
///
/// 改 60s 给 stream + CF 缓冲留 60s（acquire 60s + stream/flush 60s = 120s 内必结束）
const REQUEST_TOTAL_TIMEOUT_SECS: u64 = 60;

/// 总超时触发后给 anyhow 错误的固定标记
///
/// `map_provider_error` 通过 contains 此字符串识别"客户端超时"分支，
/// 与上游 429/OVERAGE/401 等错误区分，单独走 503 + Retry-After: 60 路径。
pub(crate) const REQUEST_TIMEOUT_MARKER: &str = "REQUEST_TOTAL_TIMEOUT";

/// 把外部调用结果分类为 metrics 消费的 [`RequestKind`]
///
/// 成功 → Success；其余按错误信息中是否包含"瞬态"关键词区分 TransientFail vs Error
/// （只用于 metrics 观测，不影响业务逻辑；分类仅作可视化粗粒度区分用）
fn classify_outcome<T>(result: &anyhow::Result<T>) -> RequestKind {
    match result {
        Ok(_) => RequestKind::Success,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("瞬态") || msg.contains("429") {
                RequestKind::TransientFail
            } else {
                RequestKind::Error
            }
        }
    }
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// 进程级请求指标记录器（与 Admin API 共享）
    metrics: Arc<MetricsRecorder>,
    /// 上游并发限制器（None = 不限制）
    ///
    /// 在 `call_api` / `call_api_stream` / `call_mcp` 入口 acquire 一个 permit，
    /// 一个客户端请求只占一个 slot（不管它内部重试多少次）。
    /// 避免复试风暴加重 Kiro 对账号的 suspicious activity 风控。
    inflight_limiter: Option<Arc<Semaphore>>,

    /// Tier 化 retry 的 fallback 代理 Client（None = 不启用，所有 attempt 走直连）
    ///
    /// 当直连重试达到 `fallback_proxy_after_attempts` 后仍未成功，
    /// 后续重试切换到该 Client 走代理出口（mihomo 等），让 source IP 多样化绕过 IP 风控。
    fallback_proxy_client: Option<Client>,

    /// 触发 fallback proxy 的 attempt 阈值
    ///
    /// 例如设 10：attempt 0-9 走直连，attempt >= 10 走 fallback proxy。
    fallback_proxy_after_attempts: usize,
}

impl KiroProvider {
    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
        metrics: Arc<MetricsRecorder>,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client
        let initial_client =
            build_client(proxy.as_ref(), 720, tls_backend).expect("创建 HTTP 客户端失败");
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), initial_client);

        // 从配置读取并发上限。0 视为禁用（不限），>0 创建 Semaphore。
        let inflight_limiter = token_manager
            .config()
            .max_inflight_kiro_requests
            .and_then(|n| {
                if n == 0 {
                    None
                } else {
                    Some(Arc::new(Semaphore::new(n as usize)))
                }
            });
        if let Some(ref sem) = inflight_limiter {
            tracing::info!(
                "上游并发限制器已启用: max_inflight={}",
                sem.available_permits()
            );
        }

        // Tier 化 retry: 配置了 fallback_proxy_url 才构建 fallback Client
        let cfg_for_fallback = token_manager.config();
        let (fallback_proxy_client, fallback_proxy_after_attempts) =
            match cfg_for_fallback.fallback_proxy_url.as_deref() {
                Some(url) if !url.is_empty() => {
                    let pc = ProxyConfig::new(url);
                    match build_client(Some(&pc), 720, tls_backend) {
                        Ok(c) => {
                            let threshold =
                                cfg_for_fallback.fallback_proxy_after_attempts.unwrap_or(10);
                            tracing::info!(
                                "Tier 化 retry fallback 代理已启用: url={}, after_attempts={}",
                                url,
                                threshold
                            );
                            (Some(c), threshold)
                        }
                        Err(e) => {
                            tracing::error!(
                                "fallback 代理 Client 构建失败 (url={}): {} — 已禁用",
                                url,
                                e
                            );
                            (None, usize::MAX)
                        }
                    }
                }
                _ => (None, usize::MAX),
            };

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint,
            metrics,
            inflight_limiter,
            fallback_proxy_client,
            fallback_proxy_after_attempts,
        }
    }

    /// 记录一次完整请求的结果到 metrics ring buffer，返回该记录的单调 seq
    ///
    /// `model` 与 `credential_id` 让 admin metrics 能按维度切片
    /// （e.g. "opus 模型 p95 是多少 / 哪个号在打瞬态"）。两者都允许 None：
    /// `model` 在 MCP / 错误路径下可能拿不到；`credential_id` 在所有 retry
    /// 都没获取到 token 时为 None。
    ///
    /// token 字段此刻填 None：流式请求记录发生在**建连完成**时，token 尚未产生；
    /// 由 anthropic 层在响应处理完成后凭返回的 seq 调 `metrics.update_tokens` 回填。
    fn record_outcome(
        &self,
        started_at: Instant,
        kind: RequestKind,
        used_fallback: bool,
        waited_for_cooldown: bool,
        model: Option<&str>,
        credential_id: Option<u64>,
    ) -> u64 {
        self.metrics.record(RequestRecord {
            seq: 0, // 占位，record() 内部分配真实 seq
            finished_at: Instant::now(),
            latency: started_at.elapsed(),
            kind,
            used_fallback,
            waited_for_cooldown,
            model: model.map(std::sync::Arc::from),
            credential_id,
            // 以下字段在后续 PR 中由调用方填入；当前路径暂默认填 None/false
            // 不影响现有行为，仅为 admin metrics 预留扩展位
            ttfb_ms: None,
            stream_aborted_midway: false,
            stream_recovered: false,
            client_visible_error: !matches!(kind, RequestKind::Success),
            used_fallback_proxy: false,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
        })
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials
            .effective_proxy_with_group(self.token_manager.config(), self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }
        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// Tier 化 retry：根据 attempt 决定走直连 client 还是 fallback proxy client
    ///
    /// - attempt < `fallback_proxy_after_attempts` → `client_for(credentials)`（直连，快）
    /// - attempt >= 阈值 且 fallback 已配置 → `fallback_proxy_client`（代理，IP 多样化）
    /// - 未配置 fallback → 永远走直连（沿用旧行为）
    ///
    /// 凭据自带 proxy（`proxy_url`）时优先级高于 fallback：
    /// 已配置专用代理的号继续走自己的代理，不会被 fallback 覆盖。
    fn client_for_attempt(
        &self,
        credentials: &KiroCredentials,
        attempt: usize,
    ) -> anyhow::Result<Client> {
        // 1. 凭据/分组有显式出口 → 固定走自己的出口，不被 fallback proxy 覆盖。
        if credentials.has_explicit_proxy_route(self.token_manager.config()) {
            return self.client_for(credentials);
        }
        // 2. 达到 fallback 阈值且全局 fallback 已配置 → 走 fallback proxy
        if attempt >= self.fallback_proxy_after_attempts
            && let Some(client) = &self.fallback_proxy_client
        {
            tracing::debug!(
                "attempt={} 已达 fallback 阈值 {}，切换到代理出口",
                attempt,
                self.fallback_proxy_after_attempts
            );
            return Ok(client.clone());
        }
        // 3. 默认：直连（fast path）
        self.client_for(credentials)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 在入口处获取并发 slot（若启用了 `inflight_limiter`）
    ///
    /// 返回的 permit 需要在请求结束（包括所有重试完成）后才释放，
    /// 因此把它存到调用者本地变量即可。
    async fn acquire_inflight_permit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let sem = self.inflight_limiter.as_ref()?.clone();
        let permit_t0 = Instant::now();
        // 先 clone 一份用于打日志（acquire_owned 会 move sem）
        let sem_for_log = sem.clone();
        match sem.acquire_owned().await {
            Ok(permit) => {
                let waited_ms = permit_t0.elapsed().as_millis() as u64;
                if waited_ms > 100 {
                    tracing::info!(
                        "上游并发限制：等待 slot {}ms, 剩余 permits={}",
                        waited_ms,
                        sem_for_log.available_permits()
                    );
                }
                Some(permit)
            }
            Err(e) => {
                tracing::error!("上游并发限制器已关闭: {}", e);
                None
            }
        }
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）
    /// 受 [`REQUEST_TOTAL_TIMEOUT_SECS`] 总超时保护，超时返回带
    /// [`REQUEST_TIMEOUT_MARKER`] 标记的错误供上层映射为 503+Retry-After。
    pub async fn call_api(
        &self,
        request_body: &str,
    ) -> anyhow::Result<(reqwest::Response, RecordHandle)> {
        let _permit = self.acquire_inflight_permit().await;
        let t0 = Instant::now();
        let mut used_fallback = false;
        let mut waited = false;
        let mut last_cred_id: Option<u64> = None;
        let model = Self::extract_model_from_request(request_body);
        let result = match timeout(
            Duration::from_secs(REQUEST_TOTAL_TIMEOUT_SECS),
            self.call_api_with_retry(
                request_body,
                false,
                &mut used_fallback,
                &mut waited,
                &mut last_cred_id,
            ),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_elapsed) => {
                tracing::error!(
                    elapsed_ms = t0.elapsed().as_millis() as u64,
                    "非流式请求触发总超时 {}s（CF 524 防御），放弃 retry 返回客户端 503",
                    REQUEST_TOTAL_TIMEOUT_SECS
                );
                Err(anyhow::anyhow!(
                    "{}: total request exceeded {}s budget",
                    REQUEST_TIMEOUT_MARKER,
                    REQUEST_TOTAL_TIMEOUT_SECS
                ))
            }
        };
        let kind = classify_outcome(&result);
        let seq = self.record_outcome(
            t0,
            kind,
            used_fallback,
            waited,
            model.as_deref(),
            last_cred_id,
        );
        result.map(|resp| (resp, RecordHandle::new(self.metrics.clone(), seq)))
    }

    /// 发送流式 API 请求
    ///
    /// 同 [`Self::call_api`] 同样受 90s 总超时保护：建连阶段（含 retry）超时
    /// 直接放弃，避免 CF 在 100/120s 切断后客户端瞎重试。
    /// 一旦建连完成（拿到 `reqwest::Response`），后续 SSE 流不再受此 timeout 影响。
    pub async fn call_api_stream(
        &self,
        request_body: &str,
    ) -> anyhow::Result<(reqwest::Response, RecordHandle)> {
        let _permit = self.acquire_inflight_permit().await;
        let t0 = Instant::now();
        let mut used_fallback = false;
        let mut waited = false;
        let mut last_cred_id: Option<u64> = None;
        let model = Self::extract_model_from_request(request_body);
        let result = match timeout(
            Duration::from_secs(REQUEST_TOTAL_TIMEOUT_SECS),
            self.call_api_with_retry(
                request_body,
                true,
                &mut used_fallback,
                &mut waited,
                &mut last_cred_id,
            ),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_elapsed) => {
                tracing::error!(
                    elapsed_ms = t0.elapsed().as_millis() as u64,
                    "流式建连触发总超时 {}s（CF 524 防御），放弃 retry 返回客户端 503",
                    REQUEST_TOTAL_TIMEOUT_SECS
                );
                Err(anyhow::anyhow!(
                    "{}: total request exceeded {}s budget",
                    REQUEST_TIMEOUT_MARKER,
                    REQUEST_TOTAL_TIMEOUT_SECS
                ))
            }
        };
        let kind = classify_outcome(&result);
        let seq = self.record_outcome(
            t0,
            kind,
            used_fallback,
            waited,
            model.as_deref(),
            last_cred_id,
        );
        result.map(|resp| (resp, RecordHandle::new(self.metrics.clone(), seq)))
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    ///
    /// 同样受 90s 总超时保护（避免 MCP 工具调用拖死客户端）。
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        let _permit = self.acquire_inflight_permit().await;
        let t0 = Instant::now();
        let mut used_fallback = false;
        let mut waited = false;
        let mut last_cred_id: Option<u64> = None;
        let result = match timeout(
            Duration::from_secs(REQUEST_TOTAL_TIMEOUT_SECS),
            self.call_mcp_with_retry(
                request_body,
                &mut used_fallback,
                &mut waited,
                &mut last_cred_id,
            ),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_elapsed) => {
                tracing::error!(
                    elapsed_ms = t0.elapsed().as_millis() as u64,
                    "MCP 请求触发总超时 {}s（CF 524 防御），放弃 retry 返回客户端 503",
                    REQUEST_TOTAL_TIMEOUT_SECS
                );
                Err(anyhow::anyhow!(
                    "{}: total MCP request exceeded {}s budget",
                    REQUEST_TIMEOUT_MARKER,
                    REQUEST_TOTAL_TIMEOUT_SECS
                ))
            }
        };
        let kind = classify_outcome(&result);
        // MCP 不绑定具体模型，model=None
        self.record_outcome(t0, kind, used_fallback, waited, None, last_cred_id);
        result
    }

    /// 获取上游（Kiro / CodeWhisperer）可用模型列表。
    ///
    /// 对应 Kiro IDE 使用的 `GET /ListAvailableModels?origin=AI_EDITOR`，
    /// 认证头与 MCP 一致（Bearer + profileArn）。响应形如
    /// `{"models":[{"id":"...","name":"..."}],"defaultModel":{...},"nextToken":...}`。
    ///
    /// 返回上游给出的完整模型元数据（[`UpstreamModel`]，去重、保序）。失败返回 `Err`，
    /// 由调用方决定是否回退到内置静态列表。最多尝试两张凭据，避免长时间阻塞
    /// `/v1/models` 这种轻量端点。
    pub async fn list_upstream_models(&self) -> anyhow::Result<Vec<UpstreamModel>> {
        let max_attempts = self.token_manager.total_count().clamp(1, 2);
        let mut last_error: Option<anyhow::Error> = None;

        for _ in 0..max_attempts {
            let ctx = match self.token_manager.acquire_context(None, None).await {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    self.token_manager.release_inflight(ctx.id);
                    last_error = Some(e);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.models_url(&rctx);
            let client = match self.client_for(&ctx.credentials) {
                Ok(c) => c,
                Err(e) => {
                    self.token_manager.release_inflight(ctx.id);
                    last_error = Some(e);
                    continue;
                }
            };

            // ListAvailableModels 是 GET，沿用 decorate_mcp 的鉴权头（含 profileArn）
            let base = client
                .get(&url)
                .header("content-type", "application/json");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    self.token_manager.release_inflight(ctx.id);
                    last_error = Some(e.into());
                    continue;
                }
            };

            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            if !status.is_success() {
                // 失效 token：报失败以触发该号 cooldown/刷新，换下一张
                self.token_manager.report_failure(ctx.id);
                last_error = Some(anyhow::anyhow!(
                    "ListAvailableModels 失败: {} {}",
                    status,
                    body
                ));
                continue;
            }

            self.token_manager.report_success(ctx.id);

            match parse_available_models(&body) {
                Ok(models) if !models.is_empty() => return Ok(models),
                Ok(_) => {
                    last_error = Some(anyhow::anyhow!("ListAvailableModels 返回空模型列表"));
                    continue;
                }
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("ListAvailableModels: 无可用凭据")))
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(
        &self,
        request_body: &str,
        used_fallback: &mut bool,
        waited: &mut bool,
        last_credential_id: &mut Option<u64>,
    ) -> anyhow::Result<reqwest::Response> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();

        for attempt in 0..max_retries {
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let ctx = match self.token_manager.acquire_context(None, None).await {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            // 跟踪本次请求最后命中的凭据 ID（用于 admin metrics 的 by_credential 聚合）
            *last_credential_id = Some(ctx.id);
            // 聚合 metrics 标志：本次请求过程中只要任何一次 acquire 走到 fallback/wait，
            // 整个请求就标记为 fallback/wait（语义"端到端是否经历过此分支"）
            if ctx.from_cooldown_fallback {
                *used_fallback = true;
            }
            if ctx.waited_for_cooldown {
                *waited = true;
            }

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let base = self
                .client_for_attempt(&ctx.credentials, attempt)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    self.token_manager.release_inflight(ctx.id);
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt, RetryReason::Network)).await;
                    }
                    continue;
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                return Ok(response);
            }

            // 在消费 body 前先抓 Retry-After 头（response.text() 会消费 response）
            let retry_after = parse_retry_after(response.headers());

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽（永久禁用）
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 402 + OVERAGE_REQUEST_LIMIT_EXCEEDED：开启 overage 后短窗口速率上限
            // 进入较长 cooldown 等待 hour/day 窗口刷新，**不**禁用凭据
            if status.as_u16() == 402 && endpoint.is_overage_request_limit(&body) {
                tracing::warn!(
                    "MCP 请求失败（OVERAGE 速率上限，凭据进入 cooldown 等待窗口刷新，尝试 {}/{}）: {}",
                    attempt + 1,
                    max_retries,
                    body
                );
                self.token_manager.report_transient_failure(
                    ctx.id,
                    TransientFailureKind::OverageRequestLimit,
                    retry_after,
                    ctx.from_cooldown_fallback,
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt, RetryReason::SwitchCredential)).await;
                }
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                self.token_manager.release_inflight(ctx.id);
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        self.token_manager.release_inflight(ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误：把该号放入短期 cooldown，下一轮 acquire 自动绕过
            // 5xx 不禁用凭据（仅 cooldown），避免上游短暂抖动导致全号池被误禁用
            // （参考 kiro2cc-proxy: "avoid cascade lock-out during upstream instability"）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                let kind = if status.as_u16() == 429 {
                    TransientFailureKind::classify_429(&body)
                } else {
                    TransientFailureKind::from_status(status.as_u16())
                };
                let directory_key = if matches!(kind, TransientFailureKind::SuspiciousActivity) {
                    extract_suspicious_directory_key(&body)
                } else {
                    None
                };
                self.token_manager.report_transient_failure_with_directory(
                    ctx.id,
                    kind,
                    retry_after,
                    ctx.from_cooldown_fallback,
                    directory_key.as_deref(),
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt, RetryReason::SwitchCredential)).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                self.token_manager.release_inflight(ctx.id);
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底：未知错误归类为上游瞬态，进入短期 cooldown
            self.token_manager.report_transient_failure(
                ctx.id,
                TransientFailureKind::UpstreamError,
                retry_after,
                ctx.from_cooldown_fallback,
            );
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt, RetryReason::SwitchCredential)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        used_fallback: &mut bool,
        waited: &mut bool,
        last_credential_id: &mut Option<u64>,
    ) -> anyhow::Result<reqwest::Response> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 尝试从请求体中提取模型信息
        let model = Self::extract_model_from_request(request_body);
        // 提取 conversation_id 用于 sticky session 路由
        let conversation_id = Self::extract_conversation_id(request_body);

        for attempt in 0..max_retries {
            // 获取调用上下文（绑定 index、credentials、token）
            let ctx = match self
                .token_manager
                .acquire_context(model.as_deref(), conversation_id.as_deref())
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            // 跟踪最后命中的凭据 ID
            *last_credential_id = Some(ctx.id);
            // 聚合 metrics 标志（端到端是否经历过 fallback / wait）
            if ctx.from_cooldown_fallback {
                *used_fallback = true;
            }
            if ctx.waited_for_cooldown {
                *waited = true;
            }

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.api_url(&rctx);
            let body = endpoint.transform_api_body(request_body, &rctx);

            let base = self
                .client_for_attempt(&ctx.credentials, attempt)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_api(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "API 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                    // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                    self.token_manager.release_inflight(ctx.id);
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt, RetryReason::Network)).await;
                    }
                    continue;
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                // 绑定 conversation_id 到该凭据（sticky session）
                if let Some(ref cid) = conversation_id {
                    self.token_manager.bind_conversation(cid, ctx.id);
                }
                return Ok(response);
            }

            // 在消费 body 前先抓 Retry-After 头
            let retry_after = parse_retry_after(response.headers());

            // 失败响应：读取 body 用于日志/错误信息
            let body = response.text().await.unwrap_or_default();

            // 402 Payment Required 且月度配额永久耗尽：禁用凭据并故障转移
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 402 + OVERAGE_REQUEST_LIMIT_EXCEEDED：开启 overage 后短窗口速率上限
            // 凭据进入较长 cooldown 等待 hour/day 窗口刷新，**不**禁用
            if status.as_u16() == 402 && endpoint.is_overage_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（OVERAGE 速率上限，凭据进入 cooldown 等待窗口刷新，尝试 {}/{}）: {}",
                    attempt + 1,
                    max_retries,
                    body
                );
                self.token_manager.report_transient_failure(
                    ctx.id,
                    TransientFailureKind::OverageRequestLimit,
                    retry_after,
                    ctx.from_cooldown_fallback,
                );
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt, RetryReason::SwitchCredential)).await;
                }
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换凭据无意义
            if status.as_u16() == 400 {
                self.token_manager.release_inflight(ctx.id);
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        self.token_manager.release_inflight(ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 429/408/5xx - 瞬态上游错误：把该凭据放入短期 cooldown，retry 自动选其他号
            // （避免 retry 全打到同一个被限号上）。不禁用凭据，cooldown 过期自动恢复。
            // 5xx 不禁用凭据（仅 cooldown），避免上游短暂抖动导致全号池被误禁用
            // （参考 kiro2cc-proxy: "avoid cascade lock-out during upstream instability"）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                let kind = if status.as_u16() == 429 {
                    TransientFailureKind::classify_429(&body)
                } else {
                    TransientFailureKind::from_status(status.as_u16())
                };
                let directory_key = if matches!(kind, TransientFailureKind::SuspiciousActivity) {
                    extract_suspicious_directory_key(&body)
                } else {
                    None
                };
                self.token_manager.report_transient_failure_with_directory(
                    ctx.id,
                    kind,
                    retry_after,
                    ctx.from_cooldown_fallback,
                    directory_key.as_deref(),
                );
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt, RetryReason::SwitchCredential)).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                self.token_manager.release_inflight(ctx.id);
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：未知错误归类为上游瞬态，进入短期 cooldown（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            self.token_manager.report_transient_failure(
                ctx.id,
                TransientFailureKind::UpstreamError,
                retry_after,
                ctx.from_cooldown_fallback,
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt, RetryReason::SwitchCredential)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// 从请求体中提取 conversationId（用于 sticky session 路由）
    fn extract_conversation_id(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("conversationId")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// 计算重试前的等待时间
    ///
    /// 区分两种 retry 场景：
    /// - [`RetryReason::SwitchCredential`]：上游 429/5xx 等错误，**下一次 retry
    ///   将自动切到不同凭据**（acquire_context 跳过 cooldown 中的号）。
    ///   原设计"零等待"是假设另一个号干净，但 `retry_after=1` 风暴后发现号池
    ///   可能整体被圈在"冷却 1s"状态，立即切号还是同一波 429。
    ///   改为 100ms × attempt 微退避（max 1s），让 acquire_context 有时间等到 1 个真醒的号。
    /// - [`RetryReason::Network`]：连接失败/超时等，可能是本地链路或上游整体抖动
    ///   → 沿用指数退避避免放大故障
    fn retry_delay(attempt: usize, reason: RetryReason) -> Duration {
        match reason {
            RetryReason::SwitchCredential => {
                // 切号微退避：100ms × (attempt+1)，上限 1000ms
                // attempt=0 → 100ms, attempt=1 → 200ms, ..., attempt>=9 → 1000ms
                let ms = 100u64
                    .saturating_mul((attempt as u64).saturating_add(1))
                    .min(1000);
                Duration::from_millis(ms)
            }
            RetryReason::Network => {
                // 指数退避 + 少量抖动
                const BASE_MS: u64 = 200;
                const MAX_MS: u64 = 2_000;
                let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
                let backoff = exp.min(MAX_MS);
                let jitter_max = (backoff / 4).max(1);
                let jitter = fastrand::u64(0..=jitter_max);
                Duration::from_millis(backoff.saturating_add(jitter))
            }
        }
    }
}

/// retry 触发原因，决定是否需要 backoff
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryReason {
    /// 上游业务错误（429/5xx/未知 status），下一次 retry 会切到不同凭据 → 不需要 backoff
    SwitchCredential,
    /// 网络层错误（连接失败/超时），可能是链路抖动 → 指数退避
    Network,
}

#[cfg(test)]
mod parse_models_tests {
    use super::parse_available_models;

    fn ids(body: &str) -> Vec<String> {
        parse_available_models(body)
            .unwrap()
            .into_iter()
            .map(|m| m.model_id)
            .collect()
    }

    #[test]
    fn parses_model_id_field() {
        let body = r#"{"models":[{"modelId":"claude-sonnet-4.5","modelName":"Sonnet"},{"modelId":"claude-opus-4.7","modelName":"Opus"}],"nextToken":null}"#;
        assert_eq!(ids(body), vec!["claude-sonnet-4.5", "claude-opus-4.7"]);
    }

    #[test]
    fn falls_back_to_id_alias() {
        let body = r#"{"models":[{"id":"claude-haiku-4.5"}]}"#;
        assert_eq!(ids(body), vec!["claude-haiku-4.5"]);
    }

    #[test]
    fn dedupes_and_preserves_order() {
        let body = r#"{"models":[{"modelId":"a"},{"modelId":"b"},{"modelId":"a"}]}"#;
        assert_eq!(ids(body), vec!["a", "b"]);
    }

    #[test]
    fn skips_empty_and_missing_ids() {
        let body = r#"{"models":[{"modelId":""},{"modelName":"no-id"},{"modelId":"valid"}]}"#;
        assert_eq!(ids(body), vec!["valid"]);
    }

    #[test]
    fn errors_on_invalid_json() {
        assert!(parse_available_models("not json").is_err());
    }

    #[test]
    fn errors_on_missing_models_array() {
        assert!(parse_available_models(r#"{"foo":"bar"}"#).is_err());
    }

    #[test]
    fn empty_models_array_returns_empty_vec() {
        assert!(
            parse_available_models(r#"{"models":[]}"#)
                .unwrap()
                .is_empty()
        );
    }

    /// 用 2026-05 服务器实测的真实响应片段验证元数据解析。
    #[test]
    fn parses_real_upstream_metadata() {
        let body = r#"{"defaultModel":{"modelId":"auto"},"models":[
            {"modelId":"claude-opus-4.7","modelName":"Claude Opus 4.7",
             "description":"Experimental preview of Claude Opus 4.7 model with 1M context window",
             "promptCaching":{"maximumCacheCheckpointsPerRequest":4,"minimumTokensPerCacheCheckpoint":4096,"supportsPromptCaching":true},
             "tokenLimits":{"maxInputTokens":1000000,"maxOutputTokens":128000},"rateMultiplier":2.2},
            {"modelId":"deepseek-3.2","modelName":"Deepseek v3.2",
             "promptCaching":{"supportsPromptCaching":false},
             "tokenLimits":{"maxInputTokens":164000,"maxOutputTokens":64000}}
        ]}"#;
        let models = parse_available_models(body).unwrap();
        assert_eq!(models.len(), 2);

        let opus = &models[0];
        assert_eq!(opus.model_id, "claude-opus-4.7");
        assert_eq!(opus.model_name, "Claude Opus 4.7");
        assert_eq!(opus.max_output_tokens, Some(128_000));

        let ds = &models[1];
        assert_eq!(ds.model_id, "deepseek-3.2");
        assert_eq!(ds.max_output_tokens, Some(64_000));
    }
}

#[cfg(test)]
mod retry_delay_tests {
    use super::{KiroProvider, RetryReason};
    use std::time::Duration;

    #[test]
    fn switch_credential_uses_micro_backoff() {
        // attempt=0 → 100ms（首次切号轻微退避）
        assert_eq!(
            KiroProvider::retry_delay(0, RetryReason::SwitchCredential),
            Duration::from_millis(100)
        );
        // attempt=1 → 200ms（线性增长）
        assert_eq!(
            KiroProvider::retry_delay(1, RetryReason::SwitchCredential),
            Duration::from_millis(200)
        );
        // attempt=4 → 500ms
        assert_eq!(
            KiroProvider::retry_delay(4, RetryReason::SwitchCredential),
            Duration::from_millis(500)
        );
        // attempt=9 → 1000ms（达到上限）
        assert_eq!(
            KiroProvider::retry_delay(9, RetryReason::SwitchCredential),
            Duration::from_millis(1000)
        );
        // attempt>=9 → 1000ms（封顶，不再增长）
        for attempt in 10..20 {
            assert_eq!(
                KiroProvider::retry_delay(attempt, RetryReason::SwitchCredential),
                Duration::from_millis(1000),
                "attempt={} 应封顶 1000ms",
                attempt
            );
        }
    }

    #[test]
    fn network_uses_exponential_backoff() {
        // attempt=0 → 200ms base + ≤25% jitter（25..=50ms）→ 200..=250ms
        let d0 = KiroProvider::retry_delay(0, RetryReason::Network);
        assert!(d0 >= Duration::from_millis(200) && d0 <= Duration::from_millis(250));
        // attempt=1 → 400ms base + ≤100ms jitter
        let d1 = KiroProvider::retry_delay(1, RetryReason::Network);
        assert!(d1 >= Duration::from_millis(400) && d1 <= Duration::from_millis(500));
        // attempt 较大时 cap 在 2000ms + jitter
        let d_high = KiroProvider::retry_delay(20, RetryReason::Network);
        assert!(d_high >= Duration::from_millis(2000) && d_high <= Duration::from_millis(2500));
    }
}

#[cfg(test)]
mod retry_after_tests {
    use super::{RETRY_AFTER_MIN_VALID_SECS, parse_retry_after};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::Duration;

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn parse_retry_after_short_returns_none() {
        // 短 retry_after（< RETRY_AFTER_MIN_VALID_SECS）应被视为无效，避免风暴循环
        for s in 0..RETRY_AFTER_MIN_VALID_SECS {
            let h = headers_with(&s.to_string());
            assert_eq!(parse_retry_after(&h), None, "Retry-After: {} 应被丢弃", s);
        }
    }

    #[test]
    fn parse_retry_after_normal_values_returned() {
        // 正常值（>= 阈值）应被正确解析
        for s in [5, 10, 30, 60, 120, 300] {
            let h = headers_with(&s.to_string());
            assert_eq!(
                parse_retry_after(&h),
                Some(Duration::from_secs(s)),
                "Retry-After: {} 应被解析",
                s
            );
        }
    }

    #[test]
    fn parse_retry_after_missing_header_returns_none() {
        let h = HeaderMap::new();
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_invalid_returns_none() {
        // HTTP-date 格式 / 非数字 → None
        let h = headers_with("Mon, 01 Jan 2030 00:00:00 GMT");
        assert_eq!(parse_retry_after(&h), None);
        let h = headers_with("not-a-number");
        assert_eq!(parse_retry_after(&h), None);
    }
}
