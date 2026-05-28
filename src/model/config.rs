use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    #[default]
    Rustls,
    NativeTls,
}

/// 自定义系统提示词注入位置
///
/// - `Prepend`：插入到 system 数组最前（旧默认行为）
/// - `Append`：追加到 system 数组末尾（recency bias 权重最高，推荐用于 override）
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SystemPromptPosition {
    Prepend,
    #[default]
    Append,
}

/// 用户自定义预设（与内置 `PRESETS` 并列，可在 Admin UI 中增删改）
///
/// id 必须全局唯一（含内置 id），仅允许 `[a-z0-9_-]`，长度 1-32。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserPreset {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub content: String,
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 是否启用瞬态错误（429/408/5xx）的 per-credential 短期冷却（默认 true）
    ///
    /// 启用后：单个凭据收到 429/5xx 时进入 cooldown，后续 acquire 自动绕过该凭据，
    /// 避免单请求 9 次 retry 全打到同一个被限号上。设 false 退回旧行为（仅
    /// release_inflight，不影响选号）。
    #[serde(default = "default_transient_cooldown_enabled")]
    pub transient_cooldown_enabled: bool,

    /// 429 限流后的 cooldown 秒数（默认 60，HTTP Retry-After 头优先）
    #[serde(default)]
    pub rate_limit_cooldown_sec: Option<u64>,

    /// 408/5xx 上游错误的 cooldown 秒数（默认 10）
    #[serde(default)]
    pub upstream_error_cooldown_sec: Option<u64>,

    /// 402 OVERAGE_REQUEST_LIMIT_EXCEEDED 的 cooldown 秒数（默认 600 = 10 分钟）
    ///
    /// 已开启 overage 付费但当前 hour/day 速率窗口已满时使用。
    /// 不**等同**于 MONTHLY_REQUEST_COUNT（后者会永久禁用凭据）。
    #[serde(default)]
    pub overage_request_cooldown_sec: Option<u64>,

    /// "suspicious activity" directory 级别封禁的 cooldown 秒数（默认 300 = 5 分钟）
    ///
    /// Kiro 返回 "Due to suspicious activity, we are imposing temporary limits"
    /// 时使用。表示 directory 维度的风控触发（所有共享 directory_id 的凭据同时受限）。
    #[serde(default)]
    pub suspicious_activity_cooldown_sec: Option<u64>,

    /// 全员 cooldown 时，acquire_context 等待最早过期号的最大秒数（默认 30）
    ///
    /// 让"上游全部限流"期间通过等待恢复给客户端 200，而不是 502。范围 [3, 120]。
    /// 单位秒，None 沿用代码内置默认（30s）。
    #[serde(default)]
    pub max_fallback_wait_secs: Option<u64>,

    /// 同一次 acquire 内允许"等待 cooldown 过期再重选"的最大轮数（默认 3）
    ///
    /// 范围 [1, 10]。等够这么多轮还选不到非 fallback 号才走 fallback 借号。
    #[serde(default)]
    pub max_fallback_wait_attempts: Option<u32>,

    /// 同时打到 Kiro 上游的最大并发请求数（默认无限制 = None）
    ///
    /// 用于避免"复试风暴"加重 Kiro 对账号的 suspicious activity 风控。
    /// 触发时新请求会在 `acquire_context` **之前**排队等待 slot，
    /// 一个客户端请求只占一个 slot（不管它内部重试多少次）。
    ///
    /// 建议值：号池大小 × 2~3。默认不限制，沿用旧行为。
    #[serde(default)]
    pub max_inflight_kiro_requests: Option<u32>,

    /// 每个凭据的最大并发请求数（默认 2）
    ///
    /// 非阻塞模式：选号时 `try_acquire` per-credential semaphore，满了就跳过该号。
    /// 防止单个凭据同时承受过多请求被 Kiro 风控。设 0 或 None 视为不限制（向后兼容）。
    /// 建议值：2-3（取决于凭据数量和上游限制）。
    #[serde(default)]
    pub max_inflight_per_credential: Option<u32>,

    /// Tier 化 retry 的 fallback 代理 URL（默认 None = 不启用）
    ///
    /// 当直连重试达到 `fallback_proxy_after_attempts` 次仍失败时，
    /// 后续重试自动切换到该代理出口（如 mihomo `http://172.17.0.1:17890`），
    /// 让 source IP 多样化绕过"IP × directory"维度的风控。
    ///
    /// 设计目标：99% 请求走直连保持低延迟，<1% 被 IP 风控的请求通过代理救活。
    /// 支持 http://host:port / socks5://host:port 格式（由 reqwest 解析）。
    #[serde(default)]
    pub fallback_proxy_url: Option<String>,

    /// 触发 fallback proxy 的 attempt 阈值（0-indexed，默认 None = 代码内置 10）
    ///
    /// 例如设 10：attempt 0-9 走直连，attempt >= 10 走 fallback proxy。
    /// 范围 [0, 30]。设 0 = 所有重试都走代理（不推荐，失去直连优势）。
    #[serde(default)]
    pub fallback_proxy_after_attempts: Option<usize>,

    /// Prompt prefix cache 是否启用（默认 true）
    ///
    /// 中转层自实现的 cache：相同 prefix（system + tools + history[..-1]）的多次请求
    /// 复用 conversation_id 并把 cache_*_input_tokens 真实上报给客户端，让命中率不再永远为 0。
    #[serde(default)]
    pub prompt_cache_enabled: Option<bool>,

    /// Prompt cache 容量（默认 1024）
    #[serde(default)]
    pub prompt_cache_capacity: Option<usize>,

    /// Prompt cache TTL 秒数（默认 300=5min，对齐 Anthropic ephemeral 规范）
    #[serde(default)]
    pub prompt_cache_ttl_secs: Option<u64>,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 自定义系统提示词补充内容（可选）
    ///
    /// 注入时：先拼接所有 `enabled_presets` 的内容，再追加这段自定义文本，
    /// 最后整体按 `system_prompt_position` 插入到 system role。
    #[serde(default)]
    pub system_prompt: Option<String>,

    /// 是否剥离客户端发来的限制性系统提示词（默认 false）
    /// 启用后会移除 Claude Code 内置的安全限制、沙箱策略、git 安全等指令
    #[serde(default)]
    pub strip_system_restrictions: bool,

    /// 系统提示词注入总开关（默认 false）
    ///
    /// 关闭时：完全不注入任何 preset 或自定义文本（但 `strip_system_restrictions`
    /// 仍独立生效）。打开后才会按 `enabled_presets` + `system_prompt` 拼接注入。
    #[serde(default)]
    pub system_prompt_enabled: bool,

    /// 启用的预设 id 列表（可以混合内置 + 用户自定义）
    ///
    /// 顺序无关 —— 实际拼接顺序：先内置（按 `PRESETS` 顺序）后用户（按 `user_presets` 顺序）。
    #[serde(default)]
    pub enabled_presets: Vec<String>,

    /// 用户自定义预设清单
    #[serde(default)]
    pub user_presets: Vec<UserPreset>,

    /// 自定义系统提示词注入位置（默认 `append`）
    ///
    /// - `prepend`：放到 system 数组最前
    /// - `append`：放到 system 数组末尾（recency bias，权重最高）
    #[serde(default)]
    pub system_prompt_position: SystemPromptPosition,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "0.11.107".to_string()
}

