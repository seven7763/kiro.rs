//! Admin API 类型定义

use serde::{Deserialize, Serialize};

use crate::model::config::{SystemPromptPosition, UserPreset};

// ============ 凭据状态 ============

/// 所有凭据状态响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsStatusResponse {
    /// 凭据总数
    pub total: usize,
    /// 可用凭据数量（未禁用）
    pub available: usize,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 已配置的凭据分组列表
    pub credential_groups: Vec<CredentialGroupStatusItem>,
    /// 各凭据状态列表
    pub credentials: Vec<CredentialStatusItem>,
}

/// 凭据分组状态信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialGroupStatusItem {
    /// 分组 ID
    pub id: String,
    /// 分组代理 URL；None 表示该组直连
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// 是否配置为代理出口
    pub has_proxy: bool,
    /// 代理认证用户名（非密钥，可回传供前端编辑回显；None 表示未配置）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,
    /// 是否配置了代理认证用户名（用于前端展示「带鉴权」；密码绝不回传）
    pub has_username: bool,
}

/// 单个凭据的状态信息
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatusItem {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级（数字越小优先级越高）
    pub priority: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 是否为当前活跃凭据
    pub is_current: bool,
    /// Token 过期时间（RFC3339 格式）
    pub expires_at: Option<String>,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
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
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// 凭据分组 ID（如果配置）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 有效代理来源：credential / credential_direct / group / group_direct / global / none
    pub proxy_source: String,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 端点名称（决定该凭据走哪套 Kiro API，已回退到默认端点）
    pub endpoint: String,
    /// 上游瞬态错误（429/408/5xx）累计次数（不参与禁用判定，仅供观测）
    #[serde(default)]
    pub transient_failure_count: u64,
    /// 最近一次瞬态错误时间（RFC3339 格式）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_transient_failure_at: Option<String>,
    /// 当前冷却剩余秒数（0 表示不在冷却中）
    #[serde(default)]
    pub cooldown_remaining_seconds: u64,
    /// 当前冷却原因（"rate_limit" / "timeout" / "upstream_error"，不在冷却时为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
}

// ============ 操作请求 ============

/// 启用/禁用凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDisabledRequest {
    /// 是否禁用
    pub disabled: bool,
}

/// 修改优先级请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPriorityRequest {
    /// 新优先级值
    pub priority: u32,
}

/// 修改凭据分组请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCredentialGroupRequest {
    /// 新分组 ID；null/空字符串表示移出分组
    pub group: Option<String>,
}

/// 新建/更新凭据分组请求（按 `id` upsert）
///
/// `proxy_username` / `proxy_password` 用对称的「保留/清空/更新」语义
/// （避免编辑分组时静默改写未提交字段）：
/// - 缺省 / `null` → 保留原值（新建分组则视为无）
/// - `Some("")` → 清空
/// - `Some(非空)` → 更新
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertCredentialGroupRequest {
    /// 分组 ID（必填，作为 upsert 主键）
    pub id: String,
    /// 分组代理 URL；省略 / `null` / `"direct"` 表示该组直连。
    /// SOCKS5 由 URL scheme（`socks5://host:port`）决定，无需额外字段。
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// 代理认证用户名（可选，语义见结构体文档）
    #[serde(default)]
    pub proxy_username: Option<String>,
    /// 代理认证密码（可选，语义见结构体文档）
    #[serde(default)]
    pub proxy_password: Option<String>,
}

/// 添加凭据请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialRequest {
    /// 刷新令牌（OAuth 凭据必填，API Key 凭据不需要）
    pub refresh_token: Option<String>,

    /// 认证方式（可选，默认 social）
    #[serde(default = "default_auth_method")]
    pub auth_method: String,

    /// OIDC Client ID（IdC 认证需要）
    pub client_id: Option<String>,

    /// OIDC Client Secret（IdC 认证需要）
    pub client_secret: Option<String>,

    /// 优先级（可选，默认 0）
    #[serde(default)]
    pub priority: u32,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    pub api_region: Option<String>,

    /// 凭据级 Machine ID（可选，64 位字符串）
    /// 未配置时回退到 config.json 的 machineId
    pub machine_id: Option<String>,

    /// 用户邮箱（可选，用于前端显示）
    pub email: Option<String>,

    /// 凭据级代理 URL（可选，特殊值 "direct" 表示不使用代理）
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    pub proxy_password: Option<String>,

    /// 凭据分组 ID（可选）
    pub group: Option<String>,

    /// Kiro API Key（API Key 凭据必填，格式: ksk_xxxxxxxx）
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 端点名称（可选，未配置时使用 config.defaultEndpoint）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// GET/PUT /api/admin/runtime/prompt-cache-config 的请求/响应
///
/// 中转层 prompt prefix 缓存的运行时配置 + 监控统计。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCacheConfigPayload {
    /// 是否启用
    pub enabled: bool,
    /// LRU 容量（条目数上限），范围 [1, 65536]
    pub capacity: usize,
    /// 单条 entry TTL（秒），范围 [10, 86400]，默认 300（5min）
    pub ttl_secs: u64,
    /// 上报命中率系数（运营/计费口径），范围 [0.0, 0.95]；null/省略=不干预。
    /// 设 0.9 时把对外上报的 cache_read 固定到客户端可见 input 的 90%，creation 置 0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perceived_cache_hit_ratio: Option<f64>,
    /// 当前 cache 中条目数（只读，PUT 时忽略）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<usize>,
    /// 自启动累计命中次数（只读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_total: Option<u64>,
    /// 自启动累计未命中次数（只读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub miss_total: Option<u64>,
    /// 累计淘汰条目数（只读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eviction_total: Option<u64>,
    /// 1 分钟窗口命中率（百分比，只读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_rate_1m: Option<f64>,
    /// 5 分钟窗口命中率（只读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_rate_5m: Option<f64>,
    /// 5 分钟内累计节省 input tokens（只读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_input_tokens_5m: Option<i64>,
    /// 1 分钟窗口上报/计费口径命中率（百分比，只读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_hit_rate_1m: Option<f64>,
    /// 5 分钟内上报/计费口径节省 input tokens（只读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_saved_input_tokens_5m: Option<i64>,
}

