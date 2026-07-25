use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::Result;

pub const OPENAI_CHATGPT_PROVIDER_ID: &str = "openai-chatgpt";
/// Public OAuth client ID used by the official OpenAI Codex CLI.
/// Source: https://github.com/openai/codex/blob/0fb559f0f6e231a88ac02ea002d3ecd248e2b515/codex-rs/login/src/auth/manager.rs#L1445-L1452
pub const OPENAI_CHATGPT_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_AUTH_BASE_URL: &str = "https://auth.openai.com";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REVOKE_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthTokenSet {
    pub access_token: String,
    pub account_id: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodePrompt {
    pub verification_url: String,
    pub user_code: String,
}

#[async_trait]
pub(crate) trait OAuthCredentialProvider: Send + Sync {
    fn id(&self) -> &'static str;

    async fn refresh(&self, refresh_token: &str) -> Result<OAuthTokenSet>;

    async fn revoke(&self, access_token: Option<&str>, refresh_token: Option<&str>) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct OAuthProviderRegistry {
    providers: HashMap<String, Arc<dyn OAuthCredentialProvider>>,
}

impl OAuthProviderRegistry {
    pub(crate) fn built_in() -> Self {
        Self::new().with_provider(Arc::new(OpenAiChatGptOAuthProvider::new()))
    }

    fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    fn with_provider(mut self, provider: Arc<dyn OAuthCredentialProvider>) -> Self {
        self.providers.insert(provider.id().to_string(), provider);
        self
    }

    pub(crate) fn get(&self, provider: &str) -> Result<Arc<dyn OAuthCredentialProvider>> {
        self.providers
            .get(provider)
            .cloned()
            .ok_or_else(|| anyhow!("unknown OAuth credential provider `{provider}`"))
    }
}

#[derive(Clone)]
pub struct OpenAiChatGptOAuthProvider {
    client: Client,
    auth_base_url: String,
    login_timeout: Duration,
    revoke_timeout: Duration,
    request_timeout: Duration,
}

impl Default for OpenAiChatGptOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiChatGptOAuthProvider {
    pub fn new() -> Self {
        Self::with_auth_base_url(DEFAULT_AUTH_BASE_URL)
    }

    pub fn with_auth_base_url(auth_base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            auth_base_url: auth_base_url.into().trim_end_matches('/').to_string(),
            login_timeout: LOGIN_TIMEOUT,
            revoke_timeout: REVOKE_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeouts(mut self, login: Duration, revoke: Duration) -> Self {
        self.login_timeout = login;
        self.revoke_timeout = revoke;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub async fn login_device_code(
        &self,
        on_prompt: impl FnOnce(&DeviceCodePrompt),
    ) -> Result<OAuthTokenSet> {
        tokio::time::timeout(self.login_timeout, async {
            let device = self.request_device_code().await?;
            on_prompt(&DeviceCodePrompt {
                verification_url: format!("{}/codex/device", self.auth_base_url),
                user_code: device.user_code.clone(),
            });

            let code = self.poll_for_authorization(&device).await?;
            self.exchange_authorization_code(code).await
        })
        .await
        .map_err(|_| anyhow!("OpenAI device login timed out"))?
    }

    async fn request_device_code(&self) -> Result<DeviceCodeResponse> {
        let response = self
            .client
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.auth_base_url
            ))
            .json(&DeviceCodeRequest {
                client_id: OPENAI_CHATGPT_OAUTH_CLIENT_ID,
            })
            .timeout(self.request_timeout)
            .send()
            .await
            .context("failed to request an OpenAI device code")?;
        let status = response.status();
        if !status.is_success() {
            bail!("OpenAI device-code request failed with status {status}");
        }
        response
            .json::<DeviceCodeResponse>()
            .await
            .context("OpenAI returned a malformed device-code response")
    }

    async fn poll_for_authorization(
        &self,
        device: &DeviceCodeResponse,
    ) -> Result<DeviceAuthorizationResponse> {
        let started = tokio::time::Instant::now();
        let mut interval = Duration::from_secs(device.interval.seconds()?.max(1));
        let endpoint = format!("{}/api/accounts/deviceauth/token", self.auth_base_url);

        loop {
            if started.elapsed() >= self.login_timeout {
                bail!("OpenAI device login timed out after 15 minutes");
            }

            let response = self
                .client
                .post(&endpoint)
                .json(&DevicePollRequest {
                    device_auth_id: &device.device_auth_id,
                    user_code: &device.user_code,
                })
                .timeout(
                    self.request_timeout
                        .min(self.login_timeout.saturating_sub(started.elapsed())),
                )
                .send()
                .await
                .context("failed while polling OpenAI device authorization")?;
            let status = response.status();
            if status.is_success() {
                return response
                    .json::<DeviceAuthorizationResponse>()
                    .await
                    .context("OpenAI returned a malformed device authorization response");
            }

            let error = response.json::<OAuthErrorResponse>().await.ok();
            let pending = matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND)
                || error.as_ref().is_some_and(|error| {
                    matches!(
                        error.error.as_deref(),
                        Some("authorization_pending" | "deviceauth_authorization_pending")
                    )
                });
            let slow_down = error
                .as_ref()
                .is_some_and(|error| error.error.as_deref() == Some("slow_down"));
            if slow_down {
                interval = next_poll_interval(interval, true);
            } else if !pending {
                let detail = error
                    .and_then(|error| error.error_description.or(error.error))
                    .unwrap_or_else(|| "unknown error".to_string());
                bail!("OpenAI device authorization failed with status {status}: {detail}");
            }

            let remaining = self.login_timeout.saturating_sub(started.elapsed());
            tokio::time::sleep(interval.min(remaining)).await;
        }
    }

    async fn exchange_authorization_code(
        &self,
        code: DeviceAuthorizationResponse,
    ) -> Result<OAuthTokenSet> {
        let response = self
            .client
            .post(format!("{}/oauth/token", self.auth_base_url))
            .form(&AuthorizationCodeExchangeRequest {
                grant_type: "authorization_code",
                code: &code.authorization_code,
                redirect_uri: &format!("{}/deviceauth/callback", self.auth_base_url),
                client_id: OPENAI_CHATGPT_OAUTH_CLIENT_ID,
                code_verifier: &code.code_verifier,
            })
            .timeout(self.request_timeout)
            .send()
            .await
            .context("failed to exchange the OpenAI device authorization code")?;
        parse_token_response(response, None).await
    }
}

fn next_poll_interval(current: Duration, slow_down: bool) -> Duration {
    if slow_down {
        current + Duration::from_secs(5)
    } else {
        current
    }
}

#[async_trait]
impl OAuthCredentialProvider for OpenAiChatGptOAuthProvider {
    fn id(&self) -> &'static str {
        OPENAI_CHATGPT_PROVIDER_ID
    }

    async fn refresh(&self, refresh_token: &str) -> Result<OAuthTokenSet> {
        let response = self
            .client
            .post(format!("{}/oauth/token", self.auth_base_url))
            .json(&RefreshRequest {
                client_id: OPENAI_CHATGPT_OAUTH_CLIENT_ID,
                grant_type: "refresh_token",
                refresh_token,
            })
            .timeout(self.request_timeout)
            .send()
            .await
            .context("failed to refresh the OpenAI credential")?;
        parse_token_response(response, Some(refresh_token)).await
    }

    async fn revoke(&self, access_token: Option<&str>, refresh_token: Option<&str>) -> Result<()> {
        let (token, token_type_hint, client_id) = if let Some(refresh_token) = refresh_token {
            (
                refresh_token,
                "refresh_token",
                Some(OPENAI_CHATGPT_OAUTH_CLIENT_ID),
            )
        } else if let Some(access_token) = access_token {
            (access_token, "access_token", None)
        } else {
            return Ok(());
        };
        let response = self
            .client
            .post(format!("{}/oauth/revoke", self.auth_base_url))
            .timeout(self.revoke_timeout)
            .json(&RevokeRequest {
                token,
                token_type_hint,
                client_id,
            })
            .send()
            .await
            .context("failed to revoke the OpenAI credential")?;
        if !response.status().is_success() {
            bail!(
                "OpenAI credential revocation failed with status {}",
                response.status()
            );
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct DeviceCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: DevicePollingInterval,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DevicePollingInterval {
    String(String),
    Number(u64),
}

impl Default for DevicePollingInterval {
    fn default() -> Self {
        Self::Number(5)
    }
}

impl DevicePollingInterval {
    fn seconds(&self) -> Result<u64> {
        match self {
            Self::String(value) => value
                .parse()
                .with_context(|| format!("invalid device polling interval `{value}`")),
            Self::Number(value) => Ok(*value),
        }
    }
}

#[derive(Serialize)]
struct DevicePollRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    authorization_code: String,
    #[serde(rename = "code_challenge")]
    _code_challenge: String,
    code_verifier: String,
}

#[derive(Serialize)]
struct AuthorizationCodeExchangeRequest<'a> {
    grant_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Serialize)]
