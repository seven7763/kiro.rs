//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的 AWS CodeWhisperer 端点：
//! - API: `https://q.{api_region}.amazonaws.com/generateAssistantResponse`
//! - MCP: `https://q.{api_region}.amazonaws.com/mcp`
//!
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体会在根对象上注入 `profileArn`。
//! external_idp 凭据额外带 `TokenType: EXTERNAL_IDP`，并剥离企业 API 不兼容字段。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

/// Kiro IDE 端点名称
pub const IDE_ENDPOINT_NAME: &str = "ide";

/// Kiro IDE 端点
pub struct IdeEndpoint;

impl IdeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            ctx.config.kiro_version, ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }
}

impl Default for IdeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn name(&self) -> &'static str {
        IDE_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://q.{}.amazonaws.com/generateAssistantResponse",
            self.api_region(ctx)
        )
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://q.{}.amazonaws.com/mcp", self.api_region(ctx))
    }

    fn models_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://q.{}.amazonaws.com/ListAvailableModels?origin=AI_EDITOR",
            self.api_region(ctx)
        )
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        } else if ctx.credentials.is_external_idp_credential() {
            req = req.header("TokenType", "EXTERNAL_IDP");
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(ref arn) = ctx.credentials.profile_arn {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        } else if ctx.credentials.is_external_idp_credential() {
            req = req.header("TokenType", "EXTERNAL_IDP");
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        let body = if ctx.credentials.is_external_idp_credential() {
            strip_enterprise_incompatible_fields(body)
        } else {
            body.to_string()
        };
        inject_profile_arn(&body, &ctx.credentials.profile_arn)
    }
}

/// 将 profile_arn 注入到请求体 JSON 根对象
fn inject_profile_arn(request_body: &str, profile_arn: &Option<String>) -> String {
    if let Some(arn) = profile_arn {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) {
            json["profileArn"] = serde_json::Value::String(arn.clone());
            if let Ok(body) = serde_json::to_string(&json) {
                return body;
            }
        }
    }
    request_body.to_string()
}

/// external_idp 企业 API 对部分字段返回 400：剥离 agentContinuationId / agentTaskType，
/// 以及空的 userInputMessageContext（无 tools / toolResults）。
fn strip_enterprise_incompatible_fields(request_body: &str) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) else {
        return request_body.to_string();
    };

    if let Some(state) = json.get_mut("conversationState") {
        if let Some(obj) = state.as_object_mut() {
            obj.remove("agentContinuationId");
            obj.remove("agentTaskType");

            if let Some(current) = obj.get_mut("currentMessage") {
                if let Some(user_msg) = current.get_mut("userInputMessage") {
                    if let Some(user_obj) = user_msg.as_object_mut() {
                        let empty_ctx = user_obj
                            .get("userInputMessageContext")
                            .map(is_empty_user_input_message_context)
                            .unwrap_or(false);
                        if empty_ctx {
                            user_obj.remove("userInputMessageContext");
                        }
                    }
                }
            }
        }
    }

    serde_json::to_string(&json).unwrap_or_else(|_| request_body.to_string())
}

fn is_empty_user_input_message_context(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return true;
            }
            let tools_empty = map
                .get("tools")
                .map(|v| v.as_array().map(|a| a.is_empty()).unwrap_or(false))
                .unwrap_or(true);
            let results_empty = map
                .get("toolResults")
                .map(|v| v.as_array().map(|a| a.is_empty()).unwrap_or(false))
                .unwrap_or(true);
            // 仅当没有实质工具/结果时视为空上下文
            tools_empty && results_empty && {
                // 其它键若也全是空数组/空对象/null 仍视为空
                map.iter().all(|(k, v)| {
                    if k == "tools" || k == "toolResults" {
                        true
                    } else {
                        match v {
                            serde_json::Value::Null => true,
                            serde_json::Value::Array(a) => a.is_empty(),
                            serde_json::Value::Object(o) => o.is_empty(),
                            serde_json::Value::String(s) => s.is_empty(),
                            _ => false,
                        }
                    }
                })
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{inject_profile_arn, strip_enterprise_incompatible_fields};
    use serde_json::Value;

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let result = inject_profile_arn(body, &None);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let arn = Some("new-arn".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let arn = Some("arn:test".to_string());
        let result = inject_profile_arn(body, &arn);
        assert_eq!(result, "not-valid-json");
    }

    #[test]
    fn test_strip_enterprise_fields() {
        let body = r#"{
            "conversationState": {
                "agentContinuationId": "cont-1",
                "agentTaskType": "vibe",
                "conversationId": "c1",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "hi",
                        "modelId": "m",
                        "userInputMessageContext": {}
                    }
                }
            }
        }"#;
        let stripped = strip_enterprise_incompatible_fields(body);
        let json: Value = serde_json::from_str(&stripped).unwrap();
        let state = &json["conversationState"];
        assert!(state.get("agentContinuationId").is_none());
        assert!(state.get("agentTaskType").is_none());
        assert_eq!(state["conversationId"], "c1");
        assert!(
            state["currentMessage"]["userInputMessage"]
                .get("userInputMessageContext")
                .is_none()
        );
    }

    #[test]
    fn test_strip_keeps_tools_context() {
        let body = r#"{
            "conversationState": {
                "agentContinuationId": "x",
                "currentMessage": {
                    "userInputMessage": {
                        "userInputMessageContext": {
                            "tools": [{"toolSpecification":{"name":"t"}}]
                        }
                    }
                }
            }
        }"#;
        let stripped = strip_enterprise_incompatible_fields(body);
        let json: Value = serde_json::from_str(&stripped).unwrap();
        assert!(
            json["conversationState"]["currentMessage"]["userInputMessage"]
                .get("userInputMessageContext")
                .is_some()
        );
        assert!(json["conversationState"].get("agentContinuationId").is_none());
    }
}
