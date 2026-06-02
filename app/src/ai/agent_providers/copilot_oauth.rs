//! GitHub Copilot / GitHub Models OAuth 登录支持。

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AgentProviderOAuthCredentials, AgentProviderOAuthKind};

pub const COPILOT_PROVIDER_NAME: &str = "Copilot Auth";
pub const COPILOT_BASE_URL: &str = "https://api.githubcopilot.com/";

const GITHUB_COPILOT_OAUTH_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_USER_URL: &str = "https://api.github.com/user";
const GITHUB_COPILOT_MODELS_URL: &str = "https://api.githubcopilot.com/models";
const USER_AGENT: &str = "opencode/0.0.0";
const EDITOR_VERSION: &str = "vscode/1.107.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
const OAUTH_SCOPE: &str = "read:user";

#[derive(Debug, Clone)]
pub struct LoginFlow {
    pub auth_url: String,
    pub user_code: String,
    device_code: String,
    interval_secs: u64,
    expires_at_ms: u64,
    cancel_flag: Arc<AtomicBool>,
}

impl LoginFlow {
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel_flag.clone()
    }
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GitHubUserResponse {
    login: String,
}

#[derive(Serialize)]
struct DeviceCodeRequest<'a> {
    client_id: &'a str,
    scope: &'a str,
}

#[derive(Serialize)]
struct AccessTokenRequest<'a> {
    client_id: &'a str,
    device_code: &'a str,
    grant_type: &'a str,
}

#[cfg(not(target_family = "wasm"))]
fn build_blocking_reqwest_client() -> anyhow::Result<reqwest::blocking::Client> {
    use http_client::ProxyMode;

    let cfg = http_client::current_proxy_config();
    let builder = match cfg.mode {
        ProxyMode::System => reqwest::blocking::Client::builder(),
        ProxyMode::Off => reqwest::blocking::Client::builder().no_proxy(),
        ProxyMode::Custom => {
            let trimmed = cfg.url.trim();
            if trimmed.is_empty() {
                reqwest::blocking::Client::builder()
            } else {
                let mut proxy = reqwest::Proxy::all(trimmed)
                    .with_context(|| format!("无效的 HTTP 代理 URL: {trimmed}"))?;
                if !cfg.username.is_empty() || !cfg.password.is_empty() {
                    proxy = proxy.basic_auth(&cfg.username, &cfg.password);
                }
                if !cfg.no_proxy.trim().is_empty() {
                    if let Some(no_proxy) = reqwest::NoProxy::from_string(cfg.no_proxy.trim()) {
                        proxy = proxy.no_proxy(Some(no_proxy));
                    }
                }
                reqwest::blocking::Client::builder().proxy(proxy)
            }
        }
    };
    builder
        .build()
        .context("构造 Copilot blocking HTTP 客户端失败")
}

#[cfg(target_family = "wasm")]
fn build_blocking_reqwest_client() -> anyhow::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .build()
        .context("构造 Copilot blocking HTTP 客户端失败")
}

pub fn begin_login() -> anyhow::Result<LoginFlow> {
    let client = build_blocking_reqwest_client()?;
    let response = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&DeviceCodeRequest {
            client_id: GITHUB_COPILOT_OAUTH_CLIENT_ID,
            scope: OAUTH_SCOPE,
        })
        .send()
        .context("请求 GitHub device code 失败")?
        .error_for_status()
        .context("GitHub device code 返回错误状态")?
        .json::<DeviceCodeResponse>()
        .context("解析 GitHub device code 响应失败")?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();

    Ok(LoginFlow {
        auth_url: response.verification_uri,
        user_code: response.user_code,
        device_code: response.device_code,
        interval_secs: response.interval.max(1),
        expires_at_ms: now_ms + response.expires_in.saturating_mul(1000),
        cancel_flag: Arc::new(AtomicBool::new(false)),
    })
}

