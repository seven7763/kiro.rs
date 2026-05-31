// `if let Some(x) = ... { if cond { ... } }` 在很多 admin handler / stream parser 里
// 嵌套两层比 if-let-chain (`if let Some(x) = ... && cond`) 更易读且 diff 更稳定，
// 因此在 crate 级别允许 collapsible_if，避免被 clippy 强行合并降低可读性。
#![allow(clippy::collapsible_if)]

mod admin;
mod admin_ui;
mod anthropic;
mod common;
mod http_client;
mod kiro;
mod model;
pub mod token;

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use kiro::endpoint::{IdeEndpoint, KiroEndpoint};
use kiro::metrics::MetricsRecorder;
use kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use kiro::provider::KiroProvider;
use kiro::token_manager::MultiTokenManager;
use model::arg::Args;
use model::config::Config;
use model::runtime::{shared_from_config, shared_retry_config_from};
use parking_lot::Mutex;

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 加载配置
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 加载凭证（支持单对象或数组格式）
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());
    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        tracing::error!("加载凭证失败: {}", e);
        std::process::exit(1);
    });

    // 判断是否为多凭据格式（用于刷新后回写）
    let is_multiple_format = credentials_config.is_multiple();

    // 转换为按优先级排序的凭据列表
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 获取第一个凭据用于日志显示（仅打印非敏感摘要，绝不 Debug dump 明文凭据）
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!(
        "主凭证: auth_method={:?}",
        first_credentials
            .auth_method
            .as_deref()
            .unwrap_or("default")
    );

    // 获取 API Key
    let api_key = config.api_key.clone().unwrap_or_else(|| {
        tracing::error!("配置文件中未设置 apiKey");
        std::process::exit(1);
    });

    // 构建代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 构建端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
    }

    // 校验默认端点存在
    if !endpoints.contains_key(&config.default_endpoint) {
        tracing::error!("默认端点 \"{}\" 未注册", config.default_endpoint);
        std::process::exit(1);
    }

    // 校验所有凭据声明的端点都已注册
    for cred in &credentials_list {
        let name = cred.endpoint.as_deref().unwrap_or(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            tracing::error!(
                "凭据 id={:?} 指定了未知端点 \"{}\"（已注册: {:?}）",
                cred.id,
                name,
                endpoints.keys().collect::<Vec<_>>()
            );
            std::process::exit(1);
        }
    }

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.into()),
        is_multiple_format,
    )
    .unwrap_or_else(|e| {
        tracing::error!("创建 Token 管理器失败: {}", e);
        std::process::exit(1);
    });
    let token_manager = Arc::new(token_manager);
    // 进程级 metrics 记录器：在 KiroProvider 与 AdminService 之间共享
    let metrics_recorder = Arc::new(MetricsRecorder::new());
    // 运行时 retry 配置：与 token_manager 和 admin service 共享，允许 Admin 热改
    let retry_config = shared_retry_config_from(&config);
    token_manager.attach_retry_config(retry_config.clone());
    let kiro_provider = KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
        metrics_recorder.clone(),
    );

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
    });

    // 构建共享运行时 Prompt 配置（Anthropic handler 与 Admin 共享，热更新无需重启）
    let prompt_config = shared_from_config(&config);
    // 可写 Config 句柄（Admin 写入 system prompt 等字段时回写 config.json）
    let config_writer: Arc<Mutex<Config>> = Arc::new(Mutex::new(config.clone()));

    // 构建中转层 Prompt cache（Anthropic handler 与 Admin 共享同一实例）
    let prompt_cache = anthropic::prompt_cache::PromptCache::new_with_perceived(
        config
            .prompt_cache_capacity
            .unwrap_or(anthropic::prompt_cache::DEFAULT_CAPACITY),
        std::time::Duration::from_secs(
            config
                .prompt_cache_ttl_secs
                .unwrap_or(anthropic::prompt_cache::DEFAULT_TTL.as_secs()),
        ),
        config.prompt_cache_enabled.unwrap_or(true),
        config.perceived_cache_hit_ratio,
    );

    // 构建 Anthropic API 路由（profile_arn 由 provider 层根据实际凭据动态注入）
    let anthropic_app = anthropic::create_router_with_provider(
        &api_key,
        Some(kiro_provider),
        config.extract_thinking,
        prompt_config.clone(),
        prompt_cache.clone(),
    );

    // 构建 Admin API 路由（如果配置了非空的 admin_api_key）
    // 安全检查：空字符串被视为未配置，防止空 key 绕过认证
    let admin_key_valid = config
        .admin_api_key
        .as_ref()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);

    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app
        } else {
            let admin_service = admin::AdminService::new(
                token_manager.clone(),
                endpoint_names.clone(),
                prompt_config.clone(),
                config_writer.clone(),
                metrics_recorder.clone(),
                retry_config.clone(),
                prompt_cache.clone(),
            );
            let admin_state = admin::AdminState::new(admin_key, admin_service);
            let admin_app = admin::create_admin_router(admin_state);

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .nest("/admin", admin_ui_app)
        }
    } else {
        anthropic_app
    };

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    // API Key 脱敏：打印一半会折损暴力破解空间且可能泄露短 key，改为首 4 + 尾 4
    let masked_api_key = if api_key.is_ascii() && api_key.len() > 16 {
        format!("{}...{}", &api_key[..4], &api_key[api_key.len() - 4..])
    } else {
        "***".to_string()
    };
    tracing::info!("API Key: {}", masked_api_key);
    if config.strip_system_restrictions {
        tracing::info!("系统提示词限制剥离: 已启用");
    }
    if config.system_prompt_enabled {
        tracing::info!(
            "自定义系统提示词注入: 已启用 (presets={:?}, custom={})",
            config.enabled_presets,
            config.system_prompt.is_some()
        );
    }
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    if admin_key_valid {
        tracing::info!("Admin API:");
        tracing::info!("  GET  /api/admin/credentials");
        tracing::info!("  POST /api/admin/credentials/:index/disabled");
        tracing::info!("  POST /api/admin/credentials/:index/priority");
        tracing::info!("  POST /api/admin/credentials/:index/reset");
        tracing::info!("  GET  /api/admin/credentials/:index/balance");
        tracing::info!("Admin UI:");
        tracing::info!("  GET  /admin");
    }

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