struct RevokeRequest<'a> {
    token: &'a str,
    token_type_hint: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct JwtExpiryClaims {
    exp: i64,
}

#[derive(Deserialize)]
struct ChatGptIdTokenClaims {
    #[serde(rename = "https://api.openai.com/auth")]
    auth: ChatGptAuthClaims,
}

#[derive(Deserialize)]
struct ChatGptAuthClaims {
    chatgpt_account_id: String,
}

async fn parse_token_response(
    response: reqwest::Response,
    current_refresh_token: Option<&str>,
) -> Result<OAuthTokenSet> {
    let status = response.status();
    if !status.is_success() {
        let error = response.json::<OAuthErrorResponse>().await.ok();
        let detail = error
            .and_then(|error| error.error_description.or(error.error))
            .unwrap_or_else(|| "unknown error".to_string());
        bail!("OpenAI token request failed with status {status}: {detail}");
    }
    let tokens = response
        .json::<TokenResponse>()
        .await
        .context("OpenAI returned a malformed token response")?;
    let access_token = tokens
        .access_token
        .ok_or_else(|| anyhow!("OpenAI token response omitted access_token"))?;
    let expires_at = match tokens.expires_in {
        Some(seconds) => Utc::now() + TimeDelta::seconds(seconds),
        None => jwt_expiry(&access_token)?,
    };
    let account_id = tokens
        .id_token
        .as_deref()
        .map(chatgpt_account_id)
        .transpose()?;
    if current_refresh_token.is_none() && account_id.is_none() {
        bail!("OpenAI token response omitted id_token");
    }
    Ok(OAuthTokenSet {
        access_token,
        account_id,
        refresh_token: tokens
            .refresh_token
            .or_else(|| current_refresh_token.map(str::to_string)),
        expires_at,
    })
}

