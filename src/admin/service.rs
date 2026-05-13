//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::{Config, UserPreset};
use crate::model::runtime::SharedPromptConfig;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, CreateUserPresetRequest,
    CredentialStatusItem, CredentialsStatusResponse, LoadBalancingModeResponse,
    PresetCatalogResponse, PresetContentResponse, PresetMetaResponse, SetLoadBalancingModeRequest,
    SystemPromptConfigResponse, UpdateSystemPromptRequest, UpdateUserPresetRequest,
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
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
        prompt_config: SharedPromptConfig,
        config_writer: Arc<Mutex<Config>>,
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
        }
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
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
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
                cfg.custom_content = if content.is_empty() { None } else { Some(content) };
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
            return Err(AdminServiceError::InvalidCredential("content 不能为空".into()));
        }

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
        }
        if let Some(ref content) = req.content {
            if content.trim().is_empty() {
                return Err(AdminServiceError::InvalidCredential(
                    "content 不能为空".into(),
                ));
            }
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

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

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
        let msg = e.to_string();

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
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据") {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 合法 id 应通过
    #[test]
    fn validate_id_accepts_valid() {
        for id in [
            "a", "abc", "my_preset", "v2-config", "123", "a_1-b_2",
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
            assert!(
                validate_user_preset_id(id).is_err(),
                "应拒绝大写: {:?}",
                id
            );
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

    /// 路径穿越/控制字符（被 [a-z0-9_-] 白名单自动拦截）
    #[test]
    fn validate_id_rejects_path_traversal_and_control_chars() {
        for id in [
            "../config",
            "../../etc/passwd",
            "foo/bar",
            "foo\\bar",
            "foo bar",  // 空格
            "foo.bar",  // 点
            "foo:bar",  // 冒号
            "foo\0bar", // null byte
            "foo\nbar", // 换行
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
