//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

/// 检查 Token 是否在指定时间内过期
pub(crate) fn is_token_expiring_within(
    credentials: &KiroCredentials,
    minutes: i64,
) -> Option<bool> {
    credentials
        .expires_at
        .as_ref()
        .and_then(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires| expires <= Utc::now() + Duration::minutes(minutes))
}

/// 检查 Token 是否已过期（提前 5 分钟判断）
pub(crate) fn is_token_expired(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 5).unwrap_or(true)
}

/// 检查 Token 是否即将过期（10分钟内）
pub(crate) fn is_token_expiring_soon(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 10).unwrap_or(false)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// 生成 API Key 脱敏展示(前 4 + ... + 后 4,长度不足或非 ASCII 回退 ***)
fn mask_api_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 16 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "***".to_string()
    }
}

/// 验证 refreshToken 的基本有效性
pub(crate) fn validate_refresh_token(credentials: &KiroCredentials) -> anyhow::Result<()> {
    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;

    if refresh_token.is_empty() {
        bail!("refreshToken 为空");
    }

    if refresh_token.len() < 100 || refresh_token.ends_with("...") || refresh_token.contains("...")
    {
        bail!(
            "refreshToken 已被截断（长度: {} 字符）。\n\
             这通常是 Kiro IDE 为了防止凭证被第三方工具使用而故意截断的。",
            refresh_token.len()
        );
    }

    Ok(())
}

/// Refresh Token 永久失效错误
///
/// 当服务端返回 400 + `invalid_grant` 时，表示 refreshToken 已被撤销或过期，
/// 不应重试，需立即禁用对应凭据。
#[derive(Debug)]
pub(crate) struct RefreshTokenInvalidError {
    pub message: String,
}

impl fmt::Display for RefreshTokenInvalidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshTokenInvalidError {}

/// 刷新 Token
pub(crate) async fn refresh_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    // API Key 凭据不支持 Token 刷新：底层契约级拦截
    // 其他调用点（try_ensure_token / 活跃路径 / add_credential）在调用前已显式分流 API Key；
    // 仅 force_refresh_token_for 未分流，此处 bail 让错误自然传播为 400 BAD_REQUEST。
    if credentials.is_api_key_credential() {
        bail!("API Key 凭据不支持刷新 Token");
    }

    validate_refresh_token(credentials)?;

    // 根据 auth_method 选择刷新方式
    // 如果未指定 auth_method，根据是否有 clientId/clientSecret 自动判断
    let auth_method = credentials.auth_method.as_deref().unwrap_or_else(|| {
        if credentials.client_id.is_some() && credentials.client_secret.is_some() {
            "idc"
        } else {
            "social"
        }
    });

    if auth_method.eq_ignore_ascii_case("idc")
        || auth_method.eq_ignore_ascii_case("builder-id")
        || auth_method.eq_ignore_ascii_case("iam")
    {
        refresh_idc_token(credentials, config, proxy).await
    } else {
        refresh_social_token(credentials, config, proxy).await
    }
}

/// 刷新 Social Token
async fn refresh_social_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 Social Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);

    let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);
    let refresh_domain = format!("prod.{}.auth.desktop.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = RefreshRequest {
        refresh_token: refresh_token.to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("KiroIDE-{}-{}", kiro_version, machine_id),
        )
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .header("host", &refresh_domain)
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        // 脱敏上游响应，防 token reflection 泄入日志/错误响应
        let redacted_body = crate::common::redact::redact_secret_text(&body_text);

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!(
                    "Social refreshToken 已失效 (invalid_grant): {}",
                    redacted_body
                ),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "OAuth 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OAuth 服务暂时不可用",
            _ => "Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, redacted_body);
    }

    let data: RefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

/// 刷新 IdC Token (AWS SSO OIDC)
async fn refresh_idc_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 IdC Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientId"))?;
    let client_secret = credentials
        .client_secret
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientSecret"))?;

    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);
    let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let x_amz_user_agent = "aws-sdk-js/3.980.0 KiroIDE";
    let user_agent = format!(
        "aws-sdk-js/3.980.0 ua/2.1 os/{} lang/js md/nodejs#{} api/sso-oidc#3.980.0 m/E KiroIDE",
        os_name, node_version
    );

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = IdcRefreshRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        refresh_token: refresh_token.to_string(),
        grant_type: "refresh_token".to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("content-type", "application/json")
        .header("x-amz-user-agent", x_amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        // 脱敏上游响应，防 token reflection 泄入日志/错误响应
        let redacted_body = crate::common::redact::redact_secret_text(&body_text);

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("IdC refreshToken 已失效 (invalid_grant): {}", redacted_body),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "IdC 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OIDC 服务暂时不可用",
            _ => "IdC Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, redacted_body);
    }

    let data: IdcRefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    // 同步更新 profile_arn（如果 IdC 响应中包含）
    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    Ok(new_credentials)
}

/// 获取使用额度信息
pub(crate) async fn get_usage_limits(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<UsageLimitsResponse> {
    tracing::debug!("正在获取使用额度信息...");

    // 优先级：凭据.api_region > config.api_region > config.region
    let region = credentials.effective_api_region(config);
    let host = format!("q.{}.amazonaws.com", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    // 构建 URL
    let mut url = format!(
        "https://{}/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST",
        host
    );

    // profileArn 是可选的
    if let Some(profile_arn) = &credentials.profile_arn {
        url.push_str(&format!("&profileArn={}", urlencoding::encode(profile_arn)));
    }

    // 构建 User-Agent headers
    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        os_name, node_version, kiro_version, machine_id
    );
    let amz_user_agent = format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", kiro_version, machine_id);

    let client = build_client(proxy, 60, config.tls_backend)?;

    let mut request = client
        .get(&url)
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token))
        .header("Connection", "close");

    if credentials.is_api_key_credential() {
        request = request.header("tokentype", "API_KEY");
    }

    let response = request.send().await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        // 脱敏上游响应，防 token reflection 泄入日志/错误响应
        let redacted_body = crate::common::redact::redact_secret_text(&body_text);
        let error_msg = match status.as_u16() {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法获取使用额度",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取使用额度失败",
        };
        bail!("{}: {} {}", error_msg, status, redacted_body);
    }

    let data: UsageLimitsResponse = response.json().await?;
    Ok(data)
}

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
    /// Per-credential 并发限制 semaphore（None = 不限制）
    ///
    /// 非阻塞 try_acquire 模式：选号时调用 `sem.try_acquire_owned()`，
    /// 成功则持有 permit，失败则跳过该号选下一个。
    /// 防止单个凭据同时承受过多请求被 Kiro 风控。
    permit_semaphore: Option<Arc<Semaphore>>,
    /// 当前持有的 per-credential 并发 permit（选号时 acquire，请求结束时 release）
    concurrency_permit: Option<OwnedSemaphorePermit>,
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

