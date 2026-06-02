//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::anthropic::prompt_cache::PromptCache;
use crate::kiro::metrics::MetricsRecorder;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::{Config, CredentialGroupConfig, UserPreset};
use crate::model::runtime::{SharedPromptConfig, SharedRetryConfig};

use super::error::AdminServiceError;
use super::metrics::{AdminMetricsResponse, PromptCacheStats, compute_admin_metrics};
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, CreateUserPresetRequest,
    CredentialGroupStatusItem, CredentialStatusItem, CredentialsStatusResponse,
    LoadBalancingModeResponse, PresetCatalogResponse, PresetContentResponse, PresetMetaResponse,
    PromptCacheConfigPayload, RetryConfigPayload, SetLoadBalancingModeRequest,
    SystemPromptConfigResponse, UpdateSystemPromptRequest, UpdateUserPresetRequest,
    UpsertCredentialGroupRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    /// 共享 Prompt 注入配置（与 Anthropic handler 同源，更新即时生效）
    prompt_config: SharedPromptConfig,
    /// 可写 Config 句柄（用于把 prompt 等运行时变更持久化回 config.json）
    config_writer: Arc<Mutex<Config>>,
    /// 进程级请求指标记录器（与 KiroProvider 共享同一实例）
    metrics: Arc<MetricsRecorder>,
    /// 运行时 retry 配置（与 token_manager 共享同一实例）
    retry_config: SharedRetryConfig,
    /// Prompt prefix 缓存（与 Anthropic handler 共享同一实例）
    prompt_cache: PromptCache,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
        prompt_config: SharedPromptConfig,
        config_writer: Arc<Mutex<Config>>,
        metrics: Arc<MetricsRecorder>,
        retry_config: SharedRetryConfig,
        prompt_cache: PromptCache,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            prompt_config,
            config_writer,
            metrics,
            retry_config,
            prompt_cache,
        }
    }

    /// 取 prompt cache 当前配置（含运行时统计）
    pub fn get_prompt_cache_config(&self) -> PromptCacheConfigPayload {
        let snap = self.prompt_cache.snapshot();
        PromptCacheConfigPayload {
            enabled: self.prompt_cache.is_enabled(),
            capacity: snap.capacity,
            ttl_secs: snap.ttl_secs,
            perceived_cache_hit_ratio: self.prompt_cache.perceived_ratio(),
            entries: Some(snap.entries),
            hit_total: Some(snap.hit_total),
            miss_total: Some(snap.miss_total),
            eviction_total: Some(snap.eviction_total),
            hit_rate_1m: Some(snap.last1m.hit_rate()),
            hit_rate_5m: Some(snap.last5m.hit_rate()),
            saved_input_tokens_5m: Some(snap.last5m.saved_input_tokens),
            reported_hit_rate_1m: Some(snap.last1m.reported_hit_rate()),
            reported_saved_input_tokens_5m: Some(snap.last5m.reported_saved_input_tokens),
        }
    }

    /// 更新 prompt cache 配置（运行时即时生效 + 持久化回 config.json）
    pub fn update_prompt_cache_config(
        &self,
        req: PromptCacheConfigPayload,
    ) -> Result<PromptCacheConfigPayload, AdminServiceError> {
        if !(1..=65536).contains(&req.capacity) {
            return Err(AdminServiceError::InvalidCredential(
                "promptCacheCapacity 必须在 [1, 65536] 范围内".to_string(),
            ));
        }
        if !(10..=86400).contains(&req.ttl_secs) {
            return Err(AdminServiceError::InvalidCredential(
                "promptCacheTtlSecs 必须在 [10, 86400] 秒范围内".to_string(),
            ));
        }
        if let Some(r) = req.perceived_cache_hit_ratio {
            if !(0.0..=0.95).contains(&r) {
                return Err(AdminServiceError::InvalidCredential(
                    "perceivedCacheHitRatio 必须在 [0.0, 0.95] 范围内".to_string(),
                ));
            }
        }

        // 1. 即时生效
        self.prompt_cache.set_enabled(req.enabled);
        self.prompt_cache.set_capacity(req.capacity);
        self.prompt_cache
            .set_ttl(std::time::Duration::from_secs(req.ttl_secs));
        self.prompt_cache
            .set_perceived_ratio(req.perceived_cache_hit_ratio);

        // 2. 持久化
        {
            let mut writer = self.config_writer.lock();
            writer.prompt_cache_enabled = Some(req.enabled);
            writer.prompt_cache_capacity = Some(req.capacity);
            writer.prompt_cache_ttl_secs = Some(req.ttl_secs);
            writer.perceived_cache_hit_ratio = req.perceived_cache_hit_ratio;
            if writer.config_path().is_some() {
                if let Err(e) = writer.save() {
                    tracing::warn!("prompt cache 配置已生效但写回 config.json 失败: {}", e);
                    return Err(AdminServiceError::InternalError(format!(
                        "运行时已更新，但持久化失败: {}",
                        e
                    )));
                }
            }
        }

        Ok(self.get_prompt_cache_config())
    }

    /// 清空 prompt cache（admin 调试用）
    pub fn clear_prompt_cache(&self) {
        self.prompt_cache.clear();
    }

    /// 计算并返回当前 Admin metrics 聚合视图
    pub fn get_metrics(&self) -> AdminMetricsResponse {
        let mut resp = compute_admin_metrics(&self.metrics, &self.token_manager);
        let snap = self.prompt_cache.snapshot();
        resp.prompt_cache = Some(PromptCacheStats {
            enabled: self.prompt_cache.is_enabled(),
            entries: snap.entries as u64,
            capacity: snap.capacity as u64,
            ttl_secs: snap.ttl_secs,
            hit_total: snap.hit_total,
            miss_total: snap.miss_total,
            eviction_total: snap.eviction_total,
            hit_rate_1m: snap.last1m.hit_rate(),
            hit_rate_5m: snap.last5m.hit_rate(),
            saved_input_tokens_5m: snap.last5m.saved_input_tokens,
            reported_hit_rate_1m: snap.last1m.reported_hit_rate(),
            reported_saved_input_tokens_5m: snap.last5m.reported_saved_input_tokens,
            perceived_cache_hit_ratio: self.prompt_cache.perceived_ratio(),
        });
        resp
    }

    /// 返回 Prometheus / OpenMetrics 文本格式的指标快照
    ///
    /// 输出已转义并按 alphabetical 顺序，便于 Prometheus parser 一次性 ingest。
    /// 关键 family：
    /// - `kiro_uptime_seconds`
    /// - `kiro_credentials_*`（active / cooling / disabled / total / cumulative counters）
    /// - `kiro_requests_total{window="1m|5m|1h", outcome="success|transient|error"}`
    /// - `kiro_latency_milliseconds{window=...,quantile="0.5|0.95|0.99"}`
    /// - `kiro_cooldown_*{window=...}`
    /// - `kiro_prompt_cache_*`
    /// - `kiro_requests_by_model_total{model="..."}` (1h 窗口)
    /// - `kiro_requests_by_credential_total{credential_id="..."}` (1h 窗口)
    pub fn get_metrics_prometheus(&self) -> String {
        let resp = self.get_metrics();
        crate::admin::metrics::render_prometheus(&resp)
    }

    /// 读取当前生效的 retry 配置
    pub fn get_retry_config(&self) -> RetryConfigPayload {
        let cfg = self.retry_config.read();
        RetryConfigPayload {
            rate_limit_cooldown_sec: cfg.rate_limit_cooldown_sec,
            upstream_error_cooldown_sec: cfg.upstream_error_cooldown_sec,
            overage_request_cooldown_sec: cfg.overage_request_cooldown_sec,
            transient_cooldown_enabled: cfg.transient_cooldown_enabled,
            max_fallback_wait_secs: cfg.max_fallback_wait_secs,
            max_fallback_wait_attempts: cfg.max_fallback_wait_attempts,
        }
    }

    /// 更新 retry 配置（运行时即时生效 + 写回 config.json）
    ///
    /// 校验：
    /// - `rateLimitCooldownSec` / `upstreamErrorCooldownSec` ∈ [1, 600] 秒
    /// - `overageRequestCooldownSec` ∈ [1, 7200] 秒（OVERAGE 窗口最长 2 小时）
    /// - `maxFallbackWaitSecs` ∈ [3, 120] 秒
    /// - `maxFallbackWaitAttempts` ∈ [1, 10]
    pub fn update_retry_config(
        &self,
        req: RetryConfigPayload,
    ) -> Result<RetryConfigPayload, AdminServiceError> {
        if let Some(secs) = req.rate_limit_cooldown_sec {
            if secs == 0 || secs > 600 {
                return Err(AdminServiceError::InvalidCredential(
                    "rateLimitCooldownSec 必须在 [1, 600] 秒范围内".to_string(),
                ));
            }
        }
        if let Some(secs) = req.upstream_error_cooldown_sec {
            if secs == 0 || secs > 600 {
                return Err(AdminServiceError::InvalidCredential(
                    "upstreamErrorCooldownSec 必须在 [1, 600] 秒范围内".to_string(),
                ));
            }
        }
        if let Some(secs) = req.overage_request_cooldown_sec {
            if secs == 0 || secs > 7200 {
                return Err(AdminServiceError::InvalidCredential(
                    "overageRequestCooldownSec 必须在 [1, 7200] 秒范围内".to_string(),
                ));
            }
        }
        if let Some(secs) = req.max_fallback_wait_secs {
            if !(3..=120).contains(&secs) {
                return Err(AdminServiceError::InvalidCredential(
                    "maxFallbackWaitSecs 必须在 [3, 120] 秒范围内".to_string(),
                ));
            }
        }
        if let Some(n) = req.max_fallback_wait_attempts {
            if !(1..=10).contains(&n) {
                return Err(AdminServiceError::InvalidCredential(
                    "maxFallbackWaitAttempts 必须在 [1, 10] 范围内".to_string(),
                ));
            }
        }

        // 1. 即时生效：写共享 RwLock
        {
            let mut w = self.retry_config.write();
            w.rate_limit_cooldown_sec = req.rate_limit_cooldown_sec;
            w.upstream_error_cooldown_sec = req.upstream_error_cooldown_sec;
            w.overage_request_cooldown_sec = req.overage_request_cooldown_sec;
            w.transient_cooldown_enabled = req.transient_cooldown_enabled;
            w.max_fallback_wait_secs = req.max_fallback_wait_secs;
            w.max_fallback_wait_attempts = req.max_fallback_wait_attempts;
        }

        // 2. 持久化回 config.json
        {
            let mut writer = self.config_writer.lock();
            writer.rate_limit_cooldown_sec = req.rate_limit_cooldown_sec;
            writer.upstream_error_cooldown_sec = req.upstream_error_cooldown_sec;
            writer.overage_request_cooldown_sec = req.overage_request_cooldown_sec;
            writer.transient_cooldown_enabled = req.transient_cooldown_enabled;
            writer.max_fallback_wait_secs = req.max_fallback_wait_secs;
            writer.max_fallback_wait_attempts = req.max_fallback_wait_attempts;
            if writer.config_path().is_some() {
                if let Err(e) = writer.save() {
                    tracing::warn!("retry 配置已生效但写回 config.json 失败: {}", e);
                    return Err(AdminServiceError::InternalError(format!(
                        "运行时已更新，但持久化失败: {}",
                        e
                    )));
                }
            }
        }

        Ok(self.get_retry_config())
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                group: entry.group,
                proxy_source: entry.proxy_source,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
                transient_failure_count: entry.transient_failure_count,
                last_transient_failure_at: entry.last_transient_failure_at,
                cooldown_remaining_seconds: entry.cooldown_remaining_seconds,
                cooldown_reason: entry.cooldown_reason,
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        let credential_groups = self
            .token_manager
            .list_groups()
            .iter()
            .map(Self::group_status_item)
            .collect();

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credential_groups,
            credentials,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 设置凭据分组
    pub fn set_group(&self, id: u64, group: Option<String>) -> Result<(), AdminServiceError> {
        if let Some(ref group) = group {
            self.validate_credential_group(group)?;
        }
        self.token_manager
            .set_group(id, group)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
        })
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }
        if let Some(ref group) = req.group {
            self.validate_credential_group(group)?;
        }

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            group: req.group,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 列出所有凭据分组（Admin API，仅元数据 + 是否带鉴权，绝不回传密码）
    pub fn list_credential_groups(&self) -> Vec<CredentialGroupStatusItem> {
        self.token_manager
            .list_groups()
            .iter()
            .map(Self::group_status_item)
            .collect()
    }

    /// 把内部 `CredentialGroupConfig` 投影成对外状态项（密码绝不回传，用户名可回传供编辑回显）。
    fn group_status_item(group: &CredentialGroupConfig) -> CredentialGroupStatusItem {
        let proxy_url = group
            .proxy_url
            .as_ref()
            .filter(|url| !url.eq_ignore_ascii_case("direct"))
            .cloned();
        CredentialGroupStatusItem {
            id: group.id.clone(),
            has_proxy: proxy_url.is_some(),
            has_username: group.proxy_username.is_some(),
            proxy_username: group.proxy_username.clone(),
            proxy_url,
        }
    }

    /// 新建/更新凭据分组（Admin API，按 id upsert）。
    ///
    /// 密码语义见 [`UpsertCredentialGroupRequest`]：缺省/null=保留原密码、
    /// 空串=清空、非空=更新。代理出口由 `proxyUrl` 的 scheme 决定（socks5/http）。
    pub fn upsert_credential_group(
        &self,
        req: UpsertCredentialGroupRequest,
    ) -> Result<(), AdminServiceError> {
        let id = req.id.trim().to_string();
        if id.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "分组 ID 不能为空".to_string(),
            ));
        }

        // 规整代理 URL：空串视为未配置（直连）；其余原样保留（含特殊值 "direct"）。
        let proxy_url = req
            .proxy_url
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty());

        // 用户名/密码用对称的「保留/清空/更新」语义，避免编辑分组时静默改写未提交字段：
        //   None       → 保留原值（前端没动该字段）
        //   Some("")   → 清空
        //   Some(非空) → 更新
        // 不对称会导致「只改 proxyUrl」时把用户名清掉却留着密码，使代理鉴权失效。
        let existing = self.token_manager.list_groups();
        let prev = existing.iter().find(|g| g.id == id);
        // 用户名 trim 后判空；密码不 trim（允许含前后空格的密码），仅空串视为清空。
        let proxy_username = resolve_group_field(
            req.proxy_username.map(|v| v.trim().to_string()),
            prev.and_then(|g| g.proxy_username.clone()),
        );
        let proxy_password = resolve_group_field(
            req.proxy_password,
            prev.and_then(|g| g.proxy_password.clone()),
        );

        let group = CredentialGroupConfig {
            id: id.clone(),
            proxy_url,
            proxy_username,
            proxy_password,
        };

        self.token_manager
            .upsert_group(group)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))
    }

    /// 删除凭据分组（Admin API）。返回被解除挂靠（回落本机直连）的凭据 ID 列表。
    pub fn delete_credential_group(&self, id: &str) -> Result<Vec<u64>, AdminServiceError> {
        self.token_manager
            .remove_group(id)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    // ============ 系统提示词配置 ============

    /// 读取当前生效的 system prompt 配置
    pub fn get_system_prompt(&self) -> SystemPromptConfigResponse {
        let cfg = self.prompt_config.read();
        snapshot_to_response(&cfg)
    }

    /// 更新 system prompt 配置（运行时即时生效 + 写回 config.json）
    ///
    /// 语义：所有字段为 `None` 表示保持现状。
    /// - `content == Some("")` 视为清空自定义文本
    /// - `enabled_presets == Some([])` 视为禁用全部 preset
    /// - 写入未知 preset id（既非内置也非用户定义）会被拒绝
    pub fn update_system_prompt(
        &self,
        req: UpdateSystemPromptRequest,
    ) -> Result<SystemPromptConfigResponse, AdminServiceError> {
        // 校验 preset id（提前失败，避免半成功状态）
        if let Some(ref ids) = req.enabled_presets {
            let user_presets = self.prompt_config.read().user_presets.clone();
            for id in ids {
                let in_builtin = crate::anthropic::prompt_presets::is_builtin(id);
                let in_user = user_presets.iter().any(|p| &p.id == id);
                if !in_builtin && !in_user {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "未知 preset id: {}",
                        id
                    )));
                }
            }
        }

        // 校验自定义 content 长度（防超大载荷写入 config.json）
        if let Some(ref content) = req.content {
            validate_preset_content(content)?;
        }

        // 1. 更新共享运行时配置，同步生成快照
        let snapshot = {
            let mut cfg = self.prompt_config.write();
            if let Some(enabled) = req.enabled {
                cfg.enabled = enabled;
            }
            if let Some(presets) = req.enabled_presets {
                cfg.enabled_presets = presets;
            }
            if let Some(content) = req.content {
                cfg.custom_content = if content.is_empty() {
                    None
                } else {
                    Some(content)
                };
            }
            if let Some(position) = req.position {
                cfg.position = position;
            }
            if let Some(strip) = req.strip_restrictions {
                cfg.strip_system_restrictions = strip;
            }
            cfg.clone()
        };

        // 2. 同步写回 Config 并持久化
        {
            let mut writer = self.config_writer.lock();
            writer.system_prompt_enabled = snapshot.enabled;
            writer.enabled_presets = snapshot.enabled_presets.clone();
            writer.system_prompt = snapshot.custom_content.clone();
            writer.strip_system_restrictions = snapshot.strip_system_restrictions;
            writer.system_prompt_position = snapshot.position;

            if writer.config_path().is_some() {
                if let Err(e) = writer.save() {
                    tracing::warn!("system prompt 配置已生效但写回 config.json 失败: {}", e);
                    return Err(AdminServiceError::InternalError(format!(
                        "运行时已更新，但持久化失败: {}",
                        e
                    )));
                }
            } else {
                tracing::warn!("Config 缺少 config_path，system prompt 更新仅在内存生效");
            }
        }

        Ok(snapshot_to_response(&snapshot))
    }

    /// 返回内置 preset 元数据清单（含 content）
    pub fn list_presets(&self) -> PresetCatalogResponse {
        let presets = crate::anthropic::prompt_presets::PRESETS
            .iter()
            .map(|p| PresetMetaResponse {
                id: p.id,
                name: p.name,
                description: p.description,
                length: p.content.chars().count(),
                content: p.content,
            })
            .collect();
        PresetCatalogResponse { presets }
    }

    /// 返回单个内置 preset 的完整内容
    pub fn get_preset_content(&self, id: &str) -> Result<PresetContentResponse, AdminServiceError> {
        crate::anthropic::prompt_presets::find(id)
            .map(|p| PresetContentResponse {
                id: p.id,
                name: p.name,
                content: p.content,
            })
            .ok_or_else(|| AdminServiceError::InvalidCredential(format!("未知 preset id: {}", id)))
    }

    // ============ 用户自定义预设 CRUD ============

    /// 添加用户预设
    pub fn add_user_preset(
        &self,
        req: CreateUserPresetRequest,
    ) -> Result<SystemPromptConfigResponse, AdminServiceError> {
        validate_user_preset_id(&req.id)?;
        if req.name.trim().is_empty() {
            return Err(AdminServiceError::InvalidCredential("name 不能为空".into()));
        }
        if req.content.trim().is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "content 不能为空".into(),
            ));
        }
        validate_preset_name(&req.name)?;
        validate_preset_content(&req.content)?;
        validate_preset_description(&req.description)?;

        // 不能与内置或现有 user preset id 冲突
        if crate::anthropic::prompt_presets::is_builtin(&req.id) {
            return Err(AdminServiceError::InvalidCredential(format!(
                "id 与内置预设冲突: {}",
                req.id
            )));
        }

        let snapshot = {
            let mut cfg = self.prompt_config.write();
            if cfg.user_presets.iter().any(|p| p.id == req.id) {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "用户预设 id 已存在: {}",
                    req.id
                )));
            }
            cfg.user_presets.push(UserPreset {
                id: req.id,
                name: req.name,
                description: req.description,
                content: req.content,
            });
            cfg.clone()
        };

        self.persist_after_user_preset_change(&snapshot)?;
        Ok(snapshot_to_response(&snapshot))
    }

    /// 编辑用户预设
    pub fn update_user_preset(
        &self,
        id: &str,
        req: UpdateUserPresetRequest,
    ) -> Result<SystemPromptConfigResponse, AdminServiceError> {
        // 校验 id（防控制字符 / 路径穿越字符进入日志）
        validate_user_preset_id(id)?;

        if let Some(ref name) = req.name {
            if name.trim().is_empty() {
                return Err(AdminServiceError::InvalidCredential("name 不能为空".into()));
            }
            validate_preset_name(name)?;
        }
        if let Some(ref content) = req.content {
            if content.trim().is_empty() {
                return Err(AdminServiceError::InvalidCredential(
                    "content 不能为空".into(),
                ));
            }
            validate_preset_content(content)?;
        }
        if let Some(ref description) = req.description {
            validate_preset_description(description)?;
        }

        let snapshot = {
            let mut cfg = self.prompt_config.write();
            let target = cfg.user_presets.iter_mut().find(|p| p.id == id);
            let target = match target {
                Some(t) => t,
                None => {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "用户预设不存在: {}",
                        id
                    )));
                }
            };
            if let Some(name) = req.name {
                target.name = name;
            }
            if let Some(description) = req.description {
                target.description = description;
            }
            if let Some(content) = req.content {
                target.content = content;
            }
            cfg.clone()
        };

        self.persist_after_user_preset_change(&snapshot)?;
        Ok(snapshot_to_response(&snapshot))
    }

    /// 删除用户预设（同时从 enabled_presets 中移除该 id）
    pub fn delete_user_preset(
        &self,
        id: &str,
    ) -> Result<SystemPromptConfigResponse, AdminServiceError> {
        // 校验 id（防控制字符 / 路径穿越字符进入日志）
        validate_user_preset_id(id)?;

        let snapshot = {
            let mut cfg = self.prompt_config.write();
            let before = cfg.user_presets.len();
            cfg.user_presets.retain(|p| p.id != id);
            if cfg.user_presets.len() == before {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "用户预设不存在: {}",
                    id
                )));
            }
            cfg.enabled_presets.retain(|e| e != id);
            cfg.clone()
        };

        self.persist_after_user_preset_change(&snapshot)?;
        Ok(snapshot_to_response(&snapshot))
    }

    /// 把 user_presets / enabled_presets 写回 Config 并落盘
    fn persist_after_user_preset_change(
        &self,
        snapshot: &crate::model::runtime::PromptRuntimeConfig,
    ) -> Result<(), AdminServiceError> {
        let mut writer = self.config_writer.lock();
        writer.user_presets = snapshot.user_presets.clone();
        writer.enabled_presets = snapshot.enabled_presets.clone();
        if writer.config_path().is_some() {
            if let Err(e) = writer.save() {
                tracing::warn!("用户预设已生效但写回 config.json 失败: {}", e);
                return Err(AdminServiceError::InternalError(format!(
                    "运行时已更新，但持久化失败: {}",
                    e
                )));
            }
        }
        Ok(())
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = crate::common::io::atomic_write_string(path, &json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    fn validate_credential_group(&self, group: &str) -> Result<(), AdminServiceError> {
        let group = group.trim();
        if group.is_empty() {
            return Ok(());
        }
        let groups = self.token_manager.list_groups();
        if groups.iter().any(|g| g.id == group) {
            return Ok(());
        }

        let mut known: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
        known.sort();
        Err(AdminServiceError::InvalidCredential(format!(
            "未知凭据分组 \"{}\"，已配置分组: {:?}",
            group, known
        )))
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        // 先脱敏：错误信息可能回显上游响应体（含 token/client_secret 片段）。
        // 脱敏只替换 JSON 字段值/Bearer，不影响下方中文关键词分类。
        let msg = crate::common::redact::redact_secret_text(&e.to_string());
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = crate::common::redact::redact_secret_text(&e.to_string());

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = crate::common::redact::redact_secret_text(&e.to_string());

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = crate::common::redact::redact_secret_text(&e.to_string());
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据")
        {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }
}

/// 从运行时快照构建 API 响应
fn snapshot_to_response(
    snap: &crate::model::runtime::PromptRuntimeConfig,
) -> SystemPromptConfigResponse {
    SystemPromptConfigResponse {
        enabled: snap.enabled,
        enabled_presets: snap.enabled_presets.clone(),
        user_presets: snap.user_presets.clone(),
        content: snap.custom_content.clone(),
        position: snap.position,
        strip_restrictions: snap.strip_system_restrictions,
    }
}

/// 解析分组 upsert 的「保留/清空/更新」字段语义（用户名 / 密码共用）。
///
/// - `incoming == None` → 保留 `prev`（前端没提交该字段）
/// - `incoming == Some("")` → 清空（返回 `None`）
/// - `incoming == Some(非空)` → 更新为新值
///
/// 注意调用方负责是否对 incoming 做 trim：用户名应先 trim 再传入，密码原样传入
/// （允许含前后空格）。
fn resolve_group_field(incoming: Option<String>, prev: Option<String>) -> Option<String> {
    match incoming {
        None => prev,
        Some(v) if v.is_empty() => None,
        Some(v) => Some(v),
    }
}

/// 校验用户预设 id 合法性
///
/// 规则：长度 1-32；仅允许 `[a-z0-9_-]`；不能以连字符开头/结尾
fn validate_user_preset_id(id: &str) -> Result<(), AdminServiceError> {
    if id.is_empty() || id.len() > 32 {
        return Err(AdminServiceError::InvalidCredential(
            "preset id 长度必须在 1-32".into(),
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(AdminServiceError::InvalidCredential(
            "preset id 仅允许小写字母/数字/下划线/短横线".into(),
        ));
    }
    if id.starts_with('-') || id.ends_with('-') {
        return Err(AdminServiceError::InvalidCredential(
            "preset id 不能以连字符开头或结尾".into(),
        ));
    }
    Ok(())
}

/// preset/system-prompt 的 name 字段最大字符数
const MAX_PRESET_NAME_CHARS: usize = 128;
/// preset/system-prompt 的 content 字段最大字符数
///
/// 这些内容会被持久化进 config.json，且每次请求都注入到 system prompt。
/// 上限防止超大载荷造成磁盘放大 / 配置文件膨胀 / 注入成本失控。
const MAX_PRESET_CONTENT_CHARS: usize = 64 * 1024;
/// preset description 字段最大字符数
const MAX_PRESET_DESC_CHARS: usize = 512;

/// 校验 name 长度（按字符数，避免多字节绕过）
fn validate_preset_name(name: &str) -> Result<(), AdminServiceError> {
    if name.chars().count() > MAX_PRESET_NAME_CHARS {
        return Err(AdminServiceError::InvalidCredential(format!(
            "name 长度不能超过 {} 字符",
            MAX_PRESET_NAME_CHARS
        )));
    }
    Ok(())
}

/// 校验 content 长度（按字符数）
fn validate_preset_content(content: &str) -> Result<(), AdminServiceError> {
    if content.chars().count() > MAX_PRESET_CONTENT_CHARS {
        return Err(AdminServiceError::InvalidCredential(format!(
            "content 长度不能超过 {} 字符",
            MAX_PRESET_CONTENT_CHARS
        )));
    }
    Ok(())
}

/// 校验 description 长度（按字符数）
fn validate_preset_description(desc: &str) -> Result<(), AdminServiceError> {
    if desc.chars().count() > MAX_PRESET_DESC_CHARS {
        return Err(AdminServiceError::InvalidCredential(format!(
            "description 长度不能超过 {} 字符",
            MAX_PRESET_DESC_CHARS
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分组字段 keep/clear/update 语义（修复：编辑分组静默清空用户名的 bug）
    #[test]
    fn resolve_group_field_keep_clear_update() {
        let prev = || Some("old".to_string());
        // None = 保留原值（前端没提交该字段）→ 这是修复点：编辑只改 URL 时用户名不丢
        assert_eq!(resolve_group_field(None, prev()), Some("old".to_string()));
        // Some("") = 清空
        assert_eq!(resolve_group_field(Some(String::new()), prev()), None);
        // Some(非空) = 更新
        assert_eq!(
            resolve_group_field(Some("new".to_string()), prev()),
            Some("new".to_string())
        );
        // 新建场景（prev=None）：None 保持 None，Some(值) 取新值
        assert_eq!(resolve_group_field(None, None), None);
        assert_eq!(
            resolve_group_field(Some("fresh".to_string()), None),
            Some("fresh".to_string())
        );
    }

    /// 合法 id 应通过
    #[test]
    fn validate_id_accepts_valid() {
        for id in [
            "a",
            "abc",
            "my_preset",
            "v2-config",
            "123",
            "a_1-b_2",
            "x".repeat(32).as_str(),
        ] {
            assert!(
                validate_user_preset_id(id).is_ok(),
                "应接受合法 id: {:?}",
                id
            );
        }
    }

    /// 大写字母拒绝
    #[test]
    fn validate_id_rejects_uppercase() {
        for id in ["Foo", "BAR", "myPreset", "MY_CONFIG"] {
            assert!(validate_user_preset_id(id).is_err(), "应拒绝大写: {:?}", id);
        }
    }

    /// 长度边界
    #[test]
    fn validate_id_rejects_length_violations() {
        // 空
        assert!(validate_user_preset_id("").is_err());
        // 33 字符（>32）
        let too_long = "a".repeat(33);
        assert!(validate_user_preset_id(&too_long).is_err());
        // 1024 字符
        let huge = "a".repeat(1024);
        assert!(validate_user_preset_id(&huge).is_err());
    }

    /// 连字符锚点
    #[test]
    fn validate_id_rejects_dash_anchor() {
        for id in ["-foo", "foo-", "-", "--", "-abc-"] {
            assert!(
                validate_user_preset_id(id).is_err(),
                "应拒绝以连字符开头/结尾: {:?}",
                id
            );
        }
    }

    /// name/content/description 长度上限
    #[test]
    fn validate_preset_field_length_limits() {
        // name：边界内通过，超界拒绝（按字符数，多字节也算 1 字符）
        assert!(validate_preset_name(&"a".repeat(MAX_PRESET_NAME_CHARS)).is_ok());
        assert!(validate_preset_name(&"a".repeat(MAX_PRESET_NAME_CHARS + 1)).is_err());
        assert!(validate_preset_name(&"中".repeat(MAX_PRESET_NAME_CHARS + 1)).is_err());

        // content：边界内通过，超界拒绝
        assert!(validate_preset_content(&"a".repeat(MAX_PRESET_CONTENT_CHARS)).is_ok());
        assert!(validate_preset_content(&"a".repeat(MAX_PRESET_CONTENT_CHARS + 1)).is_err());

        // description：边界内通过，超界拒绝；空串永远 ok
        assert!(validate_preset_description("").is_ok());
        assert!(validate_preset_description(&"a".repeat(MAX_PRESET_DESC_CHARS)).is_ok());
        assert!(validate_preset_description(&"a".repeat(MAX_PRESET_DESC_CHARS + 1)).is_err());
    }

    /// 路径穿越/控制字符（被 [a-z0-9_-] 白名单自动拦截）
    #[test]
    fn validate_id_rejects_path_traversal_and_control_chars() {
        for id in [
            "../config",
            "../../etc/passwd",
            "foo/bar",
            "foo\\bar",
            "foo bar",    // 空格
            "foo.bar",    // 点
            "foo:bar",    // 冒号
            "foo\0bar",   // null byte
            "foo\nbar",   // 换行
            "中文preset", // 非 ASCII
        ] {
            assert!(
                validate_user_preset_id(id).is_err(),
                "应拒绝特殊字符: {:?}",
                id
            );
        }
    }
}