pub fn request_headers() -> Vec<(String, String)> {
    vec![
        ("Openai-Intent".to_string(), "conversation-edits".to_string()),
        ("X-Initiator".to_string(), "agent".to_string()),
        ("Editor-Version".to_string(), EDITOR_VERSION.to_string()),
        (
            "Editor-Plugin-Version".to_string(),
            EDITOR_PLUGIN_VERSION.to_string(),
        ),
        (
            "Copilot-Integration-Id".to_string(),
            COPILOT_INTEGRATION_ID.to_string(),
        ),
    ]
}

pub async fn wait_for_login(flow: LoginFlow) -> anyhow::Result<AgentProviderOAuthCredentials> {
    let client = http_client::Client::new();
    let mut interval_secs = flow.interval_secs;

    loop {
        if flow.cancel_flag.load(Ordering::Relaxed) {
            return Err(anyhow!("Copilot 登录已取消"));
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        if now_ms >= flow.expires_at_ms {
            return Err(anyhow!("Copilot 登录已过期，请重新点击登录"));
        }

        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        let payload = client
            .post(GITHUB_ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .json(&AccessTokenRequest {
                client_id: GITHUB_COPILOT_OAUTH_CLIENT_ID,
                device_code: flow.device_code.as_str(),
                grant_type: "urn:ietf:params:oauth:grant-type:device_code",
            })
            .send()
            .await
            .context("轮询 GitHub access token 失败")?
            .error_for_status()
            .map_err(|e| anyhow::Error::new(e).context("GitHub access token 返回错误状态"))?
            .json::<AccessTokenResponse>()
            .await
            .context("解析 GitHub access token 响应失败")?;

        if let Some(access_token) = payload.access_token {
            let account_id = fetch_github_login(&access_token).await?;
            return Ok(AgentProviderOAuthCredentials {
                kind: AgentProviderOAuthKind::Copilot,
                // 对齐 opencode:GitHub OAuth token 直接作为 api.githubcopilot.com
                // 请求的 Bearer token 使用。
                access_token: access_token.clone(),
                refresh_token: access_token,
                expires_at_ms: 0,
                account_id,
            });
        }

        match payload.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval_secs = payload
                    .interval
                    .filter(|interval| *interval > 0)
                    .unwrap_or_else(|| interval_secs.saturating_add(5));
            }
            Some("access_denied") => return Err(anyhow!("Copilot 登录已取消")),
            Some("expired_token") => return Err(anyhow!("Copilot 登录已过期，请重新点击登录")),
            Some(other) => {
                let extra = payload
                    .error_description
                    .as_deref()
                    .map(|msg| format!(" ({msg})"))
                    .unwrap_or_default();
                return Err(anyhow!("Copilot OAuth 失败: {other}{extra}"));
            }
            None => {
                return Err(anyhow!("Copilot OAuth 响应缺少 access_token"));
            }
        }
    }
}

pub async fn refresh_credentials(
    credentials: &AgentProviderOAuthCredentials,
) -> anyhow::Result<AgentProviderOAuthCredentials> {
    let github_token = github_token(credentials);
    Ok(AgentProviderOAuthCredentials {
        kind: AgentProviderOAuthKind::Copilot,
        access_token: github_token.to_owned(),
        refresh_token: github_token.to_owned(),
        expires_at_ms: u64::MAX,
        account_id: credentials.account_id.clone(),
    })
}

pub fn github_token(credentials: &AgentProviderOAuthCredentials) -> &str {
    if credentials.refresh_token.is_empty() {
        credentials.access_token.as_str()
    } else {
        credentials.refresh_token.as_str()
    }
}

pub fn cancel_login(flow: &LoginFlow) {
    flow.cancel_flag.store(true, Ordering::Relaxed);
}

async fn fetch_github_login(token: &str) -> anyhow::Result<String> {
    let client = http_client::Client::new();
    let response: GitHubUserResponse = client
        .get(GITHUB_USER_URL)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("请求 GitHub user 失败")?
        .error_for_status()
        .map_err(|e| anyhow::Error::new(e).context("GitHub user 返回错误状态"))?
        .json()
        .await
        .context("解析 GitHub user 响应失败")?;
    Ok(response.login)
}