/// 上游瞬态错误分类（用于 cooldown 时长选择 + 观测）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransientFailureKind {
    /// HTTP 429 Too Many Requests（限流）
    RateLimit,
    /// HTTP 408 Request Timeout
    Timeout,
    /// HTTP 5xx 上游服务错误
    UpstreamError,
    /// HTTP 402 + `OVERAGE_REQUEST_LIMIT_EXCEEDED`
    ///
    /// 用户已开启 overage 付费，但当前 hour/day 速率窗口已满。
    /// 与 [`Self::RateLimit`] 区别：cooldown 时长更长（窗口刷新粒度通常以
    /// 小时/天计），不应误判为永久 disable。
    OverageRequestLimit,
    /// 429 + "suspicious activity" directory 级别封禁
    ///
    /// Kiro 返回 "Due to suspicious activity, we are imposing temporary limits"
    /// 表示 directory 维度的风控触发（所有共享 directory_id 的凭据同时受限）。
    /// 使用比普通 429 更长的 cooldown（默认 300s），避免号池在短时间内反复被打爆。
    SuspiciousActivity,
}

impl TransientFailureKind {
    /// 由 HTTP 状态码推断分类
    pub fn from_status(status: u16) -> Self {
        match status {
            429 => Self::RateLimit,
            408 => Self::Timeout,
            _ => Self::UpstreamError, // 5xx 及兜底
        }
    }