fn default_system_version() -> String {
    const SYSTEM_VERSIONS: &[&str] = &["darwin#24.6.0", "win32#10.0.22631"];
    SYSTEM_VERSIONS[fastrand::usize(..SYSTEM_VERSIONS.len())].to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_transient_cooldown_enabled() -> bool {
    true
}

fn default_extract_thinking() -> bool {
    true
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            load_balancing_mode: default_load_balancing_mode(),
            transient_cooldown_enabled: default_transient_cooldown_enabled(),
            rate_limit_cooldown_sec: None,
            upstream_error_cooldown_sec: None,
            overage_request_cooldown_sec: None,
            suspicious_activity_cooldown_sec: None,
            max_fallback_wait_secs: None,
            max_fallback_wait_attempts: None,
            max_inflight_kiro_requests: None,
            max_inflight_per_credential: None,
            fallback_proxy_url: None,
            fallback_proxy_after_attempts: None,
            prompt_cache_enabled: None,
            prompt_cache_capacity: None,
            prompt_cache_ttl_secs: None,
            extract_thinking: default_extract_thinking(),
            system_prompt: None,
            strip_system_restrictions: false,
            system_prompt_enabled: false,
            enabled_presets: Vec::new(),
            user_presets: Vec::new(),
            system_prompt_position: SystemPromptPosition::default(),
            default_endpoint: default_endpoint(),
            endpoints: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            return Ok(Self {
                config_path: Some(path.to_path_buf()),
                ..Self::default()
            });
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());
        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        // 原子写：tmp + rename，防进程中段被 kill 时 config.json 半写损坏
        crate::common::io::atomic_write_string(path, &content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}