/// GET/PUT /api/admin/runtime/retry-config 的请求/响应
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryConfigPayload {
    /// 429 限流默认 cooldown 时长（秒）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_cooldown_sec: Option<u64>,
    /// 408/5xx 默认 cooldown 时长（秒）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_error_cooldown_sec: Option<u64>,
    /// 402 OVERAGE_REQUEST_LIMIT_EXCEEDED 默认 cooldown 时长（秒），范围 [1, 7200]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_request_cooldown_sec: Option<u64>,
    /// 是否启用瞬态 cooldown 机制
    pub transient_cooldown_enabled: bool,
    /// 全员 cooldown 时智能等待的单轮上限（秒），范围 [3, 120]，默认 30
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fallback_wait_secs: Option<u64>,
    /// 单次 acquire 内"等待+重选"的最大轮数，范围 [1, 10]，默认 3
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fallback_wait_attempts: Option<u32>,
}

fn default_auth_method() -> String {
    "social".to_string()
}

/// 添加凭据成功响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCredentialResponse {
    pub success: bool,
    pub message: String,
    /// 新添加的凭据 ID
    pub credential_id: u64,
    /// 用户邮箱（如果获取成功）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

// ============ 余额查询 ============

/// 余额查询响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    /// 凭据 ID
    pub id: u64,
    /// 订阅类型
    pub subscription_title: Option<String>,
    /// 当前使用量
    pub current_usage: f64,
    /// 使用限额
    pub usage_limit: f64,
    /// 剩余额度
    pub remaining: f64,
    /// 使用百分比
    pub usage_percentage: f64,
    /// 下次重置时间（Unix 时间戳）
    pub next_reset_at: Option<f64>,
}

// ============ 负载均衡配置 ============

/// 负载均衡模式响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancingModeResponse {
    /// 当前模式（"priority" 或 "balanced"）
    pub mode: String,
}

/// 设置负载均衡模式请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLoadBalancingModeRequest {
    /// 模式（"priority" 或 "balanced"）
    pub mode: String,
}

// ============ 系统提示词配置 ============

/// 系统提示词配置响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemPromptConfigResponse {
    /// 注入总开关（关闭时所有 preset + 自定义文本都不注入）
    pub enabled: bool,
    /// 启用的 preset id 列表（混合内置 + 用户自定义）
    pub enabled_presets: Vec<String>,
    /// 用户自定义预设清单
    pub user_presets: Vec<UserPreset>,
    /// 自定义补充文本（None 表示未配置）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 注入位置：`prepend` / `append`
    pub position: SystemPromptPosition,
    /// 是否同时剥离客户端发来的安全限制指令（独立开关）
    pub strip_restrictions: bool,
}

/// 设置系统提示词配置请求
///
/// 所有字段都是可选的，未提供的字段保持现状。
/// - `content == Some("")` 视为清空自定义文本
/// - `enabled_presets == Some([])` 视为禁用全部 preset
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSystemPromptRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub enabled_presets: Option<Vec<String>>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub position: Option<SystemPromptPosition>,
    #[serde(default)]
    pub strip_restrictions: Option<bool>,
}

/// 单个内置 preset 的元数据 + 完整内容（前端用来本地拼接预览）
///
/// 由于内置只有 5 条、总大小约 5KB，直接返回 content 不会显著增加流量。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetMetaResponse {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// 字符数（前端展示用）
    pub length: usize,
    /// 完整 prompt 内容
    pub content: &'static str,
}

/// 预设清单响应
#[derive(Debug, Serialize)]
pub struct PresetCatalogResponse {
    pub presets: Vec<PresetMetaResponse>,
}

/// 单个 preset 的完整内容响应（保留作为按 id 单独读取的能力）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetContentResponse {
    pub id: &'static str,
    pub name: &'static str,
    pub content: &'static str,
}

/// 创建用户自定义预设请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUserPresetRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub content: String,
}

/// 更新用户自定义预设请求（id 由 URL path 指定，所有字段可选）
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateUserPresetRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub content: Option<String>,
}

// ============ 通用响应 ============

/// 操作成功响应
#[derive(Debug, Serialize)]
pub struct SuccessResponse {
    pub success: bool,
    pub message: String,
}

impl SuccessResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }
}

/// 错误响应
#[derive(Debug, Serialize)]
pub struct AdminErrorResponse {
    pub error: AdminError,
}

#[derive(Debug, Serialize)]
pub struct AdminError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl AdminErrorResponse {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: AdminError {
                error_type: error_type.into(),
                message: message.into(),
            },
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }

    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid or missing admin API key")
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn api_error(message: impl Into<String>) -> Self {
        Self::new("api_error", message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
}
