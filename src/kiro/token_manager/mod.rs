//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::token_refresh::{
    IdcRefreshRequest, IdcRefreshResponse, RefreshRequest, RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::model::config::Config;

// ============================================================================
// 多凭据 Token 管理器
// ============================================================================

/// 单个凭据条目的状态
struct CredentialEntry {
    /// 凭据唯一 ID
    id: u64,
    /// 凭据信息
    credentials: KiroCredentials,
    /// API 调用连续失败次数
    failure_count: u32,
    /// Token 刷新连续失败次数
    refresh_failure_count: u32,
    /// 是否已禁用
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）
    disabled_reason: Option<DisabledReason>,
    /// API 调用成功次数
    success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    last_used_at: Option<String>,
    /// 当前 in-flight 请求数（balanced 模式按此分发并发，请求开始 +1，结束 -1）
    inflight: u32,
    /// 上游瞬态错误（429/408/5xx）累计次数（不参与禁用判定，仅供观测）
    transient_failure_count: u64,
    /// 最近一次瞬态错误时间（RFC3339 格式）
    last_transient_failure_at: Option<String>,
    /// 最近一次瞬态错误时刻（进程内单调时钟），用于 select 时偏好"最久未失败号"
    /// 跨进程无意义、不持久化；启动时为 None 表示该号自启动后没失败过 → 最优先
    last_transient_at_instant: Option<Instant>,
    /// 冷却截止时间（>now 时该凭据被 acquire 跳过；None 表示无冷却）
    /// 不持久化（重启即清，符合 Instant 跨进程无意义的语义）
    cooldown_until: Option<Instant>,
    /// 最近一次进入冷却的原因（用于日志/Admin 观测）
    cooldown_reason: Option<TransientFailureKind>,
    /// 从 suspicious activity 响应体学习到的 Kiro directory key（如 `d-9067c98495`）。
    /// Kiro 风控按 directory 生效；知道同组 key 后可以一起冷却，避免轮询打爆同目录账号。
    directory_key: Option<String>,
    /// Per-credential 并发限制 semaphore（None = 不限制）
    ///
    /// 非阻塞 try_acquire 模式：选号时调用 `sem.try_acquire_owned()`，成功则把
    /// permit **移交给本次请求的 [`CallContext`]**（per-request 持有，请求结束 drop
    /// 时自动释放），而非存回共享 entry——后者会在两个请求选中同一号时被覆盖、
    /// 提前释放，导致 `max_inflight_per_credential` 被绕过。
    /// 防止单个凭据同时承受过多请求被 Kiro 风控。
    permit_semaphore: Option<Arc<Semaphore>>,
}

/// 判断凭据当前是否处于冷却中
fn is_in_cooldown(entry: &CredentialEntry, now: Instant) -> bool {
    matches!(entry.cooldown_until, Some(until) if until > now)
}

/// 给 cooldown 时长加 ±20% 随机 jitter，错峰恢复，避免一批号同步进/出 cooldown
/// 导致的"集体雪暴"现象（限流期间多个号几乎同时被打、几乎同时恢复、又几乎同时再被打）。
///
/// 实现：返回 `[duration * 0.8, duration * 1.2]` 区间内的均匀随机值。
/// 当 `duration < 5` 秒时跳过 jitter（区间过窄无意义），原值返回。
fn apply_cooldown_jitter(duration: StdDuration) -> StdDuration {
    let secs = duration.as_secs();
    if secs < 5 {
        return duration;
    }
    let lo = (secs as f64 * 0.8) as u64;
    let hi = (secs as f64 * 1.2) as u64;
    let jittered = fastrand::u64(lo..=hi);
    StdDuration::from_secs(jittered)
}

/// 禁用原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledReason {
    /// Admin API 手动禁用
    Manual,
    /// 连续失败达到阈值后自动禁用
    TooManyFailures,
    /// Token 刷新连续失败达到阈值后自动禁用
    TooManyRefreshFailures,
    /// 额度已用尽（如 MONTHLY_REQUEST_COUNT）
    QuotaExceeded,
    /// Refresh Token 永久失效（服务端返回 invalid_grant）
    InvalidRefreshToken,
    /// 凭据配置无效（如 authMethod=api_key 但缺少 kiroApiKey）
    InvalidConfig,
}

/// 统计数据持久化条目
#[derive(Serialize, Deserialize)]
struct StatsEntry {
    success_count: u64,
    last_used_at: Option<String>,
    /// 上游瞬态错误累计次数（v2 新增，旧文件 default = 0）
    #[serde(default)]
    transient_failure_count: u64,
    /// 最近一次瞬态错误时间（v2 新增，旧文件 default = None）
    #[serde(default)]
    last_transient_failure_at: Option<String>,
}

// ============================================================================
// Admin API 公开结构
// ============================================================================

/// 凭据条目快照（用于 Admin API 读取）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEntrySnapshot {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// Token 过期时间
    pub expires_at: Option<String>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 当前 in-flight 请求数（用于 balanced 调度可观测性）
    pub inflight: u32,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// 凭据分组 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 有效代理来源
    pub proxy_source: String,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 端点名称（未显式配置时返回 None，由 Admin 层回退到默认值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 上游瞬态错误（429/408/5xx）累计次数
    #[serde(default)]
    pub transient_failure_count: u64,
    /// 最近一次瞬态错误时间（RFC3339）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_transient_failure_at: Option<String>,
    /// 当前冷却剩余秒数（0 表示不在冷却中）
    #[serde(default)]
    pub cooldown_remaining_seconds: u64,
    /// 当前冷却原因（"rate_limit" / "timeout" / "upstream_error"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
    /// 从 suspicious activity 响应体学习到的 Kiro directory key
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory_key: Option<String>,
}

/// 凭据管理器状态快照
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSnapshot {
    /// 凭据条目列表
    pub entries: Vec<CredentialEntrySnapshot>,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 总凭据数量
    pub total: usize,
    /// 可用凭据数量
    pub available: usize,
}

/// Sticky session 路由器：将 conversation_id 绑定到 credential_id
///
/// 同一会话的后续请求优先路由到同一凭据，提升 Kiro 后端 prompt cache 命中率。
/// 绑定有效期默认 1 小时，超时后自动回退到正常选号逻辑。
struct StickyRouter {
    bindings: Mutex<HashMap<String, (u64, Instant)>>,
    ttl: StdDuration,
}