fn chatgpt_account_id(id_token: &str) -> Result<String> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("OpenAI ID token is not a JWT"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .context("OpenAI ID token has malformed JWT encoding")?;
    let claims: ChatGptIdTokenClaims =
        serde_json::from_slice(&bytes).context("OpenAI ID token has malformed JWT claims")?;
    Ok(claims.auth.chatgpt_account_id)
}

fn jwt_expiry(token: &str) -> Result<DateTime<Utc>> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("OpenAI access token is not a JWT and expires_in was omitted"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .context("OpenAI access token has malformed JWT encoding")?;
    let claims: JwtExpiryClaims =
        serde_json::from_slice(&bytes).context("OpenAI access token has malformed JWT claims")?;
    DateTime::from_timestamp(claims.exp, 0)
        .ok_or_else(|| anyhow!("OpenAI access token has an invalid expiry timestamp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wiremock::matchers::{body_json, body_string_contains, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn id_token(account_id: &str) -> String {
        let claims = serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id
            }
        }))
        .unwrap();
        format!("header.{}.signature", URL_SAFE_NO_PAD.encode(claims))
    }

    #[tokio::test]
    async fn device_login_uses_typed_codex_exchange() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/usercode"))
            .and(body_json(serde_json::json!({
                "client_id": OPENAI_CHATGPT_OAUTH_CLIENT_ID
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_auth_id": "device-id",
                "user_code": "ABCD-EFGH",
                "interval": "0"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/token"))
            .and(body_json(serde_json::json!({
                "device_auth_id": "device-id",
                "user_code": "ABCD-EFGH"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "authorization_code": "authorization-code",
                "code_challenge": "challenge",
                "code_verifier": "verifier"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=authorization-code"))
            .and(body_string_contains(format!(
                "client_id={OPENAI_CHATGPT_OAUTH_CLIENT_ID}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-token",
                "id_token": id_token("account-id"),
                "refresh_token": "refresh-token",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OpenAiChatGptOAuthProvider::with_auth_base_url(server.uri());
        let prompt = Mutex::new(None);
        let tokens = provider
            .login_device_code(|value| {
                *prompt.lock().unwrap() = Some(value.clone());
            })
            .await
            .unwrap();

        assert_eq!(tokens.access_token, "access-token");
        assert_eq!(tokens.account_id.as_deref(), Some("account-id"));
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(
            prompt.lock().unwrap().as_ref().unwrap().verification_url,
            format!("{}/codex/device", server.uri())
        );
    }

    #[tokio::test]
    async fn refresh_persists_rotated_token_and_revoke_prefers_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_json(serde_json::json!({
                "client_id": OPENAI_CHATGPT_OAUTH_CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": "old-refresh"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "rotated-refresh",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/revoke"))
            .and(body_json(serde_json::json!({
                "token": "old-refresh",
                "token_type_hint": "refresh_token",
                "client_id": OPENAI_CHATGPT_OAUTH_CLIENT_ID
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OpenAiChatGptOAuthProvider::with_auth_base_url(server.uri());
        let tokens = provider.refresh("old-refresh").await.unwrap();
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rotated-refresh"));
        provider
            .revoke(Some("access"), Some("old-refresh"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refresh_honors_request_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(250)))
            .mount(&server)
            .await;

        let provider = OpenAiChatGptOAuthProvider::with_auth_base_url(server.uri())
            .with_request_timeout(Duration::from_millis(10));
        let error = provider.refresh("refresh-token").await.unwrap_err();

        assert!(error.to_string().contains("failed to refresh"));
    }

    struct PendingOnce {
        requests: Mutex<usize>,
    }

    impl Respond for PendingOnce {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let mut requests = self.requests.lock().unwrap();
            *requests += 1;
            if *requests == 1 {
                ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "error": "authorization_pending"
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "authorization_code": "authorization-code",
                    "code_challenge": "challenge",
                    "code_verifier": "verifier"
                }))
            }
        }
    }

    #[tokio::test]
    async fn device_login_retries_pending_authorization() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/usercode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_auth_id": "device-id",
                "user_code": "ABCD-EFGH",
                "interval": "0"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/token"))
            .respond_with(PendingOnce {
                requests: Mutex::new(0),
            })
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-token",
                "id_token": id_token("account-id"),
                "refresh_token": "refresh-token",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        OpenAiChatGptOAuthProvider::with_auth_base_url(server.uri())
            .login_device_code(|_| {})
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn device_login_times_out_while_authorization_is_pending() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/usercode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_auth_id": "device-id",
                "user_code": "ABCD-EFGH",
                "interval": "1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/token"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": "authorization_pending"
            })))
            .mount(&server)
            .await;

        let provider = OpenAiChatGptOAuthProvider::with_auth_base_url(server.uri())
            .with_timeouts(Duration::from_millis(20), REVOKE_TIMEOUT);
        let error = provider
            .login_device_code(|_| {})
            .await
            .expect_err("pending device login should time out");
        assert!(error.to_string().contains("timed out"));
    }
}
