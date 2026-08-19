//! OAuth credential refresh for the ChatGPT-backed Codex Responses endpoint.

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::Builder;
use tokio::sync::Mutex;

use crate::auth::{AuthConfig, AuthFuture, AuthProvider};

pub(crate) const ACCESS_TOKEN_ENV: &str = "OPENAI_RESPONSES_ACCESS_TOKEN";
pub(crate) const REFRESH_TOKEN_ENV: &str = "OPENAI_RESPONSES_REFRESH_TOKEN";
pub(crate) const ACCOUNT_ID_ENV: &str = "OPENAI_RESPONSES_ACCOUNT_ID";
pub(crate) const BASE_URL_ENV: &str = "OPENAI_RESPONSES_BASE_URL";
pub(crate) const ENV_FILE_ENV: &str = "OPENAI_RESPONSES_ENV_FILE";
pub(crate) const OAUTH_CLIENT_ID_ENV: &str = "OPENAI_RESPONSES_OAUTH_CLIENT_ID";
pub(crate) const OAUTH_TOKEN_URL_ENV: &str = "OPENAI_RESPONSES_OAUTH_TOKEN_URL";

const DEFAULT_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REFRESH_WINDOW: Duration = Duration::from_secs(5 * 60);
const RECENT_REFRESH_WINDOW: Duration = Duration::from_secs(30);

/// A dynamic auth provider that refreshes Codex OAuth credentials as needed.
pub(crate) struct CodexOAuthAuthProvider {
    client: Client,
    account_id: String,
    base_url: Option<String>,
    env_file: Option<PathBuf>,
    oauth_client_id: String,
    oauth_token_url: String,
    refresh_enabled: bool,
    state: Mutex<TokenState>,
}

struct TokenState {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    last_refresh: Option<Instant>,
}