impl StickyRouter {
    fn new(ttl: StdDuration) -> Self {
        Self {
            bindings: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// 查询绑定：返回 credential_id 如果绑定未过期
    fn get(&self, conversation_id: &str) -> Option<u64> {
        let map = self.bindings.lock();
        map.get(conversation_id)
            .filter(|(_, expires_at)| Instant::now() < *expires_at)
            .map(|(id, _)| *id)
    }

    /// 记录绑定（请求成功后调用）
    fn bind(&self, conversation_id: &str, credential_id: u64) {
        let mut map = self.bindings.lock();
        // 超过 4096 条时清理过期条目，防内存泄漏
        if map.len() > 4096 {
            let now = Instant::now();
            map.retain(|_, (_, exp)| now < *exp);
        }
        map.insert(
            conversation_id.to_string(),
            (credential_id, Instant::now() + self.ttl),
        );
    }
}

/// 多凭据 Token 管理器
///
/// 支持多个凭据的管理，实现固定优先级 + 故障转移策略
/// 故障统计基于 API 调用结果，而非 Token 刷新结果
pub struct MultiTokenManager {
    config: Config,
    proxy: Option<ProxyConfig>,
    /// 凭据条目列表
    entries: Mutex<Vec<CredentialEntry>>,
    /// 当前活动凭据 ID
    current_id: Mutex<u64>,
    /// Token 刷新锁，per-credential 避免跨凭据阻塞
    refresh_locks: Mutex<HashMap<u64, Arc<TokioMutex<()>>>>,
    /// 凭据文件路径（用于回写）
    credentials_path: Option<PathBuf>,
    /// 是否为多凭据格式（数组格式才回写）
    is_multiple_format: bool,
    /// 负载均衡模式（运行时可修改）
    load_balancing_mode: Mutex<String>,
    /// 最近一次统计持久化时间（用于 debounce）
    last_stats_save_at: Mutex<Option<Instant>>,
    /// 统计数据是否有未落盘更新
    stats_dirty: AtomicBool,
    /// 可选的运行时 retry 配置句柄（生产代码 attach；测试默认 None 走 config）
    retry_config: Mutex<Option<crate::model::runtime::SharedRetryConfig>>,
    /// Sticky session 路由器（conversation_id → credential_id 绑定）
    sticky_router: StickyRouter,
}

/// 每个凭据最大 API 调用失败次数
const MAX_FAILURES_PER_CREDENTIAL: u32 = 3;
/// 统计数据持久化防抖间隔
const STATS_SAVE_DEBOUNCE: StdDuration = StdDuration::from_secs(30);

/// 429 限流的默认 cooldown 时长（可被 config.rate_limit_cooldown_sec 或 Retry-After 覆盖）
const DEFAULT_RATE_LIMIT_COOLDOWN: StdDuration = StdDuration::from_secs(120);
/// 408/5xx 的默认 cooldown 时长（可被 config.upstream_error_cooldown_sec 覆盖）
const DEFAULT_UPSTREAM_ERROR_COOLDOWN: StdDuration = StdDuration::from_secs(30);
/// 402 OVERAGE_REQUEST_LIMIT_EXCEEDED 的默认 cooldown 时长
///
/// Kiro 上游 overage 速率限制窗口通常以小时计；默认 600s = 10 分钟，
/// 既避免长时间锁死号、又给窗口足够时间刷新。可被
/// `config.overage_request_cooldown_sec` 覆盖。
const DEFAULT_OVERAGE_REQUEST_COOLDOWN: StdDuration = StdDuration::from_secs(600);
/// "suspicious activity" directory 级别封禁的默认 cooldown 时长
///
/// Kiro 返回 "Due to suspicious activity, we are imposing temporary limits" 表示
/// directory 维度的风控触发，所有共享该 directory_id 的凭据同时受限。
/// 默认 300s = 5 分钟，给上游足够时间解除风控。可被
/// `config.suspicious_activity_cooldown_sec` 覆盖。
const DEFAULT_SUSPICIOUS_ACTIVITY_COOLDOWN: StdDuration = StdDuration::from_secs(300);
/// Retry-After 头允许的最大 cooldown（防上游传入异常大值锁死凭据）
const RETRY_AFTER_MAX_CLAMP: StdDuration = StdDuration::from_secs(300);
/// 全员 cooldown 时 acquire_context 智能等待最早过期号的**默认**上限。
///
/// 通过 `RetryRuntimeConfig.max_fallback_wait_secs` 可热调（范围 [3, 120]s）。
/// 默认 60s 让客户端在"全员上游限流"时仍有机会拿到 200 而不是 502；
/// 服务端等待期间号池透明，客户端无感。
const DEFAULT_MAX_FALLBACK_WAIT: StdDuration = StdDuration::from_secs(60);

/// 单次 acquire 内"等待 + 重选"循环的**默认**最大轮数。
///
/// 通过 `RetryRuntimeConfig.max_fallback_wait_attempts` 可热调（范围 [1, 10]）。
/// 默认 5 让 cooldown 不齐的号池有 5 次机会等到下一批号过期。
const DEFAULT_MAX_FALLBACK_WAIT_ATTEMPTS: u32 = 5;

/// API 调用上下文
///
/// 绑定特定凭据的调用上下文，确保 token、credentials 和 id 的一致性
/// 用于解决并发调用时 current_id 竞态问题
pub struct CallContext {
    /// 凭据 ID（用于 report_success/report_failure）
    pub id: u64,
    /// 凭据信息（用于构建请求头）
    pub credentials: KiroCredentials,
    /// 访问 Token
    pub token: String,
    /// 本次调用是否来自"全员 cooldown" fallback 路径
    ///
    /// 当所有可用凭据都在 cooldown 中、调度器无奈挑出"最早过期"的号时，
    /// 标记为 true。后续 [`MultiTokenManager::report_transient_failure`] 看到此标记
    /// 会**只累计计数、不再延长 cooldown**——避免反复刷新同一组号的 cooldown
    /// 导致永远没有号能真正恢复。
    pub from_cooldown_fallback: bool,
    /// 本次 acquire 是否经历了"全员 cooldown 智能等待"路径
    ///
    /// 用于 metrics 观测：true 表示调度器为了避免 fallback 借号、主动 sleep
    /// 等到最早过期的号恢复后再返回（提供观测信号，与正确性无关）。
    pub waited_for_cooldown: bool,
    /// 本次请求持有的 per-credential 并发 permit（per-request 所有权）。
    ///
    /// 选号时从该凭据的 `permit_semaphore` acquire 得到；`CallContext` 被 drop 时
    /// permit 自动归还 semaphore，从而精确地把"在途请求数"约束在
    /// `max_inflight_per_credential`。`None` 表示未配置限制或满载降级。
    /// 字段顺序置于末尾，确保即便手工构造也不影响其余字段语义。
    pub(crate) concurrency_permit: Option<OwnedSemaphorePermit>,
}

mod acquire;
mod admin_ops;
mod failure;
mod failure_kind;
mod persistence;
mod refresh;
mod selection;

pub use failure_kind::{TransientFailureKind, extract_suspicious_directory_key};
// refresh.rs 持有无状态刷新 HTTP 函数 + 调度层；这些自由函数被多个子模块经 `super::*` 引用
pub(crate) use refresh::RefreshTokenInvalidError;
use refresh::{
    get_usage_limits, is_token_expired, is_token_expiring_soon, mask_api_key, refresh_token,
    sha256_hex, validate_refresh_token,
};

impl MultiTokenManager {
    /// 创建多凭据 Token 管理器
    ///
    /// # Arguments
    /// * `config` - 应用配置
    /// * `credentials` - 凭据列表
    /// * `proxy` - 可选的代理配置
    /// * `credentials_path` - 凭据文件路径（用于回写）
    /// * `is_multiple_format` - 是否为多凭据格式（数组格式才回写）
    pub fn new(
        config: Config,
        credentials: Vec<KiroCredentials>,
        proxy: Option<ProxyConfig>,
        credentials_path: Option<PathBuf>,
        is_multiple_format: bool,
    ) -> anyhow::Result<Self> {
        // 计算当前最大 ID，为没有 ID 的凭据分配新 ID
        let max_existing_id = credentials.iter().filter_map(|c| c.id).max().unwrap_or(0);
        let mut next_id = max_existing_id + 1;
        let mut has_new_ids = false;
        let mut has_new_machine_ids = false;
        let config_ref = &config;

        // 创建 per-credential 并发限制 semaphore（None = 不限制）
        let per_cred_semaphore: Option<Arc<Semaphore>> =
            config.max_inflight_per_credential.and_then(|n| {
                if n == 0 {
                    None
                } else {
                    Some(Arc::new(Semaphore::new(n as usize)))
                }
            });

        let entries: Vec<CredentialEntry> = credentials
            .into_iter()
            .map(|mut cred| {
                cred.canonicalize_auth_method();
                let id = cred.id.unwrap_or_else(|| {
                    let id = next_id;
                    next_id += 1;
                    cred.id = Some(id);
                    has_new_ids = true;
                    id
                });
                if cred.machine_id.is_none() {
                    cred.machine_id =
                        Some(machine_id::generate_from_credentials(&cred, config_ref));
                    has_new_machine_ids = true;
                }
                CredentialEntry {
                    id,
                    credentials: cred.clone(),
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled: cred.disabled, // 从配置文件读取 disabled 状态
                    disabled_reason: if cred.disabled {
                        Some(DisabledReason::Manual)
                    } else {
                        None
                    },
                    success_count: 0,
                    last_used_at: None,
                    inflight: 0,
                    transient_failure_count: 0,
                    last_transient_failure_at: None,
                    last_transient_at_instant: None,
                    cooldown_until: None,
                    cooldown_reason: None,
                    directory_key: None,
                    permit_semaphore: per_cred_semaphore.clone(),
                }
            })
            .collect();

        // 校验 API Key 凭据配置完整性：authMethod=api_key 时必须提供 kiroApiKey
        let mut entries = entries;
        for entry in &mut entries {
            if entry.credentials.kiro_api_key.is_none()
                && entry
                    .credentials
                    .auth_method
                    .as_deref()
                    .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                    .unwrap_or(false)
            {
                tracing::warn!(
                    "凭据 #{} 配置了 authMethod=api_key 但缺少 kiroApiKey 字段，已自动禁用",
                    entry.id
                );
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::InvalidConfig);
            }
        }

        // 检测重复 ID
        let mut seen_ids = std::collections::HashSet::new();
        let mut duplicate_ids = Vec::new();
        for entry in &entries {
            if !seen_ids.insert(entry.id) {
                duplicate_ids.push(entry.id);
            }
        }
        if !duplicate_ids.is_empty() {
            anyhow::bail!("检测到重复的凭据 ID: {:?}", duplicate_ids);
        }

        // 选择初始凭据：优先级最高（priority 最小）的可用凭据，无可用凭据时为 0
        let initial_id = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
            .map(|e| e.id)
            .unwrap_or(0);

        let load_balancing_mode = config.load_balancing_mode.clone();
        let manager = Self {
            config,
            proxy,
            entries: Mutex::new(entries),
            current_id: Mutex::new(initial_id),
            refresh_locks: Mutex::new(HashMap::new()),
            credentials_path,
            is_multiple_format,
            load_balancing_mode: Mutex::new(load_balancing_mode),
            last_stats_save_at: Mutex::new(None),
            stats_dirty: AtomicBool::new(false),
            retry_config: Mutex::new(None),
            sticky_router: StickyRouter::new(StdDuration::from_secs(3600)),
        };

        // 如果有新分配的 ID 或新生成的 machineId，立即持久化到配置文件
        if has_new_ids || has_new_machine_ids {
            if let Err(e) = manager.persist_credentials() {
                tracing::warn!("补全凭据 ID/machineId 后持久化失败: {}", e);
            } else {
                tracing::info!("已补全凭据 ID/machineId 并写回配置文件");
            }
        }

        // 加载持久化的统计数据（success_count, last_used_at）
        manager.load_stats();

        Ok(manager)
    }

    /// 获取配置的引用
    pub fn config(&self) -> &Config {
        &self.config
    }

    fn effective_proxy_for(&self, credentials: &KiroCredentials) -> Option<ProxyConfig> {
        credentials.effective_proxy_with_group(&self.config, self.proxy.as_ref())
    }

    /// 获取凭据总数
    pub fn total_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 获取可用凭据数量
    pub fn available_count(&self) -> usize {
        self.entries.lock().iter().filter(|e| !e.disabled).count()
    }

    /// 关联运行时 retry 配置句柄
    ///
    /// 调用后 [`Self::report_transient_failure`] 会优先读取该共享配置；
    /// 未关联（默认）时退回 `self.config` 中的字段。
    pub fn attach_retry_config(&self, handle: crate::model::runtime::SharedRetryConfig) {
        *self.retry_config.lock() = Some(handle);
    }

    /// 获取缓存目录（凭据文件所在目录）
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }
}