pub async fn fetch_copilot_oauth_models(
    token: &str,
) -> anyhow::Result<Vec<crate::settings::AgentProviderModel>> {
    let client = http_client::Client::new();
    let headers = request_headers();
    let payload = client
        .get(GITHUB_COPILOT_MODELS_URL)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", USER_AGENT)
        .header(&headers[0].0, &headers[0].1)
        .header(&headers[1].0, &headers[1].1)
        .header(&headers[2].0, &headers[2].1)
        .send()
        .await
        .context("请求 GitHub Copilot models 失败")?
        .error_for_status()
        .map_err(|e| anyhow::Error::new(e).context("GitHub Copilot models 返回错误状态"))?
        .json::<Value>()
        .await
        .context("解析 GitHub Copilot models 失败")?;

    let entries = match &payload {
        Value::Array(items) => items,
        Value::Object(map) => map
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("GitHub Copilot models 响应缺少 data 数组"))?,
        _ => {
            return Err(anyhow!(
                "GitHub Copilot models 响应格式不支持: 期望数组或 {{ data: [...] }}"
            ));
        }
    };

    let mut models: Vec<_> = entries.iter().filter_map(parse_copilot_model).collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    if models.is_empty() {
        return Err(anyhow!("GitHub Copilot models 返回了空模型列表"));
    }
    Ok(models)
}

pub fn default_copilot_oauth_models() -> Vec<crate::settings::AgentProviderModel> {
    // Copilot 的 /models endpoint 会按账号、计划和客户端能力做过滤,部分 token 会返回 400。
    // 登录流程不能因此失败,这里保留一组可编辑的保守默认值。
    [
        ("GPT-5.4", "gpt-5.4", 272_000, 128_000, true),
        ("GPT-5.4 Mini", "gpt-5.4-mini", 272_000, 128_000, true),
        ("GPT-5.3 Codex", "gpt-5.3-codex", 272_000, 128_000, true),
        ("GPT-5 Mini", "gpt-5-mini", 128_000, 32_000, true),
        ("GPT-4.1", "gpt-4.1", 128_000, 16_384, false),
    ]
    .into_iter()
    .map(|(name, id, context_window, max_output_tokens, reasoning)| {
        crate::settings::AgentProviderModel {
            name: name.to_string(),
            id: id.to_string(),
            context_window,
            max_output_tokens,
            reasoning,
            tool_call: true,
            image: Some(true),
            pdf: Some(false),
            audio: Some(false),
        }
    })
    .collect()
}

fn parse_copilot_model(entry: &Value) -> Option<crate::settings::AgentProviderModel> {
    let id = entry.get("id")?.as_str()?.trim().to_owned();
    if id.is_empty() {
        return None;
    }

    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| entry.get("display_name").and_then(Value::as_str))
        .or_else(|| entry.get("model_picker_name").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(id.as_str())
        .to_owned();

    let context_window = entry
        .pointer("/capabilities/limits/max_context_window_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or_default();
    let max_output_tokens = entry
        .pointer("/capabilities/limits/max_output_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or_default();
    let supports = entry.get("capabilities")?.get("supports")?;
    let reasoning = supports
        .get("adaptive_thinking")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || supports
            .get("reasoning_effort")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
        || supports.get("max_thinking_budget").is_some()
        || supports.get("min_thinking_budget").is_some();
    let image = supports
        .get("vision")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || entry
            .pointer("/capabilities/limits/vision/supported_media_types")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|media_type| media_type.starts_with("image/"))
            });

    Some(crate::settings::AgentProviderModel {
        name,
        id,
        context_window,
        max_output_tokens,
        reasoning,
        tool_call: true,
        image: Some(image),
        pdf: None,
        audio: None,
    })
}
