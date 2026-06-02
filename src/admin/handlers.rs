//! Admin API HTTP 处理器

use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};

use super::{
    middleware::AdminState,
    types::{
        AddCredentialRequest, CreateUserPresetRequest, PromptCacheConfigPayload,
        RetryConfigPayload, SetCredentialGroupRequest, SetDisabledRequest,
        SetLoadBalancingModeRequest, SetPriorityRequest, SuccessResponse,
        UpdateSystemPromptRequest, UpdateUserPresetRequest, UpsertCredentialGroupRequest,
    },
};

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials();
    Json(response)
}

/// GET /api/admin/metrics
/// 返回聚合的运行时指标（请求计数、延迟分位、cooldown/fallback 统计）
pub async fn get_metrics(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_metrics())
}

/// GET /api/admin/metrics/prometheus
/// 以 Prometheus / OpenMetrics 文本格式输出关键指标
///
/// 适合 Prometheus / Grafana / VictoriaMetrics scrape。返回 `text/plain;
/// charset=utf-8; version=0.0.4`，逐行 `metric{labels} value` 格式。
pub async fn get_metrics_prometheus(State(state): State<AdminState>) -> impl IntoResponse {
    let body = state.service.get_metrics_prometheus();
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// GET /api/admin/runtime/retry-config
/// 读取当前运行时 retry 配置
pub async fn get_retry_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_retry_config())
}

/// PUT /api/admin/runtime/retry-config
/// 更新 retry 配置（运行时即时生效 + 写回 config.json）
pub async fn update_retry_config(
    State(state): State<AdminState>,
    Json(payload): Json<RetryConfigPayload>,
) -> impl IntoResponse {
    match state.service.update_retry_config(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/runtime/prompt-cache-config
/// 读取当前 prompt cache 配置 + 运行时统计
pub async fn get_prompt_cache_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_prompt_cache_config())
}

/// PUT /api/admin/runtime/prompt-cache-config
/// 更新 prompt cache 配置（运行时即时生效 + 写回 config.json）
pub async fn update_prompt_cache_config(
    State(state): State<AdminState>,
    Json(payload): Json<PromptCacheConfigPayload>,
) -> impl IntoResponse {
    match state.service.update_prompt_cache_config(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/runtime/prompt-cache-config/clear
/// 清空 prompt cache（保留配置，但移除全部条目）
pub async fn clear_prompt_cache(State(state): State<AdminState>) -> impl IntoResponse {
    state.service.clear_prompt_cache();
    Json(SuccessResponse::new("prompt cache 已清空".to_string()))
}

/// POST /api/admin/credentials/:id/disabled
/// 设置凭据禁用状态
pub async fn set_credential_disabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetDisabledRequest>,
) -> impl IntoResponse {
    match state.service.set_disabled(id, payload.disabled) {
        Ok(_) => {
            let action = if payload.disabled { "禁用" } else { "启用" };
            Json(SuccessResponse::new(format!("凭据 #{} 已{}", id, action))).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/priority
/// 设置凭据优先级
pub async fn set_credential_priority(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetPriorityRequest>,
) -> impl IntoResponse {
    match state.service.set_priority(id, payload.priority) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 优先级已设置为 {}",
            id, payload.priority
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/group
/// 设置凭据分组
pub async fn set_credential_group(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetCredentialGroupRequest>,
) -> impl IntoResponse {
    match state.service.set_group(id, payload.group.clone()) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 分组已设置为 {}",
            id,
            payload.group.unwrap_or_else(|| "(none)".to_string())
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/reset
/// 重置失败计数并重新启用
pub async fn reset_failure_count(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reset_and_enable(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 失败计数已重置并重新启用",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/balance
/// 获取指定凭据的余额
pub async fn get_credential_balance(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.get_balance(id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials
/// 添加新凭据
pub async fn add_credential(
    State(state): State<AdminState>,
    Json(payload): Json<AddCredentialRequest>,
) -> impl IntoResponse {
    match state.service.add_credential(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/credentials/:id
/// 删除凭据
pub async fn delete_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/refresh
/// 强制刷新凭据 Token
pub async fn force_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.force_refresh_token(id).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} Token 已强制刷新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/load-balancing
/// 获取负载均衡模式
pub async fn get_load_balancing_mode(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_load_balancing_mode();
    Json(response)
}

/// PUT /api/admin/config/load-balancing
/// 设置负载均衡模式
pub async fn set_load_balancing_mode(
    State(state): State<AdminState>,
    Json(payload): Json<SetLoadBalancingModeRequest>,
) -> impl IntoResponse {
    match state.service.set_load_balancing_mode(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/credential-groups
/// 列出所有凭据分组（仅元数据，密码绝不回传）
pub async fn list_credential_groups(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.list_credential_groups())
}

/// POST /api/admin/config/credential-groups
/// 新建/更新凭据分组（按 id upsert，运行时即时生效 + 写回 config.json）
pub async fn upsert_credential_group(
    State(state): State<AdminState>,
    Json(payload): Json<UpsertCredentialGroupRequest>,
) -> impl IntoResponse {
    let id = payload.id.trim().to_string();
    match state.service.upsert_credential_group(payload) {
        Ok(_) => Json(SuccessResponse::new(format!("分组 \"{}\" 已保存", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/config/credential-groups/:id
/// 删除凭据分组（挂靠该组的凭据回落本机直连）
pub async fn delete_credential_group(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_credential_group(&id) {
        Ok(detached) => {
            let msg = if detached.is_empty() {
                format!("分组 \"{}\" 已删除", id)
            } else {
                format!(
                    "分组 \"{}\" 已删除，{} 个凭据已回落本机直连",
                    id,
                    detached.len()
                )
            };
            Json(SuccessResponse::new(msg)).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/system-prompt
/// 读取当前生效的系统提示词配置
pub async fn get_system_prompt(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_system_prompt())
}

/// PUT /api/admin/config/system-prompt
/// 更新系统提示词配置（运行时即时生效 + 写回 config.json）
pub async fn update_system_prompt(
    State(state): State<AdminState>,
    Json(payload): Json<UpdateSystemPromptRequest>,
) -> impl IntoResponse {
    match state.service.update_system_prompt(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/system-prompt/presets
/// 返回内置 preset 元数据清单（不含完整 content）
pub async fn list_presets(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.list_presets())
}

/// GET /api/admin/config/system-prompt/presets/:id
/// 返回单个 preset 的完整内容（用于前端"预览"按钮）
pub async fn get_preset_content(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.get_preset_content(&id) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/config/system-prompt/user-presets
/// 添加用户自定义预设
pub async fn add_user_preset(
    State(state): State<AdminState>,
    Json(payload): Json<CreateUserPresetRequest>,
) -> impl IntoResponse {
    match state.service.add_user_preset(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// PUT /api/admin/config/system-prompt/user-presets/:id
/// 编辑用户自定义预设
pub async fn update_user_preset(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateUserPresetRequest>,
) -> impl IntoResponse {
    match state.service.update_user_preset(&id, payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/config/system-prompt/user-presets/:id
/// 删除用户自定义预设
pub async fn delete_user_preset(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.service.delete_user_preset(&id) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}