impl Drop for MultiTokenManager {
    fn drop(&mut self) {
        if self.stats_dirty.load(Ordering::Relaxed) {
            self.save_stats();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)] // mock 数据构造保持可读性

    use super::*;

    #[test]
    fn test_is_token_expired_with_expired_token() {
        let mut credentials = KiroCredentials::default();
        credentials.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_with_valid_token() {
        let mut credentials = KiroCredentials::default();
        let future = Utc::now() + Duration::hours(1);
        credentials.expires_at = Some(future.to_rfc3339());
        assert!(!is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_within_5_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(3);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_no_expires_at() {
        let credentials = KiroCredentials::default();
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_within_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(8);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_beyond_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(15);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(!is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_validate_refresh_token_missing() {
        let credentials = KiroCredentials::default();
        let result = validate_refresh_token(&credentials);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_refresh_token_valid() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("a".repeat(150));
        let result = validate_refresh_token(&credentials);
        assert!(result.is_ok());
    }

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[tokio::test]
    async fn test_refresh_token_rejects_api_key_credential() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.kiro_api_key = Some("ksk_test_key_123".to_string());
        credentials.auth_method = Some("api_key".to_string());

        let result = refresh_token(&credentials, &config, None).await;

        assert!(result.is_err(), "API Key 凭据应被 refresh_token 拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("API Key 凭据不支持刷新"),
            "期望错误消息包含 'API Key 凭据不支持刷新'，实际: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_refresh_token() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.refresh_token = Some("a".repeat(150));

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("凭据已存在"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_success() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        let id = result.unwrap();
        assert!(id > 0);
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_api_key() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing_key".to_string());
        existing.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.kiro_api_key = Some("ksk_existing_key".to_string());
        duplicate.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("kiroApiKey 重复")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_empty_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some(String::new());
        cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("kiroApiKey 为空")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_missing_key_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        // kiro_api_key is None

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("缺少 kiroApiKey")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_and_oauth_coexist() {
        let config = Config::default();

        let mut oauth_cred = KiroCredentials::default();
        oauth_cred.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![oauth_cred], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_new_key".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    // MultiTokenManager 测试

    #[test]
    fn test_multi_token_manager_new() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.priority = 0;
        let mut cred2 = KiroCredentials::default();
        cred2.priority = 1;

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    #[test]
    fn test_multi_token_manager_empty_credentials() {
        let config = Config::default();
        let result = MultiTokenManager::new(config, vec![], None, None, false);
        // 支持 0 个凭据启动（可通过管理面板添加）
        assert!(result.is_ok());
        let manager = result.unwrap();
        assert_eq!(manager.total_count(), 0);
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_duplicate_ids() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.id = Some(1);
        let mut cred2 = KiroCredentials::default();
        cred2.id = Some(1); // 重复 ID

        let result = MultiTokenManager::new(config, vec![cred1, cred2], None, None, false);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("重复的凭据 ID"),
            "错误消息应包含 '重复的凭据 ID'，实际: {}",
            err_msg
        );
    }

    #[test]
    fn test_multi_token_manager_api_key_missing_kiro_api_key_auto_disabled() {
        let config = Config::default();

        // auth_method=api_key 但缺少 kiro_api_key → 应被自动禁用
        let mut bad_cred = KiroCredentials::default();
        bad_cred.auth_method = Some("api_key".to_string());
        // kiro_api_key 保持 None

        let mut good_cred = KiroCredentials::default();
        good_cred.refresh_token = Some("valid_token".to_string());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 1); // bad_cred 被禁用，只剩 1 个可用
    }

    #[test]
    fn test_multi_token_manager_api_key_with_kiro_api_key_not_disabled() {
        let config = Config::default();

        // auth_method=api_key 且有 kiro_api_key → 不应被禁用
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("ksk_test123".to_string());

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_report_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        // 前两次失败不会禁用（使用 ID 1）
        assert!(manager.report_failure(1));
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 2);

        // 第三次失败会禁用第一个凭据
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 1);

        // 继续失败第二个凭据（使用 ID 2）
        assert!(manager.report_failure(2));
        assert!(manager.report_failure(2));
        assert!(!manager.report_failure(2)); // 所有凭据都禁用了
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_report_success() {
        let config = Config::default();
        let cred = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 失败两次（使用 ID 1）
        manager.report_failure(1);
        manager.report_failure(1);

        // 成功后重置计数（使用 ID 1）
        manager.report_success(1);

        // 再失败两次不会禁用
        manager.report_failure(1);
        manager.report_failure(1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_switch_to_next() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.refresh_token = Some("token1".to_string());
        let mut cred2 = KiroCredentials::default();
        cred2.refresh_token = Some("token2".to_string());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        let initial_id = manager.snapshot().current_id;

        // 切换到下一个
        assert!(manager.switch_to_next());
        assert_ne!(manager.snapshot().current_id, initial_id);
    }

    #[test]
    fn test_set_load_balancing_mode_persists_to_config_file() {
        let config_path =
            std::env::temp_dir().join(format!("kiro-load-balancing-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, r#"{"loadBalancingMode":"priority"}"#).unwrap();

        let config = Config::load(&config_path).unwrap();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        manager
            .set_load_balancing_mode("balanced".to_string())
            .unwrap();

        let persisted = Config::load(&config_path).unwrap();
        assert_eq!(persisted.load_balancing_mode, "balanced");
        assert_eq!(manager.get_load_balancing_mode(), "balanced");

        std::fs::remove_file(&config_path).unwrap();
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_auto_recovers_all_disabled() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(2);
        }

        assert_eq!(manager.available_count(), 0);

        // 应触发自愈：重置失败计数并重新启用，避免必须重启进程
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert!(ctx.token == "t1" || ctx.token == "t2");
        assert_eq!(manager.available_count(), 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_balanced_retries_until_bad_credential_disabled()
     {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut bad_cred = KiroCredentials::default();
        bad_cred.priority = 0;
        bad_cred.refresh_token = Some("bad".to_string());

        let mut good_cred = KiroCredentials::default();
        good_cred.priority = 1;
        good_cred.access_token = Some("good-token".to_string());
        good_cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();

        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "good-token");
    }

    #[test]
    fn test_multi_token_manager_report_refresh_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL - 1) {
            assert!(manager.report_refresh_failure(1));
        }
        assert_eq!(manager.available_count(), 2);

        assert!(manager.report_refresh_failure(1));
        assert_eq!(manager.available_count(), 1);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(first.refresh_failure_count, MAX_FAILURES_PER_CREDENTIAL);
        assert_eq!(snapshot.current_id, 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_refresh_failure_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_refresh_failure(1);
            manager.report_refresh_failure(2);
        }
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None, None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
    }

    #[test]
    fn test_multi_token_manager_report_quota_exhausted() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        assert_eq!(manager.available_count(), 2);
        assert!(manager.report_quota_exhausted(1));
        assert_eq!(manager.available_count(), 1);

        // 再禁用第二个后，无可用凭据
        assert!(!manager.report_quota_exhausted(2));
        assert_eq!(manager.available_count(), 0);
    }

    #[tokio::test]
    async fn test_multi_token_manager_quota_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        manager.report_quota_exhausted(1);
        manager.report_quota_exhausted(2);
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None, None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
        assert_eq!(manager.available_count(), 0);
    }

    // ============ 凭据级 Region 优先级测试 ============

    #[test]
    fn test_credential_region_priority_uses_credential_auth_region() {
        // 凭据配置了 auth_region 时，应使用凭据的 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-west-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-west-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_credential_region() {
        // 凭据未配置 auth_region 但配置了 region 时，应回退到凭据.region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-central-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_config() {
        // 凭据未配置 auth_region 和 region 时，应回退到 config
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials::default();
        assert!(credentials.auth_region.is_none());
        assert!(credentials.region.is_none());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_multiple_credentials_use_respective_regions() {
        // 多凭据场景下，不同凭据使用各自的 auth_region
        let mut config = Config::default();
        config.region = "ap-northeast-1".to_string();

        let mut cred1 = KiroCredentials::default();
        cred1.auth_region = Some("us-east-1".to_string());

        let mut cred2 = KiroCredentials::default();
        cred2.region = Some("eu-west-1".to_string());

        let cred3 = KiroCredentials::default(); // 无 region，使用 config

        assert_eq!(cred1.effective_auth_region(&config), "us-east-1");
        assert_eq!(cred2.effective_auth_region(&config), "eu-west-1");
        assert_eq!(cred3.effective_auth_region(&config), "ap-northeast-1");
    }

    #[test]
    fn test_idc_oidc_endpoint_uses_credential_auth_region() {
        // 验证 IdC OIDC endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);

        assert_eq!(refresh_url, "https://oidc.eu-central-1.amazonaws.com/token");
    }

    #[test]
    fn test_social_refresh_endpoint_uses_credential_auth_region() {
        // 验证 Social refresh endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("ap-southeast-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);

        assert_eq!(
            refresh_url,
            "https://prod.ap-southeast-1.auth.desktop.kiro.dev/refreshToken"
        );
    }

    #[test]
    fn test_api_call_uses_effective_api_region() {
        // 验证 API 调用使用 effective_api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-west-1".to_string());

        // 凭据.region 不参与 api_region 回退链
        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.us-west-2.amazonaws.com");
    }

    #[test]
    fn test_api_call_uses_credential_api_region() {
        // 凭据配置了 api_region 时，API 调用应使用凭据的 api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.api_region = Some("eu-central-1".to_string());

        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.eu-central-1.amazonaws.com");
    }

    #[test]
    fn test_credential_region_empty_string_treated_as_set() {
        // 空字符串 auth_region 被视为已设置（虽然不推荐，但行为应一致）
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("".to_string());

        let region = credentials.effective_auth_region(&config);
        // 空字符串被视为已设置，不会回退到 config
        assert_eq!(region, "");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("auth-only".to_string());
        credentials.api_region = Some("api-only".to_string());

        assert_eq!(credentials.effective_auth_region(&config), "auth-only");
        assert_eq!(credentials.effective_api_region(&config), "api-only");
    }

    // ========================================================================
    // Transient Failure Cooldown 测试（Bug A/B/C/D 修复）
    // ========================================================================

    #[test]
    fn test_overage_request_limit_uses_long_cooldown() {
        // 验证 OverageRequestLimit 走 DEFAULT_OVERAGE_REQUEST_COOLDOWN (600s)
        // 而不是 RateLimit 的 60s 或 UpstreamError 的 10s
        let config = Config::default();
        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        manager.report_transient_failure(1, TransientFailureKind::OverageRequestLimit, None, false);

        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|x| x.id == 1).unwrap();

        assert_eq!(e.transient_failure_count, 1, "瞬态失败计数应 +1");
        assert_eq!(
            e.cooldown_reason.as_deref(),
            Some("overage_request_limit"),
            "cooldown_reason 应为 overage_request_limit"
        );
        // 默认 600s + ±20% jitter → [480, 720]s
        assert!(
            (479..=720).contains(&e.cooldown_remaining_seconds),
            "OVERAGE 默认 cooldown 应在 [480,720] (600s±20% jitter)，实际 {}s",
            e.cooldown_remaining_seconds
        );
    }

    #[test]
    fn test_overage_request_limit_ignores_short_retry_after() {
        // OVERAGE 不应被 retry_after 缩短到默认值以下（窗口长，短 retry_after 误导）
        let config = Config::default();
        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 即使上游传 retry_after=5s，OVERAGE 也应使用默认 cooldown 600s
        manager.report_transient_failure(
            1,
            TransientFailureKind::OverageRequestLimit,
            Some(StdDuration::from_secs(5)),
            false,
        );

        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|x| x.id == 1).unwrap();
        assert!(
            e.cooldown_remaining_seconds >= 100,
            "OVERAGE 不接受短 retry_after 缩短，cooldown 应远大于 5s，实际 {}s",
            e.cooldown_remaining_seconds
        );
    }

    #[test]
    fn test_overage_request_cooldown_runtime_config_override() {
        use crate::model::runtime::shared_retry_config_from;

        // 验证运行时配置可覆盖默认 600s
        let mut config = Config::default();
        config.overage_request_cooldown_sec = Some(30);
        let handle = shared_retry_config_from(&config);

        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        manager.attach_retry_config(handle);

        manager.report_transient_failure(1, TransientFailureKind::OverageRequestLimit, None, false);

        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|x| x.id == 1).unwrap();
        // 30s + ±20% jitter → [24, 36]s
        assert!(
            (23..=36).contains(&e.cooldown_remaining_seconds),
            "运行时配置 30s 应生效（jitter 后 [24,36]），实际 {}s",
            e.cooldown_remaining_seconds
        );
    }

    #[test]
    fn test_report_transient_failure_sets_cooldown_and_counter() {
        let config = Config::default();
        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, false);

        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|x| x.id == 1).unwrap();

        assert_eq!(e.transient_failure_count, 1, "瞬态失败计数应 +1");
        assert_eq!(
            e.cooldown_reason.as_deref(),
            Some("rate_limit"),
            "cooldown_reason 应为 rate_limit"
        );
        // 默认 RateLimit cooldown 120s + ±20% jitter → [96, 144]s，允许 1s 低误差
        assert!(
            (95..=144).contains(&e.cooldown_remaining_seconds),
            "RateLimit 默认 cooldown 应在 [96,144] (120s±20% jitter)，实际 {}s",
            e.cooldown_remaining_seconds
        );
        // 不应禁用、不应改 failure_count
        assert!(!e.disabled, "瞬态失败不应禁用凭据");
        assert_eq!(e.failure_count, 0, "瞬态失败不应改 failure_count");
        assert_eq!(snap.available, 1, "瞬态失败不应减少 available");
    }