#[derive(Debug, Deserialize)]
struct TokenRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct OAuthErrorResponse {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

impl CodexOAuthAuthProvider {
    pub(crate) fn from_env() -> Result<Self> {
        let access_token = required_env(ACCESS_TOKEN_ENV)?;
        let account_id = required_env(ACCOUNT_ID_ENV)?;
        let refresh_token = optional_env(REFRESH_TOKEN_ENV);
        let base_url = optional_env(BASE_URL_ENV);
        let env_file = optional_env(ENV_FILE_ENV).map(PathBuf::from);
        let oauth_client_id = optional_env(OAUTH_CLIENT_ID_ENV)
            .unwrap_or_else(|| DEFAULT_OAUTH_CLIENT_ID.to_string());
        let oauth_token_url = optional_env(OAUTH_TOKEN_URL_ENV)
            .unwrap_or_else(|| DEFAULT_OAUTH_TOKEN_URL.to_string());

        Ok(Self::new(
            Client::new(),
            access_token,
            refresh_token,
            account_id,
            base_url,
            env_file,
            oauth_client_id,
            oauth_token_url,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        client: Client,
        access_token: String,
        refresh_token: Option<String>,
        account_id: String,
        base_url: Option<String>,
        env_file: Option<PathBuf>,
        oauth_client_id: String,
        oauth_token_url: String,
    ) -> Self {
        let expires_at = jwt_expiry(&access_token);
        let refresh_enabled = refresh_token.is_some();
        Self {
            client,
            account_id,
            base_url,
            env_file,
            oauth_client_id,
            oauth_token_url,
            refresh_enabled,
            state: Mutex::new(TokenState {
                access_token,
                refresh_token,
                expires_at,
                last_refresh: None,
            }),
        }
    }

    async fn credentials(&self, force_refresh: bool) -> Result<AuthConfig> {
        let mut state = self.state.lock().await;
        let token_expiring = state
            .expires_at
            .is_some_and(|expires_at| expires_at <= unix_time() + REFRESH_WINDOW.as_secs());
        let recently_refreshed = state
            .last_refresh
            .is_some_and(|refreshed| refreshed.elapsed() < RECENT_REFRESH_WINDOW);
        let should_refresh = (token_expiring || force_refresh) && !recently_refreshed;

        if should_refresh {
            let refresh_token = state.refresh_token.clone().with_context(|| {
                if token_expiring {
                    format!(
                        "{ACCESS_TOKEN_ENV} is expired or near expiry and {REFRESH_TOKEN_ENV} is not set"
                    )
                } else {
                    format!("cannot refresh rejected Codex credentials without {REFRESH_TOKEN_ENV}")
                }
            })?;
            self.refresh_locked(&mut state, refresh_token).await?;
        }

        Ok(self.auth_config(&state.access_token))
    }

    async fn refresh_locked(&self, state: &mut TokenState, refresh_token: String) -> Result<()> {
        let response = self
            .client
            .post(&self.oauth_token_url)
            .form(&[
                ("client_id", self.oauth_client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
            ])
            .send()
            .await
            .context("failed to send Codex OAuth token refresh request")?;

        let status = response.status();
        if !status.is_success() {
            let error = response
                .json::<OAuthErrorResponse>()
                .await
                .unwrap_or_default();
            let detail = match (error.error.is_empty(), error.error_description.is_empty()) {
                (false, false) => format!("{}: {}", error.error, error.error_description),
                (false, true) => error.error,
                _ => "OAuth authority rejected the refresh request".to_string(),
            };
            anyhow::bail!("Codex OAuth token refresh failed ({status}): {detail}");
        }

        let refreshed = response
            .json::<TokenRefreshResponse>()
            .await
            .context("failed to parse Codex OAuth token refresh response")?;
        if refreshed.access_token.trim().is_empty() {
            anyhow::bail!("Codex OAuth token refresh returned an empty access token");
        }

        let next_refresh_token = refreshed
            .refresh_token
            .filter(|token| !token.trim().is_empty())
            .unwrap_or(refresh_token);
        let expires_at = jwt_expiry(&refreshed.access_token).or_else(|| {
            refreshed
                .expires_in
                .map(|expires_in| unix_time().saturating_add(expires_in))
        });

        state.access_token = refreshed.access_token;
        state.refresh_token = Some(next_refresh_token);
        state.expires_at = expires_at;
        state.last_refresh = Some(Instant::now());

        if let Some(path) = self.env_file.clone() {
            let access_token = state.access_token.clone();
            let refresh_token = state.refresh_token.clone().unwrap_or_default();
            let persistence_path = path.clone();
            let persistence = tokio::task::spawn_blocking(move || {
                persist_tokens(&persistence_path, &access_token, &refresh_token)
            })
            .await
            .context("Codex credential persistence task failed")?;
            if let Err(error) = persistence {
                // Keep serving with the fresh in-memory token, but make the
                // durability failure visible without logging any credential.
                tracing::error!(
                    env_file = %path.display(),
                    error = %error,
                    "refreshed Codex credentials but could not persist them"
                );
            }
        } else {
            tracing::warn!(
                env_var = ENV_FILE_ENV,
                "refreshed Codex credentials in memory only; no env file is configured"
            );
        }

        Ok(())
    }

    fn auth_config(&self, access_token: &str) -> AuthConfig {
        let auth = match self.base_url.as_deref() {
            Some(base_url) => AuthConfig::with_base_url(access_token, base_url),
            None => AuthConfig::new(access_token),
        };
        auth.with_account_id(&self.account_id)
    }
}

impl AuthProvider for CodexOAuthAuthProvider {
    fn get_auth(&self) -> AuthFuture<'_> {
        Box::pin(self.credentials(false))
    }

    fn refresh_auth(&self) -> AuthFuture<'_> {
        Box::pin(self.credentials(true))
    }

    fn supports_refresh(&self) -> bool {
        self.refresh_enabled
    }
}

fn required_env(name: &str) -> Result<String> {
    env::var(name)
        .with_context(|| format!("{name} environment variable not set"))
        .and_then(|value| {
            if value.trim().is_empty() {
                anyhow::bail!("{name} environment variable is empty")
            } else {
                Ok(value)
            }
        })
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn jwt_expiry(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::URL_SAFE
                .decode(payload)
                .ok()
        })?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_u64()
}

fn persist_tokens(path: &Path, access_token: &str, refresh_token: &str) -> Result<()> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read env file metadata {}", path.display()))?;
    let updated = update_env_contents(&contents, access_token, refresh_token)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("OPENAI_RESPONSES_ENV_FILE must have a parent directory")?;

    let mut temp = Builder::new()
        .prefix(".omega-responses-auth-")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "failed to create temporary env file in {}",
                parent.display()
            )
        })?;
    temp.as_file()
        .set_permissions(metadata.permissions())
        .context("failed to preserve env file permissions")?;
    temp.write_all(updated.as_bytes())
        .context("failed to write refreshed credentials")?;
    temp.as_file()
        .sync_all()
        .context("failed to sync refreshed credentials")?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace env file {}", path.display()))?;

    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn update_env_contents(contents: &str, access_token: &str, refresh_token: &str) -> Result<String> {
    validate_env_value(ACCESS_TOKEN_ENV, access_token)?;
    validate_env_value(REFRESH_TOKEN_ENV, refresh_token)?;

    let replacements = [
        (ACCESS_TOKEN_ENV, quote_env_value(access_token)),
        (REFRESH_TOKEN_ENV, quote_env_value(refresh_token)),
    ];
    let mut found = [false; 2];
    let mut output = String::with_capacity(contents.len() + 256);

    for segment in contents.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        let key = assignment_key(line);
        let mut replaced = false;
        for (index, (target, value)) in replacements.iter().enumerate() {
            if key == Some(*target) {
                output.push_str(target);
                output.push('=');
                output.push_str(value);
                output.push_str(newline);
                found[index] = true;
                replaced = true;
                break;
            }
        }
        if !replaced {
            output.push_str(segment);
        }
    }

    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    for (index, (key, value)) in replacements.iter().enumerate() {
        if !found[index] {
            output.push_str(key);
            output.push('=');
            output.push_str(value);
            output.push('\n');
        }
    }
    Ok(output)
}

fn assignment_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    let key = key.trim();
    (!key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(key)
}

