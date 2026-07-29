//! profileArn 自动探测
//!
//! 当凭据缺少 `profileArn` 时，遍历常见 region 调用
//! `POST /ListAvailableProfiles`，取第一个可用 profile。

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

/// 默认探测 region 列表（与 Kiro IDE 常见部署一致）
pub const DEFAULT_PROFILE_REGIONS: &[&str] =
    &["us-east-1", "eu-central-1", "ap-southeast-1", "us-west-2"];

/// ListAvailableProfiles 响应中的单个 profile
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableProfile {
    #[serde(default)]
    pub arn: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAvailableProfilesResponse {
    #[serde(default)]
    profiles: Vec<AvailableProfile>,
    /// 部分响应可能用不同字段名
    #[serde(default)]
    available_profiles: Vec<AvailableProfile>,
}

impl ListAvailableProfilesResponse {
    fn first_profile(&self) -> Option<&AvailableProfile> {
        self.profiles
            .iter()
            .chain(self.available_profiles.iter())
            .find(|p| p.arn.as_ref().map(|a| !a.is_empty()).unwrap_or(false))
    }
}

/// 探测结果
#[derive(Debug, Clone)]
pub struct DiscoveredProfile {
    pub profile_arn: String,
    #[allow(dead_code)]
    pub name: Option<String>,
    pub region: String,
}

/// 在指定 region 列表上探测 profile。
///
/// `token` 为当前 access token。失败返回 Err（全部 region 都失败）。
pub async fn discover_profile_arn(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    regions: &[&str],
) -> anyhow::Result<DiscoveredProfile> {
    let mut last_err: Option<anyhow::Error> = None;

    // 优先凭据已声明的 api/auth region
    let mut ordered: Vec<String> = Vec::new();
    for r in [
        credentials.api_region.as_deref(),
        credentials.region.as_deref(),
        credentials.auth_region.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let r = r.trim();
        if !r.is_empty() && !ordered.iter().any(|x| x == r) {
            ordered.push(r.to_string());
        }
    }
    for r in regions {
        if !ordered.iter().any(|x| x == r) {
            ordered.push((*r).to_string());
        }
    }

    for region in ordered {
        match list_available_profiles_in_region(credentials, config, token, proxy, &region).await {
            Ok(Some(profile)) => {
                tracing::info!(
                    "ListAvailableProfiles 命中 region={} arn={}",
                    region,
                    profile.profile_arn
                );
                return Ok(profile);
            }
            Ok(None) => {
                tracing::debug!("ListAvailableProfiles region={} 无可用 profile", region);
            }
            Err(e) => {
                tracing::debug!("ListAvailableProfiles region={} 失败: {}", region, e);
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("所有 region 均未返回可用 profile")))
}

async fn list_available_profiles_in_region(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    region: &str,
) -> anyhow::Result<Option<DiscoveredProfile>> {
    let host = format!("q.{}.amazonaws.com", region);
    let url = format!("https://{}/ListAvailableProfiles", host);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        os_name, node_version, kiro_version, machine_id
    );
    let amz_user_agent = format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", kiro_version, machine_id);

    let client = build_client(proxy, 30, config.tls_backend)?;
    let mut request = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token))
        .header("Connection", "close")
        .body("{}");

    if credentials.is_api_key_credential() {
        request = request.header("tokentype", "API_KEY");
    } else if credentials.is_external_idp_credential() {
        request = request.header("TokenType", "EXTERNAL_IDP");
    }

    let response = request
        .send()
        .await
        .context("ListAvailableProfiles 请求失败")?;
    let status = response.status();
    let body_text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let redacted = crate::common::redact::redact_secret_text(&body_text);
        bail!("ListAvailableProfiles {}: {}", status, redacted);
    }

    let data: ListAvailableProfilesResponse =
        serde_json::from_str(&body_text).with_context(|| "解析 ListAvailableProfiles 响应失败")?;

    if let Some(p) = data.first_profile() {
        let arn = p.arn.clone().unwrap();
        return Ok(Some(DiscoveredProfile {
            profile_arn: arn,
            name: p.name.clone(),
            region: region.to_string(),
        }));
    }
    Ok(None)
}

/// 若凭据缺少 profile_arn，则尝试探测并回填；失败只记 warn。
pub async fn ensure_profile_arn(
    mut credentials: KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> KiroCredentials {
    if credentials
        .profile_arn
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        return credentials;
    }

    let token = match credentials.access_token.as_deref() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            tracing::warn!("缺少 accessToken，跳过 profileArn 探测");
            return credentials;
        }
    };

    match discover_profile_arn(&credentials, config, &token, proxy, DEFAULT_PROFILE_REGIONS).await {
        Ok(found) => {
            credentials.profile_arn = Some(found.profile_arn);
            if credentials.api_region.is_none() {
                credentials.api_region = Some(found.region.clone());
            }
            if credentials.region.is_none() {
                credentials.region = Some(found.region);
            }
            credentials
        }
        Err(e) => {
            tracing::warn!("自动探测 profileArn 失败（不阻断）: {}", e);
            credentials
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profiles_response() {
        let json =
            r#"{"profiles":[{"arn":"arn:aws:codewhisperer:us-east-1:1:profile/ABC","name":"p1"}]}"#;
        let data: ListAvailableProfilesResponse = serde_json::from_str(json).unwrap();
        let p = data.first_profile().unwrap();
        assert_eq!(
            p.arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:1:profile/ABC")
        );
    }

    #[test]
    fn parse_available_profiles_alt_key() {
        let json = r#"{"availableProfiles":[{"arn":"arn:test","name":"n"}]}"#;
        let data: ListAvailableProfilesResponse = serde_json::from_str(json).unwrap();
        assert!(data.first_profile().is_some());
    }
}