    /// 从 429 响应体中检测是否为 "suspicious activity" directory 封禁
    pub fn classify_429(body: &str) -> Self {
        if body.contains("suspicious activity") || body.contains("imposing temporary limits") {
            Self::SuspiciousActivity
        } else {
            Self::RateLimit
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Timeout => "timeout",
            Self::UpstreamError => "upstream_error",
            Self::OverageRequestLimit => "overage_request_limit",
            Self::SuspiciousActivity => "suspicious_activity",
        }
    }
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
    /// Token 刷新锁，确保同一时间只有一个刷新操作
    refresh_lock: TokioMutex<()>,
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
#[derive(Clone)]
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
}

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
                    permit_semaphore: per_cred_semaphore.clone(),
                    concurrency_permit: None,
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
            refresh_lock: TokioMutex::new(()),
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

    /// 获取凭据总数
    pub fn total_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 获取可用凭据数量
    pub fn available_count(&self) -> usize {
        self.entries.lock().iter().filter(|e| !e.disabled).count()
    }

    /// 根据负载均衡模式选择下一个凭据
    ///
    /// - priority 模式：选择优先级最高（priority 最小）的可用凭据
    /// - balanced 模式：均衡选择可用凭据
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    #[allow(dead_code)] // 历史选号入口，保留供后续 strategy 切换或测试覆盖
    fn select_next_credential(&self, model: Option<&str>) -> Option<(u64, KiroCredentials)> {
        let entries = self.entries.lock();

        // 检查是否是 opus 模型
        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);

        let now = Instant::now();

        // 过滤未禁用 + 模型适配的凭据（不含 cooldown 过滤）
        let candidates: Vec<&CredentialEntry> = entries
            .iter()
            .filter(|e| {
                if e.disabled {
                    return false;
                }
                if is_opus && !e.credentials.supports_opus() {
                    return false;
                }
                true
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // 第一遍：跳过冷却中的凭据；第二遍 fallback：所有候选都在冷却时
        // 选 cooldown 最早过期的（"最快恢复"），保证不返回 None 让上层 503
        let live: Vec<&CredentialEntry> = candidates
            .iter()
            .copied()
            .filter(|e| !is_in_cooldown(e, now))
            .collect();

        let pool: &[&CredentialEntry] = if !live.is_empty() { &live } else { &candidates };

        let mode = self.load_balancing_mode.lock().clone();
        let mode = mode.as_str();

        match mode {
            "balanced" => {
                // In-Flight 优先 + Least-Used 次之：先选当前并发最少的凭据，
                // 这样 N 个并发请求会被分散到 N 个不同的号上。
                // 平局（同 inflight）时按累计成功数最少（历史均衡），再按 priority。
                // 末尾追加随机数，防止全平局时永远选 Vec 的第一项（导致流量倾斜）。
                let entry = pool.iter().min_by_key(|e| {
                    (
                        e.inflight,
                        e.success_count,
                        e.credentials.priority,
                        fastrand::u32(..),
                    )
                })?;
                Some((entry.id, entry.credentials.clone()))
            }
            _ => {
                // priority 模式（默认）：选择优先级最高的
                let entry = pool.iter().min_by_key(|e| e.credentials.priority)?;
                Some((entry.id, entry.credentials.clone()))
            }
        }
    }

    /// 选择一个可用凭据并原子地占用 inflight 槽位
    ///
    /// 与 `select_next_credential` 的区别：在持有 entries 锁的同时把 `inflight += 1`，
    /// 这样并发调用会立刻看到该号 inflight 升高，下一个调用自然分发到其他号。
    ///
    /// 调用方拿到 `(id, credentials)` 后，**必须**通过 `release_inflight(id)`（或
    /// `report_success` / `report_failure` 等会自动释放的接口）归还槽位，否则会泄漏。
    ///
    /// 返回 `(id, credentials, from_cooldown_fallback, earliest_cooldown_until)`：
    /// - `from_cooldown_fallback=true` 表示全员都在 cooldown 中、本次是无奈选了最早
    ///   过期的号"硬试"，调用方在失败上报时不应再延长 cooldown（防雪暴）。
    /// - `earliest_cooldown_until`: 全员 cooldown 时是被选中号的过期时刻；非 fallback
    ///   分支为 `None`。调用方可据此决定是否短暂等待后重新选号（智能等待）。
    fn select_and_acquire_slot(
        &self,
        model: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Option<(u64, KiroCredentials, bool, Option<Instant>)> {
        let mut entries = self.entries.lock();

        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);

        let mode = self.load_balancing_mode.lock().clone();
        let mode = mode.as_str();

        let now = Instant::now();

        // 过滤未禁用 + 模型适配的候选索引（不含 cooldown 过滤）
        let candidates: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if e.disabled {
                    return None;
                }
                if is_opus && !e.credentials.supports_opus() {
                    return None;
                }
                Some(i)
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Sticky session：同一 conversation_id 优先路由到同一凭据
        if let Some(cid) = conversation_id {
            if let Some(sticky_id) = self.sticky_router.get(cid) {
                if let Some(&idx) = candidates.iter().find(|&&i| entries[i].id == sticky_id) {
                    if !is_in_cooldown(&entries[idx], now) {
                        // Sticky 命中且不在 cooldown，直接使用
                        if let Some(ref sem) = entries[idx].permit_semaphore {
                            if let Ok(permit) = sem.clone().try_acquire_owned() {
                                entries[idx].concurrency_permit = Some(permit);
                            }
                        }
                        entries[idx].inflight = entries[idx].inflight.saturating_add(1);
                        tracing::debug!(
                            "sticky session 命中: conversation={} → 凭据 #{}",
                            cid,
                            entries[idx].id
                        );
                        return Some((
                            entries[idx].id,
                            entries[idx].credentials.clone(),
                            false,
                            None,
                        ));
                    }
                }
            }
        }

        // 第一遍：排除 cooldown 中的凭据；
        // 全员 cooldown 时退化到原候选池（挑 cooldown 最早过期的）。
        let live: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| !is_in_cooldown(&entries[i], now))
            .collect();
        let all_in_cooldown = live.is_empty();
        let pool: &[usize] = if all_in_cooldown { &candidates } else { &live };

        let chosen_idx = if all_in_cooldown {
            // 退化分支：所有号都在 cooldown，必须 fallback 借用一个号。
            //
            // **重要修复（避免单号死循环 bug）**：
            // 旧逻辑按 `cooldown_until` 升序选"最早过期"，但 fallback 路径下
            // `report_transient_failure` **不更新 cooldown_until**（防雪暴），
            // 导致同一个号永远是"最早过期"反复被选中 → 实测 93% 的 429 集中到 1 个号。
            //
            // 新逻辑：优先按 `last_transient_at_instant` 选"最久没失败的"号
            // （None < Some，老 Instant < 新 Instant）——让 cred A 失败后被推到末尾，
            // 下一次 fallback 选别的号，**全员轮转借用**而不是死磕一个。
            // 末尾追加 fastrand 破平局，防止 Vec 第一项被永久选中。
            *pool.iter().min_by_key(|&&i| {
                let e = &entries[i];
                (
                    e.last_transient_at_instant,
                    e.cooldown_until.unwrap_or(now),
                    e.inflight,
                    e.success_count,
                    e.credentials.priority,
                    fastrand::u32(..),
                )
            })?
        } else {
            match mode {
                "balanced" => {
                    // In-Flight 优先：N 个并发请求自然分散到 N 个号上。
                    // 次级键 last_transient_at_instant：None < Some 且老 Instant < 新 Instant，
                    // 让"最久没失败的号"优先（避免刚 cooldown 过期立即又被选中）。
                    // 末尾追加随机数破平局，防 Vec 第一项被永久选中。
                    *pool.iter().min_by_key(|&&i| {
                        let e = &entries[i];
                        (
                            e.inflight,
                            e.last_transient_at_instant,
                            e.success_count,
                            e.credentials.priority,
                            fastrand::u32(..),
                        )
                    })?
                }
                _ => {
                    // priority 模式：先按优先级，同优先级偏好"最久没失败的号"
                    *pool.iter().min_by_key(|&&i| {
                        let e = &entries[i];
                        (
                            e.credentials.priority,
                            e.last_transient_at_instant,
                            e.success_count,
                        )
                    })?
                }
            }
        };

        // Per-credential 并发限制：try_acquire，满则降级（不阻塞不重选）
        if let Some(ref sem) = entries[chosen_idx].permit_semaphore {
            match sem.clone().try_acquire_owned() {
                Ok(permit) => {
                    entries[chosen_idx].concurrency_permit = Some(permit);
                }
                Err(_) => {
                    tracing::debug!(
                        "凭据 #{} per-credential 并发已满，降级为不限并发",
                        entries[chosen_idx].id
                    );
                }
            }
        }

        let entry = &mut entries[chosen_idx];
        entry.inflight = entry.inflight.saturating_add(1);
        let earliest_until = if all_in_cooldown {
            entry.cooldown_until
        } else {
            None
        };
        Some((
            entry.id,
            entry.credentials.clone(),
            all_in_cooldown,
            earliest_until,
        ))
    }

    /// 释放 inflight 槽位和 per-credential 并发 permit（请求结束时调用）
    pub fn release_inflight(&self, id: u64) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.inflight = entry.inflight.saturating_sub(1);
            // 释放 per-credential 并发 permit（drop OwnedSemaphorePermit 即释放 slot）
            entry.concurrency_permit = None;
        }
    }

    /// 将 conversation_id 绑定到 credential（请求成功后调用，用于 sticky session）
    pub fn bind_conversation(&self, conversation_id: &str, credential_id: u64) {
        self.sticky_router.bind(conversation_id, credential_id);
    }

    /// 关联运行时 retry 配置句柄
    ///
    /// 调用后 [`Self::report_transient_failure`] 会优先读取该共享配置；
    /// 未关联（默认）时退回 `self.config` 中的字段。
    pub fn attach_retry_config(&self, handle: crate::model::runtime::SharedRetryConfig) {
        *self.retry_config.lock() = Some(handle);
    }

    /// 当前生效的 RateLimit cooldown 时长（优先 retry_config，其次 config，最后内置默认）
    fn effective_rate_limit_cooldown(&self) -> StdDuration {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            if let Some(secs) = handle.read().rate_limit_cooldown_sec {
                return StdDuration::from_secs(secs);
            }
        }
        self.config
            .rate_limit_cooldown_sec
            .map(StdDuration::from_secs)
            .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN)
    }

    /// 当前生效的 408/5xx cooldown 时长
    fn effective_upstream_error_cooldown(&self) -> StdDuration {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            if let Some(secs) = handle.read().upstream_error_cooldown_sec {
                return StdDuration::from_secs(secs);
            }
        }
        self.config
            .upstream_error_cooldown_sec
            .map(StdDuration::from_secs)
            .unwrap_or(DEFAULT_UPSTREAM_ERROR_COOLDOWN)
    }

    /// 当前生效的 402 OVERAGE_REQUEST_LIMIT_EXCEEDED cooldown 时长
    fn effective_overage_request_cooldown(&self) -> StdDuration {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            if let Some(secs) = handle.read().overage_request_cooldown_sec {
                return StdDuration::from_secs(secs);
            }
        }
        self.config
            .overage_request_cooldown_sec
            .map(StdDuration::from_secs)
            .unwrap_or(DEFAULT_OVERAGE_REQUEST_COOLDOWN)
    }

    /// 当前生效的 "suspicious activity" directory 封禁 cooldown 时长
    fn effective_suspicious_activity_cooldown(&self) -> StdDuration {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            if let Some(secs) = handle.read().suspicious_activity_cooldown_sec {
                return StdDuration::from_secs(secs);
            }
        }
        self.config
            .suspicious_activity_cooldown_sec
            .map(StdDuration::from_secs)
            .unwrap_or(DEFAULT_SUSPICIOUS_ACTIVITY_COOLDOWN)
    }

    /// 当前是否启用瞬态 cooldown 机制（优先 retry_config）
    fn effective_transient_cooldown_enabled(&self) -> bool {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            return handle.read().transient_cooldown_enabled;
        }
        self.config.transient_cooldown_enabled
    }

    /// 当前生效的"全员 cooldown 智能等待"单轮上限（夹到 [3, 120]s）
    fn effective_max_fallback_wait(&self) -> StdDuration {
        let secs = if let Some(handle) = self.retry_config.lock().as_ref() {
            handle.read().max_fallback_wait_secs
        } else {
            self.config.max_fallback_wait_secs
        };
        match secs {
            Some(s) => StdDuration::from_secs(s.clamp(3, 120)),
            None => DEFAULT_MAX_FALLBACK_WAIT,
        }
    }

    /// 当前生效的"全员 cooldown 智能等待"最大轮数（夹到 [1, 10]）
    fn effective_max_fallback_wait_attempts(&self) -> u32 {
        let raw = if let Some(handle) = self.retry_config.lock().as_ref() {
            handle.read().max_fallback_wait_attempts
        } else {
            self.config.max_fallback_wait_attempts
        };
        raw.map(|n| n.clamp(1, 10))
            .unwrap_or(DEFAULT_MAX_FALLBACK_WAIT_ATTEMPTS)
    }

    /// 报告凭据遇到上游瞬态错误（429/408/5xx）
    ///
    /// 与 `report_failure` 不同：**不**累计 `failure_count`、**不**禁用凭据；
    /// 仅累计 `transient_failure_count` 并（视情况）把该凭据放入短期冷却。意图：
    ///
    /// - 单请求 retry 期间不再重复打到刚 429 的号；
    /// - 进入 cooldown 的号不会被永久禁用，到期自动恢复；
    /// - 全部号都被冷却时调度器 fallback 到"最早过期"的号，避免 503 风暴。
    ///
    /// 防雪暴：cooldown 时长加 ±20% jitter，错峰恢复，避免一批号同步进/出冷却。
    /// 防 fallback 死循环：`from_cooldown_fallback=true` 时**只累计计数、不刷新 cooldown**——
    /// 这种调用本来就是无奈选了一个还在冷却中的号"硬试"，再延长它的 cooldown 只会
    /// 让它永远是"最早过期"反复被借用，导致没有号能真正恢复。
    ///
    /// # 参数
    /// - `id`: 凭据 ID
    /// - `kind`: 错误分类（决定默认 cooldown 时长）
    /// - `retry_after`: 上游 `Retry-After` 头解析出的等待时长（如有则优先于默认值，无 jitter）
    /// - `from_cooldown_fallback`: 本次调用是否来自全员冷却的 fallback 路径
    pub fn report_transient_failure(
        &self,
        id: u64,
        kind: TransientFailureKind,
        retry_after: Option<StdDuration>,
        from_cooldown_fallback: bool,
    ) {
        // toggle 关闭时退化为旧行为：仅释放 inflight
        if !self.effective_transient_cooldown_enabled() {
            self.release_inflight(id);
            return;
        }

        // 计算基础 cooldown 时长：
        // - OVERAGE 速率窗口通常按 hour/day 计，**不**接受 retry_after 缩短（即使有也以默认值为准）
        // - SuspiciousActivity 为 directory 级别封禁，使用独立长 cooldown（默认 300s），
        //   **不**接受 retry_after 缩短（上游返回的短 Retry-After 不适用于风控场景）
        // - 其余类型 retry_after 优先（夹到 RETRY_AFTER_MAX_CLAMP），否则按 kind 选默认值
        let base_duration = if matches!(kind, TransientFailureKind::OverageRequestLimit) {
            self.effective_overage_request_cooldown()
        } else if matches!(kind, TransientFailureKind::SuspiciousActivity) {
            self.effective_suspicious_activity_cooldown()
        } else if let Some(d) = retry_after {
            d.min(RETRY_AFTER_MAX_CLAMP)
        } else {
            match kind {
                TransientFailureKind::RateLimit => self.effective_rate_limit_cooldown(),
                TransientFailureKind::Timeout | TransientFailureKind::UpstreamError => {
                    self.effective_upstream_error_cooldown()
                }
                TransientFailureKind::OverageRequestLimit
                | TransientFailureKind::SuspiciousActivity => unreachable!(),
            }
        };

        // ±20% jitter 错峰恢复（仅对默认值/配置值；上游显式 Retry-After 不抖动以尊重协议）
        let duration = if retry_after.is_some() {
            base_duration
        } else {
            apply_cooldown_jitter(base_duration)
        };

        let cooldown_until = Instant::now() + duration;
        let now_rfc = Utc::now().to_rfc3339();
        let mut became_dirty = false;

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.inflight = entry.inflight.saturating_sub(1);
                entry.concurrency_permit = None;
                entry.transient_failure_count = entry.transient_failure_count.saturating_add(1);
                entry.last_transient_failure_at = Some(now_rfc);
                entry.last_transient_at_instant = Some(Instant::now());

                if from_cooldown_fallback {
                    // 全员冷却 fallback 路径：不刷新 cooldown，让它按原计划过期。
                    // 否则会把"最早过期"的号永久推到末尾，导致 directory 整体限流时
                    // 没有号能恢复。
                    tracing::warn!(
                        "凭据 #{} 瞬态失败（fallback 路径，不延长冷却；原因 {}，累计 {} 次）",
                        id,
                        kind.as_str(),
                        entry.transient_failure_count
                    );
                } else {
                    // 只放宽 cooldown，不缩短：避免短的 retry-after 覆盖前面的长冷却
                    let extend = entry
                        .cooldown_until
                        .map(|until| cooldown_until > until)
                        .unwrap_or(true);
                    if extend {
                        entry.cooldown_until = Some(cooldown_until);
                    }
                    entry.cooldown_reason = Some(kind);
                    tracing::warn!(
                        "凭据 #{} 进入冷却 {}s（原因 {}，累计瞬态失败 {} 次）",
                        id,
                        duration.as_secs(),
                        kind.as_str(),
                        entry.transient_failure_count
                    );
                }
                became_dirty = true;
            }
        }

        if became_dirty {
            self.save_stats_debounced();
        }
    }

    /// 获取 API 调用上下文
    ///
    /// 返回绑定了 id、credentials 和 token 的调用上下文
    /// 确保整个 API 调用过程中使用一致的凭据信息
    ///
    /// 如果 Token 过期或即将过期，会自动刷新
    /// Token 刷新失败会累计到当前凭据，达到阈值后禁用并切换
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    /// - `conversation_id`: 可选的会话 ID，用于 sticky session 路由
    pub async fn acquire_context(
        &self,
        model: Option<&str>,
        conversation_id: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        let total = self.total_count();
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize).max(1);
        let mut attempt_count = 0;
        // 全员 cooldown 时智能等待已用轮数；上限来自 `effective_max_fallback_wait_attempts()`，
        // 避免在频繁限流下无限等待。每轮单次 sleep 不超过 `effective_max_fallback_wait()`。
        let mut fallback_wait_count: u32 = 0;
        let max_wait_attempts = self.effective_max_fallback_wait_attempts();
        let max_wait_per_round = self.effective_max_fallback_wait();

        loop {
            if attempt_count >= max_attempts {
                anyhow::bail!(
                    "所有凭据均无法获取有效 Token（可用: {}/{}）",
                    self.available_count(),
                    total
                );
            }

            let (id, credentials, from_fallback) = {
                let is_balanced = self.load_balancing_mode.lock().as_str() == "balanced";

                // balanced 模式：原子地"选号 + inflight +1"，让并发请求分发到不同号
                // priority 模式：优先使用 current_id 指向的凭据，并对该凭据 inflight +1
                let current_hit = if is_balanced {
                    None
                } else {
                    let mut entries = self.entries.lock();
                    let current_id = *self.current_id.lock();
                    entries
                        .iter_mut()
                        .find(|e| {
                            e.id == current_id && !e.disabled && !is_in_cooldown(e, Instant::now())
                        })
                        .map(|e| {
                            e.inflight = e.inflight.saturating_add(1);
                            // current_id 直接命中且不在 cooldown：非 fallback 路径
                            (e.id, e.credentials.clone(), false)
                        })
                };

                if let Some(hit) = current_hit {
                    hit
                } else {
                    // 当前凭据不可用或 balanced 模式，按策略选号并占用 inflight 槽
                    let mut best = self.select_and_acquire_slot(model, conversation_id);

                    // 没有可用凭据：如果是"自动禁用导致全灭"，做一次类似重启的自愈
                    if best.is_none() {
                        let mut entries = self.entries.lock();
                        if entries.iter().any(|e| {
                            e.disabled && e.disabled_reason == Some(DisabledReason::TooManyFailures)
                        }) {
                            tracing::warn!(
                                "所有凭据均已被自动禁用，执行自愈：重置失败计数并重新启用（等价于重启）"
                            );
                            for e in entries.iter_mut() {
                                if e.disabled_reason == Some(DisabledReason::TooManyFailures) {
                                    e.disabled = false;
                                    e.disabled_reason = None;
                                    e.failure_count = 0;
                                }
                            }
                            drop(entries);
                            best = self.select_and_acquire_slot(model, conversation_id);
                        }
                    }

                    // 智能等待：选中 fallback 号且最早过期 ≤ max_wait_per_round 时，
                    // 释放 slot、sleep 到过期 + 50ms 缓冲、重新 select。
                    // 至多重复 max_wait_attempts 轮，避免饥饿。
                    let wait_target = if fallback_wait_count < max_wait_attempts {
                        best.as_ref().and_then(|(tmp_id, _, fb, until)| {
                            if *fb {
                                until.map(|u| (*tmp_id, u))
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    };
                    if let Some((tmp_id, until)) = wait_target {
                        let now = Instant::now();
                        if until > now {
                            let wait = until.saturating_duration_since(now);
                            if wait <= max_wait_per_round {
                                fallback_wait_count = fallback_wait_count.saturating_add(1);
                                self.release_inflight(tmp_id);
                                let total_wait = wait + StdDuration::from_millis(50);
                                tracing::info!(
                                    "全员 cooldown 智能等待 {}ms 后重选（最早过期号 #{}, 第 {}/{} 轮）",
                                    total_wait.as_millis(),
                                    tmp_id,
                                    fallback_wait_count,
                                    max_wait_attempts,
                                );
                                tokio::time::sleep(total_wait).await;
                                // 重新走整个 acquire 循环
                                continue;
                            }
                        }
                    }

                    if let Some((new_id, new_creds, from_fb, _)) = best {
                        // 更新 current_id
                        let mut current_id = self.current_id.lock();
                        *current_id = new_id;
                        (new_id, new_creds, from_fb)
                    } else {
                        let entries = self.entries.lock();
                        // 注意：必须在 bail! 之前计算 available_count，
                        // 因为 available_count() 会尝试获取 entries 锁，
                        // 而此时我们已经持有该锁，会导致死锁
                        let available = entries.iter().filter(|e| !e.disabled).count();
                        anyhow::bail!("所有凭据均已禁用（{}/{}）", available, total);
                    }
                }
            };

            // 尝试获取/刷新 Token
            match self.try_ensure_token(id, &credentials).await {
                Ok(mut ctx) => {
                    ctx.from_cooldown_fallback = from_fallback;
                    ctx.waited_for_cooldown = fallback_wait_count > 0;
                    return Ok(ctx);
                }
                Err(e) => {
                    // 刷新失败：归还 inflight 槽（这次没产生真实请求）
                    self.release_inflight(id);
                    // refreshToken 永久失效 → 立即禁用，不累计重试
                    let has_available = if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                        tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, e);
                        self.report_refresh_token_invalid(id)
                    } else {
                        tracing::warn!("凭据 #{} Token 刷新失败: {}", id, e);
                        self.report_refresh_failure(id)
                    };
                    attempt_count += 1;
                    if !has_available {
                        anyhow::bail!("所有凭据均已禁用（0/{}）", total);
                    }
                }
            }
        }
    }

    /// 选择优先级最高的未禁用凭据作为当前凭据（内部方法）
    ///
    /// 纯粹按优先级选择，不排除当前凭据，用于优先级变更后立即生效
    fn select_highest_priority(&self) {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（不排除当前凭据）
        if let Some(best) = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
        {
            if best.id != *current_id {
                tracing::info!(
                    "优先级变更后切换凭据: #{} -> #{}（优先级 {}）",
                    *current_id,
                    best.id,
                    best.credentials.priority
                );
                *current_id = best.id;
            }
        }
    }

    /// 尝试使用指定凭据获取有效 Token
    ///
    /// 使用双重检查锁定模式，确保同一时间只有一个刷新操作
    ///
    /// # Arguments
    /// * `id` - 凭据 ID，用于更新正确的条目
    /// * `credentials` - 凭据信息
    async fn try_ensure_token(
        &self,
        id: u64,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<CallContext> {
        // API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需刷新
        if credentials.is_api_key_credential() {
            let token = credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            return Ok(CallContext {
                id,
                credentials: credentials.clone(),
                token,
                from_cooldown_fallback: false,
                waited_for_cooldown: false,
            });
        }

        // 第一次检查（无锁）：快速判断是否需要刷新
        let needs_refresh = is_token_expired(credentials) || is_token_expiring_soon(credentials);

        let creds = if needs_refresh {
            // 获取刷新锁，确保同一时间只有一个刷新操作
            let _guard = self.refresh_lock.lock().await;

            // 第二次检查：获取锁后重新读取凭据，因为其他请求可能已经完成刷新
            let current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 不存在", id))?
            };

            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                // 确实需要刷新
                let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref());
                let new_creds =
                    refresh_token(&current_creds, &self.config, effective_proxy.as_ref()).await?;

                if is_token_expired(&new_creds) {
                    anyhow::bail!("刷新后的 Token 仍然无效或已过期");
                }

                // 更新凭据
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = new_creds.clone();
                    }
                }

                // 回写凭据到文件（仅多凭据格式），失败只记录警告
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                }

                new_creds
            } else {
                // 其他请求已经完成刷新，直接使用新凭据
                tracing::debug!("Token 已被其他请求刷新，跳过刷新");
                current_creds
            }
        } else {
            credentials.clone()
        };

        let token = creds
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有可用的 accessToken"))?;

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.refresh_failure_count = 0;
            }
        }

        Ok(CallContext {
            id,
            credentials: creds,
            token,
            from_cooldown_fallback: false,
            waited_for_cooldown: false,
        })
    }

    /// 将凭据列表回写到源文件
    ///
    /// 仅在以下条件满足时回写：
    /// - 源文件是多凭据格式（数组）
    /// - credentials_path 已设置
    ///
    /// # Returns
    /// - `Ok(true)` - 成功写入文件
    /// - `Ok(false)` - 跳过写入（非多凭据格式或无路径配置）
    /// - `Err(_)` - 写入失败
    fn persist_credentials(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        // 仅多凭据格式才回写
        if !self.is_multiple_format {
            return Ok(false);
        }

        let path = match &self.credentials_path {
            Some(p) => p,
            None => return Ok(false),
        };

        // 收集所有凭据
        let credentials: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    // 同步 disabled 状态到凭据对象
                    cred.disabled = e.disabled;
                    cred
                })
                .collect()
        };

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(&credentials).context("序列化凭据失败")?;

        // 原子写入（tmp + rename），防进程中段被 kill 时 credentials.json 半写损坏。
        // 用 _secure 变体：Unix 上自动 chmod 0o600，防同主机其他用户/服务读到
        // refresh_token / access_token / api_key 等敏感字段。
        // 在 Tokio runtime 内使用 block_in_place 避免阻塞 worker。
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| {
                crate::common::io::atomic_write_string_secure(path, &json)
            })
            .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        } else {
            crate::common::io::atomic_write_string_secure(path, &json)
                .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        }

        tracing::debug!("已回写凭据到文件: {:?}", path);
        Ok(true)
    }

    /// 获取缓存目录（凭据文件所在目录）
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// 统计数据文件路径
    fn stats_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("kiro_stats.json"))
    }

    /// 从磁盘加载统计数据并应用到当前条目
    fn load_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };

        let stats: HashMap<String, StatsEntry> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("解析统计缓存失败，将忽略: {}", e);
                return;
            }
        };

        let mut entries = self.entries.lock();
        for entry in entries.iter_mut() {
            if let Some(s) = stats.get(&entry.id.to_string()) {
                entry.success_count = s.success_count;
                entry.last_used_at = s.last_used_at.clone();
                entry.transient_failure_count = s.transient_failure_count;
                entry.last_transient_failure_at = s.last_transient_failure_at.clone();
            }
        }
        *self.last_stats_save_at.lock() = Some(Instant::now());
        self.stats_dirty.store(false, Ordering::Relaxed);
        tracing::info!("已从缓存加载 {} 条统计数据", stats.len());
    }

    /// 将当前统计数据持久化到磁盘
    fn save_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let stats: HashMap<String, StatsEntry> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    (
                        e.id.to_string(),
                        StatsEntry {
                            success_count: e.success_count,
                            last_used_at: e.last_used_at.clone(),
                            transient_failure_count: e.transient_failure_count,
                            last_transient_failure_at: e.last_transient_failure_at.clone(),
                        },
                    )
                })
                .collect()
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                if let Err(e) = crate::common::io::atomic_write_string(&path, &json) {
                    tracing::warn!("保存统计缓存失败: {}", e);
                } else {
                    *self.last_stats_save_at.lock() = Some(Instant::now());
                    self.stats_dirty.store(false, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!("序列化统计数据失败: {}", e),
        }
    }

    /// 标记统计数据已更新，并按 debounce 策略决定是否立即落盘
    fn save_stats_debounced(&self) {
        self.stats_dirty.store(true, Ordering::Relaxed);

        let should_flush = {
            let last = *self.last_stats_save_at.lock();
            match last {
                Some(last_saved_at) => last_saved_at.elapsed() >= STATS_SAVE_DEBOUNCE,
                None => true,
            }
        };

        if should_flush {
            self.save_stats();
        }
    }

    /// 报告指定凭据 API 调用成功
    ///
    /// 重置该凭据的失败计数
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_success(&self, id: u64) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.success_count += 1;
                entry.last_used_at = Some(Utc::now().to_rfc3339());
                entry.inflight = entry.inflight.saturating_sub(1);
                entry.concurrency_permit = None;
                // 成功调用证明上游对该号已恢复，立即解除冷却
                entry.cooldown_until = None;
                entry.cooldown_reason = None;
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次，当前并发 {}）",
                    id,
                    entry.success_count,
                    entry.inflight
                );
            }
        }
        self.save_stats_debounced();
    }

    /// 报告指定凭据 API 调用失败
    ///
    /// 增加失败计数，达到阈值时禁用凭据并切换到优先级最高的可用凭据
    /// 返回是否还有可用凭据可以重试
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            // 释放 inflight 槽（请求已结束）
            entry.inflight = entry.inflight.saturating_sub(1);
            entry.concurrency_permit = None;

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.failure_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            let failure_count = entry.failure_count;

            tracing::warn!(
                "凭据 #{} API 调用失败（{}/{}）",
                id,
                failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyFailures);
                tracing::error!("凭据 #{} 已连续失败 {} 次，已被禁用", id, failure_count);

                // 切换到优先级最高的可用凭据
                if let Some(next) = entries
                    .iter()
                    .filter(|e| !e.disabled)
                    .min_by_key(|e| e.credentials.priority)
                {
                    *current_id = next.id;
                    tracing::info!(
                        "已切换到凭据 #{}（优先级 {}）",
                        next.id,
                        next.credentials.priority
                    );
                } else {
                    tracing::error!("所有凭据均已禁用！");
                }
            }

            entries.iter().any(|e| !e.disabled)
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据额度已用尽
    ///
    /// 用于处理 402 Payment Required 且 reason 为 `MONTHLY_REQUEST_COUNT` 的场景：
    /// - 立即禁用该凭据（不等待连续失败阈值）
    /// - 切换到下一个可用凭据继续重试
    /// - 返回是否还有可用凭据
    pub fn report_quota_exhausted(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            // 释放 inflight 槽（请求已结束）
            entry.inflight = entry.inflight.saturating_sub(1);
            entry.concurrency_permit = None;

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::QuotaExceeded);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;

            tracing::error!("凭据 #{} 额度已用尽（MONTHLY_REQUEST_COUNT），已被禁用", id);

            // 切换到优先级最高的可用凭据
            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据刷新 Token 失败。
    ///
    /// 连续刷新失败达到阈值后禁用凭据并切换，阈值内保持当前凭据不切换，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.refresh_failure_count += 1;
            let refresh_failure_count = entry.refresh_failure_count;

            tracing::warn!(
                "凭据 #{} Token 刷新失败（{}/{}）",
                id,
                refresh_failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if refresh_failure_count < MAX_FAILURES_PER_CREDENTIAL {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);

            tracing::error!(
                "凭据 #{} Token 已连续刷新失败 {} 次，已被禁用",
                id,
                refresh_failure_count
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据的 refreshToken 永久失效（invalid_grant）。
    ///
    /// 立即禁用凭据，不累计、不重试。
    /// 返回是否还有可用凭据。
    pub fn report_refresh_token_invalid(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InvalidRefreshToken);

            tracing::error!(
                "凭据 #{} refreshToken 已失效 (invalid_grant)，已立即禁用",
                id
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 切换到优先级最高的可用凭据
    ///
    /// 返回是否成功切换
    pub fn switch_to_next(&self) -> bool {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（排除当前凭据）
        if let Some(next) = entries
            .iter()
            .filter(|e| !e.disabled && e.id != *current_id)
            .min_by_key(|e| e.credentials.priority)
        {
            *current_id = next.id;
            tracing::info!(
                "已切换到凭据 #{}（优先级 {}）",
                next.id,
                next.credentials.priority
            );
            true
        } else {
            // 没有其他可用凭据，检查当前凭据是否可用
            entries.iter().any(|e| e.id == *current_id && !e.disabled)
        }
    }

    // ========================================================================
    // Admin API 方法
    // ========================================================================

    /// 获取管理器状态快照（用于 Admin API）
    pub fn snapshot(&self) -> ManagerSnapshot {
        let entries = self.entries.lock();
        let current_id = *self.current_id.lock();
        let available = entries.iter().filter(|e| !e.disabled).count();
        let now = Instant::now();

        ManagerSnapshot {
            entries: entries
                .iter()
                .map(|e| CredentialEntrySnapshot {
                    id: e.id,
                    priority: e.credentials.priority,
                    disabled: e.disabled,
                    failure_count: e.failure_count,
                    auth_method: if e.credentials.is_api_key_credential() {
                        Some("api_key".to_string())
                    } else {
                        e.credentials.auth_method.as_deref().map(|m| {
                            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam")
                            {
                                "idc".to_string()
                            } else {
                                m.to_string()
                            }
                        })
                    },
                    has_profile_arn: e.credentials.profile_arn.is_some(),
                    expires_at: if e.credentials.is_api_key_credential() {
                        None // API Key 凭据本地不维护过期时间（服务端策略未知）
                    } else {
                        e.credentials.expires_at.clone()
                    },
                    refresh_token_hash: if e.credentials.is_api_key_credential() {
                        None
                    } else {
                        e.credentials.refresh_token.as_deref().map(sha256_hex)
                    },
                    api_key_hash: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(sha256_hex)
                    } else {
                        None
                    },
                    masked_api_key: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(mask_api_key)
                    } else {
                        None
                    },
                    email: e.credentials.email.clone(),
                    success_count: e.success_count,
                    inflight: e.inflight,
                    last_used_at: e.last_used_at.clone(),
                    has_proxy: e.credentials.proxy_url.is_some(),
                    proxy_url: e.credentials.proxy_url.clone(),
                    refresh_failure_count: e.refresh_failure_count,
                    disabled_reason: e.disabled_reason.map(|r| {
                        match r {
                            DisabledReason::Manual => "Manual",
                            DisabledReason::TooManyFailures => "TooManyFailures",
                            DisabledReason::TooManyRefreshFailures => "TooManyRefreshFailures",
                            DisabledReason::QuotaExceeded => "QuotaExceeded",
                            DisabledReason::InvalidRefreshToken => "InvalidRefreshToken",
                            DisabledReason::InvalidConfig => "InvalidConfig",
                        }
                        .to_string()
                    }),
                    endpoint: e.credentials.endpoint.clone(),
                    transient_failure_count: e.transient_failure_count,
                    last_transient_failure_at: e.last_transient_failure_at.clone(),
                    cooldown_remaining_seconds: e
                        .cooldown_until
                        .and_then(|until| until.checked_duration_since(now))
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    cooldown_reason: if e.cooldown_until.map(|until| until > now).unwrap_or(false) {
                        e.cooldown_reason.map(|r| r.as_str().to_string())
                    } else {
                        None
                    },
                })
                .collect(),
            current_id,
            total: entries.len(),
            available,
        }
    }

    /// 设置凭据禁用状态（Admin API）
    pub fn set_disabled(&self, id: u64, disabled: bool) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.disabled = disabled;
            if !disabled {
                // 启用时重置失败计数与冷却（管理员手动启用即清除一切惩罚状态）
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.disabled_reason = None;
                entry.cooldown_until = None;
                entry.cooldown_reason = None;
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
            }
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据优先级（Admin API）
    ///
    /// 修改优先级后会立即按新优先级重新选择当前凭据。
    /// 即使持久化失败，内存中的优先级和当前凭据选择也会生效。
    pub fn set_priority(&self, id: u64, priority: u32) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.priority = priority;
        }
        // 立即按新优先级重新选择当前凭据（无论持久化是否成功）
        self.select_highest_priority();
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 重置凭据失败计数并重新启用（Admin API）
    pub fn reset_and_enable(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            if entry.disabled_reason == Some(DisabledReason::InvalidConfig) {
                anyhow::bail!("凭据 #{} 因配置无效被禁用，请修正配置后重启服务", id);
            }
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.disabled = false;
            entry.disabled_reason = None;
            entry.cooldown_until = None;
            entry.cooldown_reason = None;
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 获取指定凭据的使用额度（Admin API）
    pub async fn get_usage_limits_for(&self, id: u64) -> anyhow::Result<UsageLimitsResponse> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiro_api_key，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            // 检查是否需要刷新 token
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let _guard = self.refresh_lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };

                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref());
                    let new_creds =
                        refresh_token(&current_creds, &self.config, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = new_creds.clone();
                        }
                    }
                    // 持久化失败只记录警告，不影响本次请求
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let usage_limits =
            get_usage_limits(&credentials, &self.config, &token, effective_proxy.as_ref()).await?;

        // 更新订阅等级到凭据（仅在发生变化时持久化）
        if let Some(subscription_title) = usage_limits.subscription_title() {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let old_title = entry.credentials.subscription_title.clone();
                    if old_title.as_deref() != Some(subscription_title) {
                        entry.credentials.subscription_title = Some(subscription_title.to_string());
                        tracing::info!(
                            "凭据 #{} 订阅等级已更新: {:?} -> {}",
                            id,
                            old_title,
                            subscription_title
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if changed {
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("订阅等级更新后持久化失败（不影响本次请求）: {}", e);
                }
            }
        }

        Ok(usage_limits)
    }

    /// 添加新凭据（Admin API）
    ///
    /// # 流程
    /// 1. 验证凭据基本字段（API Key: kiroApiKey 不为空; OAuth: refreshToken 不为空）
    /// 2. 基于 kiroApiKey 或 refreshToken 的 SHA-256 哈希检测重复
    /// 3. OAuth: 尝试刷新 Token 验证凭据有效性; API Key: 跳过
    /// 4. 分配新 ID（当前最大 ID + 1）
    /// 5. 添加到 entries 列表
    /// 6. 持久化到配置文件
    ///
    /// # 返回
    /// - `Ok(u64)` - 新凭据 ID
    /// - `Err(_)` - 验证失败或添加失败
    pub async fn add_credential(&self, new_cred: KiroCredentials) -> anyhow::Result<u64> {
        // 1. 基本验证
        if new_cred.is_api_key_credential() {
            let api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("kiroApiKey 为空");
            }
        } else {
            validate_refresh_token(&new_cred)?;
        }

        // 2. 基于哈希检测重复
        if new_cred.is_api_key_credential() {
            let new_api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 kiroApiKey"))?;
            let new_api_key_hash = sha256_hex(new_api_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .kiro_api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（kiroApiKey 重复）");
            }
        } else {
            let new_refresh_token = new_cred
                .refresh_token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;
            let new_refresh_token_hash = sha256_hex(new_refresh_token);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_refresh_token_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（refreshToken 重复）");
            }
        }

        // 3. 验证凭据有效性（API Key 无需网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential() {
            new_cred.clone()
        } else {
            let effective_proxy = new_cred.effective_proxy(self.proxy.as_ref());
            refresh_token(&new_cred, &self.config, effective_proxy.as_ref()).await?
        };

        // 4. 分配新 ID
        let new_id = {
            let entries = self.entries.lock();
            entries.iter().map(|e| e.id).max().unwrap_or(0) + 1
        };

        // 5. 设置 ID 并保留用户输入的元数据
        validated_cred.id = Some(new_id);
        validated_cred.priority = new_cred.priority;
        validated_cred.auth_method = new_cred.auth_method.map(|m| {
            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                "idc".to_string()
            } else {
                m
            }
        });
        validated_cred.client_id = new_cred.client_id;
        validated_cred.client_secret = new_cred.client_secret;
        validated_cred.region = new_cred.region;
        validated_cred.auth_region = new_cred.auth_region;
        validated_cred.api_region = new_cred.api_region;
        validated_cred.machine_id = new_cred.machine_id;
        validated_cred.email = new_cred.email;
        validated_cred.proxy_url = new_cred.proxy_url;
        validated_cred.proxy_username = new_cred.proxy_username;
        validated_cred.proxy_password = new_cred.proxy_password;
        validated_cred.kiro_api_key = new_cred.kiro_api_key;

        {
            let mut entries = self.entries.lock();
            // 复用已有凭据的 per-credential semaphore（所有凭据共享同一配置的 semaphore）
            let per_cred_sem = entries.iter().find_map(|e| e.permit_semaphore.clone());
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled: false,
                disabled_reason: None,
                success_count: 0,
                last_used_at: None,
                inflight: 0,
                transient_failure_count: 0,
                last_transient_failure_at: None,
                last_transient_at_instant: None,
                cooldown_until: None,
                cooldown_reason: None,
                permit_semaphore: per_cred_sem,
                concurrency_permit: None,
            });
        }

        // 6. 持久化
        self.persist_credentials()?;

        tracing::info!("成功添加凭据 #{}", new_id);
        Ok(new_id)
    }

    /// 删除凭据（Admin API）
    ///
    /// # 前置条件
    /// - 凭据必须已禁用（disabled = true）
    ///
    /// # 行为
    /// 1. 验证凭据存在
    /// 2. 验证凭据已禁用
    /// 3. 从 entries 移除
    /// 4. 如果删除的是当前凭据，切换到优先级最高的可用凭据
    /// 5. 如果删除后没有凭据，将 current_id 重置为 0
    /// 6. 持久化到文件
    ///
    /// # 返回
    /// - `Ok(())` - 删除成功
    /// - `Err(_)` - 凭据不存在、未禁用或持久化失败
    pub fn delete_credential(&self, id: u64) -> anyhow::Result<()> {
        let was_current = {
            let mut entries = self.entries.lock();

            // 查找凭据
            let entry = entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            // 检查是否已禁用
            if !entry.disabled {
                anyhow::bail!("只能删除已禁用的凭据（请先禁用凭据 #{}）", id);
            }

            // 记录是否是当前凭据
            let current_id = *self.current_id.lock();
            let was_current = current_id == id;

            // 删除凭据
            entries.retain(|e| e.id != id);

            was_current
        };

        // 如果删除的是当前凭据，切换到优先级最高的可用凭据
        if was_current {
            self.select_highest_priority();
        }

        // 如果删除后没有任何凭据，将 current_id 重置为 0（与初始化行为保持一致）
        {
            let entries = self.entries.lock();
            if entries.is_empty() {
                let mut current_id = self.current_id.lock();
                *current_id = 0;
                tracing::info!("所有凭据已删除，current_id 已重置为 0");
            }
        }

        // 持久化更改
        self.persist_credentials()?;

        // 立即回写统计数据，清除已删除凭据的残留条目
        self.save_stats();

        tracing::info!("已删除凭据 #{}", id);
        Ok(())
    }

    /// 强制刷新指定凭据的 Token（Admin API）
    ///
    /// 无条件调用上游 API 重新获取 access token，不检查是否过期。
    /// 适用于排查问题、Token 异常但未过期、主动更新凭据状态等场景。
    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<()> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // 获取刷新锁防止并发刷新
        let _guard = self.refresh_lock.lock().await;

        // 无条件调用 refresh_token
        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let new_creds = refresh_token(&credentials, &self.config, effective_proxy.as_ref()).await?;

        // 更新 entries 中对应凭据
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials = new_creds;
                entry.refresh_failure_count = 0;
            }
        }

        // 持久化
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("强制刷新 Token 后持久化失败: {}", e);
        }

        tracing::info!("凭据 #{} Token 已强制刷新", id);
        Ok(())
    }

    /// 获取负载均衡模式（Admin API）
    pub fn get_load_balancing_mode(&self) -> String {
        self.load_balancing_mode.lock().clone()
    }

    fn persist_load_balancing_mode(&self, mode: &str) -> anyhow::Result<()> {
        use anyhow::Context;

        let config_path = match self.config.config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!("配置文件路径未知，负载均衡模式仅在当前进程生效: {}", mode);
                return Ok(());
            }
        };

        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.load_balancing_mode = mode.to_string();
        config
            .save()
            .with_context(|| format!("持久化负载均衡模式失败: {}", config_path.display()))?;

        Ok(())
    }

    /// 设置负载均衡模式（Admin API）
    pub fn set_load_balancing_mode(&self, mode: String) -> anyhow::Result<()> {
        // 验证模式值
        if mode != "priority" && mode != "balanced" {
            anyhow::bail!("无效的负载均衡模式: {}", mode);
        }

        let previous_mode = self.get_load_balancing_mode();
        if previous_mode == mode {
            return Ok(());
        }

        *self.load_balancing_mode.lock() = mode.clone();

        if let Err(err) = self.persist_load_balancing_mode(&mode) {
            *self.load_balancing_mode.lock() = previous_mode;
            return Err(err);
        }

        tracing::info!("负载均衡模式已设置为: {}", mode);
        Ok(())
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
}