fn validate_env_value(name: &str, value: &str) -> Result<()> {
    if value.contains(['\0', '\n', '\r']) {
        anyhow::bail!("refreshed {name} contains characters unsafe for an EnvironmentFile")
    }
    Ok(())
}

fn quote_env_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn jwt_with_exp(exp: u64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.signature")
    }

    #[test]
    fn extracts_jwt_expiry() {
        assert_eq!(jwt_expiry(&jwt_with_exp(123_456)), Some(123_456));
        assert_eq!(jwt_expiry("opaque-token"), None);
    }

    #[test]
    fn updates_all_token_assignments_and_preserves_other_lines() {
        let original = concat!(
            "# credentials\n",
            "OPENAI_API_TYPE=responses\n",
            "OPENAI_RESPONSES_ACCESS_TOKEN=old-access\n",
            "  OPENAI_RESPONSES_REFRESH_TOKEN='old-refresh'\n",
            "OPENAI_RESPONSES_ACCESS_TOKEN=duplicate-old-access\n",
        );
        let updated = update_env_contents(original, "new-access", "new-refresh").unwrap();

        assert!(updated.contains("# credentials\n"));
        assert!(updated.contains("OPENAI_API_TYPE=responses\n"));
        assert_eq!(
            updated
                .lines()
                .filter(|line| line.starts_with(ACCESS_TOKEN_ENV))
                .count(),
            2
        );
        assert!(!updated.contains("old-access"));
        assert!(!updated.contains("old-refresh"));
        assert!(updated.contains(&format!(r#"{ACCESS_TOKEN_ENV}="new-access""#)));
        assert!(updated.contains(&format!(r#"{REFRESH_TOKEN_ENV}="new-refresh""#)));
    }

    #[test]
    fn appends_missing_token_assignments() {
        let updated = update_env_contents("OPENAI_API_TYPE=responses", "access", "refresh")
            .expect("env update should succeed");
        assert_eq!(
            updated,
            concat!(
                "OPENAI_API_TYPE=responses\n",
                "OPENAI_RESPONSES_ACCESS_TOKEN=\"access\"\n",
                "OPENAI_RESPONSES_REFRESH_TOKEN=\"refresh\"\n",
            )
        );
    }

    #[test]
    fn rejects_multiline_credentials() {
        assert!(update_env_contents("", "bad\ntoken", "refresh").is_err());
        assert!(update_env_contents("", "access", "bad\rrefresh").is_err());
    }

    #[tokio::test]
    async fn refreshes_and_persists_rotated_tokens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "refresh request ended before its headers");
                request.extend_from_slice(&chunk[..read]);
                if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap();
            while request.len() < header_end + content_length {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "refresh request ended before its body");
                request.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(
                &request[header_end..header_end.saturating_add(content_length)],
            )
            .into_owned();
            let response_body = serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            body
        });

        let directory = tempfile::tempdir().unwrap();
        let env_file = directory.path().join("omega.env");
        fs::write(
            &env_file,
            concat!(
                "OPENAI_RESPONSES_ACCESS_TOKEN=old-access\n",
                "OPENAI_RESPONSES_REFRESH_TOKEN=old-refresh\n",
                "OPENAI_RESPONSES_ACCOUNT_ID=account-123\n",
            ),
        )
        .unwrap();
        let provider = CodexOAuthAuthProvider::new(
            Client::new(),
            jwt_with_exp(unix_time().saturating_sub(60)),
            Some("old-refresh".to_string()),
            "account-123".to_string(),
            None,
            Some(env_file.clone()),
            "test-client".to_string(),
            format!("http://{address}/oauth/token"),
        );

        let auth = provider.get_auth().await.unwrap();
        assert_eq!(auth.api_key, "new-access");
        assert_eq!(auth.account_id.as_deref(), Some("account-123"));

        let request_body = server.await.unwrap();
        assert!(request_body.contains("client_id=test-client"));
        assert!(request_body.contains("grant_type=refresh_token"));
        assert!(request_body.contains("refresh_token=old-refresh"));

        let persisted = fs::read_to_string(env_file).unwrap();
        assert!(persisted.contains("OPENAI_RESPONSES_ACCESS_TOKEN=\"new-access\""));
        assert!(persisted.contains("OPENAI_RESPONSES_REFRESH_TOKEN=\"new-refresh\""));
    }

    #[cfg(unix)]
    #[test]
    fn atomically_persists_tokens_and_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("omega.env");
        fs::write(
            &path,
            concat!(
                "OPENAI_RESPONSES_ACCESS_TOKEN=old\n",
                "OPENAI_RESPONSES_REFRESH_TOKEN=old\n",
                "OPENAI_MODEL=gpt-test\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        persist_tokens(&path, "new-access", "new-refresh").unwrap();

        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("OPENAI_RESPONSES_ACCESS_TOKEN=\"new-access\""));
        assert!(updated.contains("OPENAI_RESPONSES_REFRESH_TOKEN=\"new-refresh\""));
        assert!(updated.contains("OPENAI_MODEL=gpt-test"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