    #[test]
    fn test_suspicious_activity_cools_known_directory_peers() {
        let mut config = Config::default();
        config.suspicious_activity_cooldown_sec = Some(60);
        let creds = vec![
            KiroCredentials::default(),
            KiroCredentials::default(),
            KiroCredentials::default(),
        ];
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        // 先通过 fallback 路径学习 #2/#3 的 directory key，不刷新它们的 cooldown。
        manager.report_transient_failure_with_directory(
            2,
            TransientFailureKind::SuspiciousActivity,
            None,
            true,
            Some("d-shared"),
        );
        manager.report_transient_failure_with_directory(
            3,
            TransientFailureKind::SuspiciousActivity,
            None,
            true,
            Some("d-other"),
        );
        assert_eq!(manager.snapshot().entries[1].cooldown_remaining_seconds, 0);

        manager.report_transient_failure_with_directory(
            1,
            TransientFailureKind::SuspiciousActivity,
            None,
            false,
            Some("d-shared"),
        );

        let snap = manager.snapshot();
        let e1 = snap.entries.iter().find(|x| x.id == 1).unwrap();
        let e2 = snap.entries.iter().find(|x| x.id == 2).unwrap();
        let e3 = snap.entries.iter().find(|x| x.id == 3).unwrap();

        assert!(e1.cooldown_remaining_seconds > 0);
        assert!(
            e2.cooldown_remaining_seconds > 0,
            "同 directory 凭据应一起冷却"
        );
        assert_eq!(
            e2.transient_failure_count, 1,
            "peer 冷却不应重复增加瞬态失败计数"
        );
        assert_eq!(
            e3.cooldown_remaining_seconds, 0,
            "不同 directory 不应被冷却"
        );
        assert_eq!(e1.directory_key.as_deref(), Some("d-shared"));
        assert_eq!(e2.directory_key.as_deref(), Some("d-shared"));
        assert_eq!(e3.directory_key.as_deref(), Some("d-other"));
    }

    #[tokio::test]
    async fn test_cooldown_excludes_credential_from_acquire() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 让 #1 进入 cooldown
        manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, false);

        // 连续 5 次 acquire 都应只选 #2（绕过 cooldown 中的 #1）
        for i in 0..5 {
            let ctx = manager.acquire_context(None, None).await.unwrap();
            assert_eq!(
                ctx.id, 2,
                "第 {} 次 acquire 应选 #2，实际选了 #{}（cooldown 未生效）",
                i, ctx.id
            );
            manager.release_inflight(ctx.id);
        }
    }

    #[tokio::test]
    async fn test_all_in_cooldown_fallback_does_not_503() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 全员进入 cooldown
        manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, false);
        manager.report_transient_failure(2, TransientFailureKind::RateLimit, None, false);

        // fallback 应仍能挑出某个号，避免 503
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert!(
            ctx.id == 1 || ctx.id == 2,
            "全员 cooldown 时 fallback 应仍返回某个号，实际 #{}",
            ctx.id
        );
    }

    #[test]
    fn test_transient_failure_does_not_disable_after_many_calls() {
        let config = Config::default();
        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        for _ in 0..100 {
            manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, false);
        }

        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|x| x.id == 1).unwrap();
        assert!(!e.disabled, "100 次瞬态失败也不应导致禁用");
        assert_eq!(e.failure_count, 0, "瞬态失败不该累计 failure_count");
        assert_eq!(e.transient_failure_count, 100);
        assert_eq!(snap.available, 1, "available 应保持 1");
    }

    #[test]
    fn test_retry_after_overrides_default_cooldown() {
        let config = Config::default();
        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // UpstreamError 默认 10s，传入 retry_after=120s 应优先（首次调用，不存在缩短问题，retry_after 不加 jitter）
        manager.report_transient_failure(
            1,
            TransientFailureKind::UpstreamError,
            Some(StdDuration::from_secs(120)),
            false,
        );

        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|x| x.id == 1).unwrap();
        assert!(
            (119..=120).contains(&e.cooldown_remaining_seconds),
            "Retry-After=120 应优先于 5xx 默认 10s，实际 cooldown {}s",
            e.cooldown_remaining_seconds
        );
    }

    #[test]
    fn test_transient_cooldown_disabled_falls_back_to_release_inflight() {
        let mut config = Config::default();
        config.transient_cooldown_enabled = false;
        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, false);

        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|x| x.id == 1).unwrap();
        // toggle 关闭：cooldown 不生效、计数器也不动（仅释放 inflight）
        assert_eq!(
            e.cooldown_remaining_seconds, 0,
            "toggle 关闭时不应进入 cooldown"
        );
        assert_eq!(e.transient_failure_count, 0, "toggle 关闭时不应累计计数");
        assert!(e.cooldown_reason.is_none());
    }

    #[test]
    fn test_load_old_stats_without_new_fields_is_compatible() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("kiro-stats-compat-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("credentials.json");
        // 旧版 stats.json：只有 success_count 和 last_used_at，没有 transient_failure_count
        let stats_path = dir.join("kiro_stats.json");
        fs::write(
            &stats_path,
            r#"{"1":{"success_count":42,"last_used_at":"2026-05-18T10:00:00Z"}}"#,
        )
        .unwrap();

        let config = Config::default();
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        let manager =
            MultiTokenManager::new(config, vec![cred], None, Some(creds_path), true).unwrap();

        let snap = manager.snapshot();
        let e = snap.entries.iter().find(|x| x.id == 1).unwrap();
        assert_eq!(e.success_count, 42, "应从旧 stats.json 加载 success_count");
        assert_eq!(
            e.transient_failure_count, 0,
            "缺失新字段应 serde default = 0"
        );
        assert!(e.last_transient_failure_at.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_acquire_waits_when_all_in_cooldown_short() {
        // 全员 cooldown 但最早过期 ≤ effective_max_fallback_wait() (默认 30s) 时，
        // acquire_context 应等待到过期再返回非 fallback 的有效号。
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 用 retry_after=1s 让两个号都进 1s 短 cooldown（retry_after 不加 jitter）
        manager.report_transient_failure(
            1,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_secs(1)),
            false,
        );
        manager.report_transient_failure(
            2,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_secs(1)),
            false,
        );

        let t0 = std::time::Instant::now();
        let ctx = manager.acquire_context(None, None).await.unwrap();
        let elapsed = t0.elapsed();

        assert!(
            elapsed >= StdDuration::from_millis(900),
            "智能等待应等待 ~1s，实际仅等 {}ms",
            elapsed.as_millis()
        );
        assert!(
            elapsed <= StdDuration::from_millis(2500),
            "等待时间不应过长，实际 {}ms",
            elapsed.as_millis()
        );
        // 等待后应返回非 fallback（cooldown 已过期）
        assert!(
            !ctx.from_cooldown_fallback,
            "等待后选中的号 cooldown 已过期，不应再标记为 fallback"
        );
    }

    #[tokio::test]
    async fn test_acquire_falls_back_when_cooldown_too_long() {
        // 全员 cooldown 但最早过期 > effective_max_fallback_wait() (默认 30s) 时，
        // acquire_context 应直接 fallback，不等待。
        // 本测试手动调低 max_fallback_wait_secs 到3s，让 60s cooldown 远超上限。
        let mut config = Config::default();
        config.max_fallback_wait_secs = Some(3);
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 默认 60s cooldown（jitter 后 [48,72]s），远大于 3s 上限
        manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, false);
        manager.report_transient_failure(2, TransientFailureKind::RateLimit, None, false);

        let t0 = std::time::Instant::now();
        let ctx = manager.acquire_context(None, None).await.unwrap();
        let elapsed = t0.elapsed();

        assert!(
            elapsed < StdDuration::from_millis(500),
            "60s cooldown 远超 3s 上限，应立即 fallback 不等待，实际 {}ms",
            elapsed.as_millis()
        );
        assert!(ctx.from_cooldown_fallback, "应标记为 fallback 路径");
    }

    #[tokio::test]
    async fn test_acquire_waits_multiple_rounds_for_long_cooldown() {
        // 验证 D2：max_fallback_wait_attempts=3 时，单次 acquire 可以等多轮。
        // 构造场景：2 个号都进 1.2s 短 cooldown，但 max_fallback_wait_secs=2s，
        // 所以单轮够等。验证不仅等了一次，而且最终拿到非 fallback 号。
        let mut config = Config::default();
        config.max_fallback_wait_secs = Some(2);
        config.max_fallback_wait_attempts = Some(3);
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        manager.report_transient_failure(
            1,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_millis(1200)),
            false,
        );
        manager.report_transient_failure(
            2,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_millis(1200)),
            false,
        );

        let t0 = std::time::Instant::now();
        let ctx = manager.acquire_context(None, None).await.unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= StdDuration::from_millis(1100),
            "应等待 cooldown 过期（~1.2s）, 实际 {}ms",
            elapsed.as_millis()
        );
        assert!(
            !ctx.from_cooldown_fallback,
            "等过 cooldown 后应返回非 fallback 号"
        );
        assert!(
            ctx.waited_for_cooldown,
            "应标记 waited_for_cooldown=true（用于 metrics）"
        );
    }

    #[tokio::test]
    async fn test_select_prefers_credentials_with_oldest_or_no_transient_failure() {
        // D3：同优先级内 select 应偏好"最久没瞬态失败"的号
        // 测试 1：构造 A/B 都有 last_transient（A 较老、B 较新），验证选 A
        // 测试 2：构造 C 从未失败 + A/B 都失败过，验证选 C
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        let mut cred_a = KiroCredentials::default();
        cred_a.access_token = Some("ta".to_string());
        cred_a.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        cred_a.priority = 10;
        let mut cred_b = KiroCredentials::default();
        cred_b.access_token = Some("tb".to_string());
        cred_b.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        cred_b.priority = 10;
        let manager =
            MultiTokenManager::new(config.clone(), vec![cred_a, cred_b], None, None, false)
                .unwrap();
        // A 先失败（更老的 instant），B 后失败（更新的 instant）
        manager.report_transient_failure(
            1,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_millis(1)),
            false,
        );
        std::thread::sleep(StdDuration::from_millis(20));
        manager.report_transient_failure(
            2,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_millis(1)),
            false,
        );
        std::thread::sleep(StdDuration::from_millis(10)); // 让 cooldown 过期

        // balanced 模式预期：A 比 B 老 → 选 A
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(
            ctx.id, 1,
            "A 失败更早（last_transient_at_instant 更老），应被优先选中，实际选 #{}",
            ctx.id
        );
        manager.release_inflight(ctx.id);

        // 测试 2：加一个全新号 C（从未失败）→ C 应被选（None < Some(_)）
        let mut cred_c = KiroCredentials::default();
        cred_c.access_token = Some("tc".to_string());
        cred_c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        cred_c.priority = 10;
        let manager2 = MultiTokenManager::new(
            config,
            vec![
                {
                    let mut a = KiroCredentials::default();
                    a.access_token = Some("ta".to_string());
                    a.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                    a.priority = 10;
                    a
                },
                {
                    let mut b = KiroCredentials::default();
                    b.access_token = Some("tb".to_string());
                    b.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
                    b.priority = 10;
                    b
                },
                cred_c,
            ],
            None,
            None,
            false,
        )
        .unwrap();
        manager2.report_transient_failure(
            1,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_millis(1)),
            false,
        );
        manager2.report_transient_failure(
            2,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_millis(1)),
            false,
        );
        std::thread::sleep(StdDuration::from_millis(10));
        let ctx = manager2.acquire_context(None, None).await.unwrap();
        assert_eq!(
            ctx.id, 3,
            "C 从未失败（last_transient_at_instant=None），应最优先，实际选 #{}",
            ctx.id
        );
        manager2.release_inflight(ctx.id);
    }

    #[tokio::test]
    async fn test_priority_fastpath_skips_free_credential_for_opus() {
        // 回归测试：priority 模式下，即便 current_id 指向 FREE 号（不支持 opus），
        // 来 opus 请求时也不能命中它，必须跳到支持 opus 的号。
        // 防止 acquire_context 的 priority 快路径绕过 select_and_acquire_slot 的 opus 过滤。
        let config = Config::default(); // 默认 priority 模式

        // #1：FREE 号（current_id 默认指向第一个），不支持 opus
        let mut free = KiroCredentials::default();
        free.access_token = Some("free".to_string());
        free.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        free.subscription_title = Some("KIRO FREE".to_string());
        free.priority = 10;

        // #2：PRO 号，支持 opus
        let mut pro = KiroCredentials::default();
        pro.access_token = Some("pro".to_string());
        pro.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        pro.subscription_title = Some("KIRO PRO".to_string());
        pro.priority = 10;

        let manager = MultiTokenManager::new(config, vec![free, pro], None, None, false).unwrap();

        // opus 请求：必须选 #2（PRO），绝不能命中 #1（FREE）
        let ctx = manager
            .acquire_context(Some("claude-opus-4-7"), None)
            .await
            .unwrap();
        assert_eq!(
            ctx.id, 2,
            "opus 请求不应命中 FREE 号 #1，应跳到支持 opus 的 PRO 号 #2，实际选 #{}",
            ctx.id
        );
        manager.release_inflight(ctx.id);

        // 对照：用全新 manager（避免上面 opus 请求已把 current_id 移到 #2），
        // 非 opus 请求（sonnet）priority 模式应正常命中 current_id #1（FREE 也能跑 sonnet）
        let config2 = Config::default();
        let mut free2 = KiroCredentials::default();
        free2.access_token = Some("free".to_string());
        free2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        free2.subscription_title = Some("KIRO FREE".to_string());
        free2.priority = 10;
        let mut pro2 = KiroCredentials::default();
        pro2.access_token = Some("pro".to_string());
        pro2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        pro2.subscription_title = Some("KIRO PRO".to_string());
        pro2.priority = 10;
        let manager2 =
            MultiTokenManager::new(config2, vec![free2, pro2], None, None, false).unwrap();
        let ctx = manager2
            .acquire_context(Some("claude-sonnet-4-5"), None)
            .await
            .unwrap();
        assert_eq!(
            ctx.id, 1,
            "非 opus 请求 priority 模式应命中 current_id #1，实际选 #{}",
            ctx.id
        );
        manager2.release_inflight(ctx.id);
    }

    #[tokio::test]
    async fn test_acquire_falls_back_after_exhausting_wait_attempts() {
        // 验证 D2：max_fallback_wait_attempts=1 时，等过 1 轮如果仍不可用则走 fallback。
        // 构造：cooldown 单轮足够等，但用 attempts=1 限制只能等 1 次；
        // sleep 后 cooldown 已过期，所以应正常拿非 fallback——本测试主要验证参数生效。
        let mut config = Config::default();
        config.max_fallback_wait_secs = Some(2);
        config.max_fallback_wait_attempts = Some(1);
        let mut cred = KiroCredentials::default();
        cred.access_token = Some("t".to_string());
        cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        manager.report_transient_failure(
            1,
            TransientFailureKind::RateLimit,
            Some(StdDuration::from_millis(800)),
            false,
        );
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert!(ctx.waited_for_cooldown);
        assert!(!ctx.from_cooldown_fallback);
    }

    #[test]
    fn test_fallback_path_does_not_extend_cooldown() {
        // 验证 fallback 路径"借用"已 cooldown 号失败时不再延长 cooldown，
        // 防止全员 cooldown 时反复刷新同一组号导致永远没有号能恢复
        let config = Config::default();
        let cred = KiroCredentials::default();
        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 第一次正常 transient：进入 cooldown
        manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, false);
        let snap1 = manager.snapshot();
        let cd1 = snap1.entries[0].cooldown_remaining_seconds;
        assert!(cd1 > 30, "首次应进入正常 cooldown，实际 {}s", cd1);

        // 第二次 fallback path：cooldown 不应被延长（最多保持原值或自然衰减 1s）
        std::thread::sleep(std::time::Duration::from_millis(50));
        manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, true);
        let snap2 = manager.snapshot();
        let cd2 = snap2.entries[0].cooldown_remaining_seconds;
        assert!(
            cd2 <= cd1,
            "fallback 路径不应延长 cooldown：第一次 {}s -> 第二次 {}s（应只衰减不增加）",
            cd1,
            cd2
        );
        // 但计数器应该累加（仍然是观测信号）
        assert_eq!(snap2.entries[0].transient_failure_count, 2);
    }

    #[test]
    fn test_jitter_keeps_cooldown_within_bounds() {
        // 验证 jitter 落在 [80%, 120%] 区间内（多次采样验证不会越界）
        let config = Config::default();
        for _ in 0..50 {
            let cred = KiroCredentials::default();
            let manager =
                MultiTokenManager::new(config.clone(), vec![cred], None, None, false).unwrap();
            manager.report_transient_failure(1, TransientFailureKind::RateLimit, None, false);
            let cd = manager.snapshot().entries[0].cooldown_remaining_seconds;
            assert!(
                (95..=144).contains(&cd),
                "120s base ±20% jitter 应落在 [96,144]±1s，实际 {}s",
                cd
            );
        }
    }

    #[tokio::test]
    async fn test_balanced_tiebreaker_distributes_across_credentials() {
        // 验证 Bug D 修复：所有号 inflight=0、success_count 相同时，
        // 选号应分布而非永远选第一个（旧代码 min_by_key 平局总返回第一项）
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut creds = vec![];
        for _ in 0..4 {
            let mut c = KiroCredentials::default();
            c.access_token = Some("t".to_string());
            c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
            creds.push(c);
        }
        let manager = MultiTokenManager::new(config, creds, None, None, false).unwrap();

        let mut counts: HashMap<u64, u64> = HashMap::new();
        for _ in 0..200 {
            let ctx = manager.acquire_context(None, None).await.unwrap();
            *counts.entry(ctx.id).or_insert(0) += 1;
            // 立即释放，让 inflight 始终回到 0，强制走平局分支
            manager.release_inflight(ctx.id);
        }

        // 4 个号都应被选中至少一次（无随机时只会选 #1，c1 会是 200，其余 0）
        for id in 1..=4u64 {
            let c = counts.get(&id).copied().unwrap_or(0);
            assert!(
                c > 0,
                "凭据 #{} 应被选中至少一次（随机 tiebreaker 应让分布均衡），\
                 实际 0 次。完整分布 {:?}",
                id,
                counts
            );
        }
    }

    /// 构造一个带有效（未过期）access_token 的凭据，避免 try_ensure_token 走网络刷新。
    fn live_cred(token: &str) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.access_token = Some(token.to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        c
    }

    /// 回归（审计 P0）：per-credential 并发 permit 移入 CallContext 后，
    /// `max_inflight_per_credential` 必须被精确约束——同一凭据上同时存活的
    /// 持 permit 上下文数不得超过上限。
    ///
    /// 旧实现把 permit 存进共享 `CredentialEntry.concurrency_permit`，第二个请求
    /// 选中同一号时会覆盖并 drop 掉第一个 permit，导致上限被绕过。
    #[tokio::test]
    async fn permit_caps_concurrent_inflight_per_credential() {
        let mut config = Config::default();
        config.max_inflight_per_credential = Some(2);
        // 单凭据：所有请求都落到它身上，便于观察并发上限
        let manager =
            MultiTokenManager::new(config, vec![live_cred("t1")], None, None, false).unwrap();

        // 同时持有 3 个 ctx：前 2 个应拿到 permit，第 3 个应满载降级（permit=None）
        let c1 = manager.acquire_context(None, None).await.unwrap();
        let c2 = manager.acquire_context(None, None).await.unwrap();
        let c3 = manager.acquire_context(None, None).await.unwrap();

        let held = [&c1, &c2, &c3]
            .iter()
            .filter(|c| c.concurrency_permit.is_some())
            .count();
        assert_eq!(
            held, 2,
            "max_inflight_per_credential=2 时,同时存活的持 permit 上下文应恰为 2(第 3 个降级)"
        );

        // 释放一个持 permit 的 ctx 后,semaphore 腾出一个 slot,新请求应能再拿到 permit。
        // （drop CallContext 即归还其 permit，与 inflight 计数无关）
        drop(c1);
        let c4 = manager.acquire_context(None, None).await.unwrap();
        assert!(
            c4.concurrency_permit.is_some(),
            "释放一个 permit 后,新请求应能重新取得 permit"
        );
    }

    /// 未配置 max_inflight_per_credential 时不应有任何 permit（不限并发）。
    #[tokio::test]
    async fn no_permit_when_limit_unset() {
        let config = Config::default(); // max_inflight_per_credential = None
        let manager =
            MultiTokenManager::new(config, vec![live_cred("t1")], None, None, false).unwrap();
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert!(
            ctx.concurrency_permit.is_none(),
            "未配置并发上限时 permit 应为 None(不限并发)"
        );
    }
}
