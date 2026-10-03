use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PGBHubError {
    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error [{status}]: {message}")]
    Api { status: u16, message: String },
    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct PGBHubConfig {
    pub endpoint: String,
    pub api_key: Option<String>,
    pub database: String,
    /// Required by QueuesClient, whose routes are addressed by project
    /// rather than database.
    pub project_id: Option<String>,
}

impl Default for PGBHubConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://cloud.pgbhub.com".to_string(),
            api_key: std::env::var("PGBHUB_API_KEY").ok(),
            database: "postgres".to_string(),
            project_id: std::env::var("PGBHUB_PROJECT_ID").ok(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResponse {
    pub action: String,
    pub status: String,
    pub dry_run: bool,
    pub duration_ms: f64,
    pub results: serde_json::Value,
    pub outbox_id: Option<i64>,
    pub message: Option<String>,
}

pub struct PGBHubClient {
    config: PGBHubConfig,
    http: HttpClient,

    /// Built-In PGB Auth-as-a-Service client.
    pub auth: AuthClient,
    /// Covers file/media download (upload/list/delete are not yet
    /// exposed in the Rust SDK -- tracked as follow-up breadth work).
    pub storage: StorageClient,
    /// Covers deploy/delete/invoke/list of serverless cloud functions.
    pub functions: FunctionsClient,
    /// Covers scheduled job management, addressed by database name.
    pub cron: CronClient,
    /// Covers the on-demand asynchronous background job queue.
    pub jobs: JobsClient,
    /// Covers durable message queues (send/receive/ack/nack).
    pub queues: QueuesClient,
    /// Covers broadcast/subscribe channels.
    pub realtime: RealtimeClient,
}

impl PGBHubClient {
    pub fn new(config: PGBHubConfig) -> Self {
        let endpoint = config.endpoint.trim_end_matches('/').to_string();
        let http = HttpClient::new();
        let auth = AuthClient::new(endpoint.clone(), config.api_key.clone(), http.clone());
        let storage = StorageClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            http: http.clone(),
        };
        let functions = FunctionsClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            database: config.database.clone(),
            http: http.clone(),
        };
        let cron = CronClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            database: config.database.clone(),
            http: http.clone(),
        };
        let jobs = JobsClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            database: config.database.clone(),
            http: http.clone(),
        };
        let queues = QueuesClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            project_id: config.project_id.clone(),
            http: http.clone(),
        };
        let realtime = RealtimeClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            http: http.clone(),
        };
        Self {
            config,
            http,
            auth,
            storage,
            functions,
            cron,
            jobs,
            queues,
            realtime,
        }
    }

    pub async fn get_schema(&self) -> Result<serde_json::Value, PGBHubError> {
        let url = format!(
            "{}/api/v1/databases/{}/schema.json",
            self.config.endpoint.trim_end_matches('/'),
            self.config.database
        );

        let mut req = self.http.get(&url).header("Accept", "application/json");
        if let Some(ref key) = self.config.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let msg = resp.text().await.unwrap_or_default();
            return Err(PGBHubError::Api {
                status,
                message: msg,
            });
        }

        Ok(resp.json::<serde_json::Value>().await?)
    }

    pub async fn invoke_action(
        &self,
        action: &str,
        params: HashMap<String, serde_json::Value>,
        dry_run: bool,
    ) -> Result<ActionResponse, PGBHubError> {
        let url = format!(
            "{}/v1/actions/invoke",
            self.config.endpoint.trim_end_matches('/')
        );

        let body = serde_json::json!({
            "action": action,
            "params": params,
            "dry_run": dry_run,
            "dbname": self.config.database
        });

        let mut req = self.http.post(&url).json(&body);
        if let Some(ref key) = self.config.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let msg = resp.text().await.unwrap_or_default();
            return Err(PGBHubError::Api {
                status,
                message: msg,
            });
        }

        Ok(resp.json::<ActionResponse>().await?)
    }

    /// Provisions a new PGBHub-managed PostgreSQL database.
    ///
    /// Requires an account-level API key (scope_type "account", not a
    /// project-scoped pgb_sec_/pgb_pub_ key) -- a project key is
    /// permanently bound to the one database it was issued for and the
    /// server rejects (403) any attempt to use it to provision an
    /// unrelated new one.
    pub async fn create_database(
        &self,
        name: &str,
        options: Option<CreateDatabaseOptions>,
    ) -> Result<CreateDatabaseResult, PGBHubError> {
        let opts = options.unwrap_or_default();
        let pg_version = opts.pg_version.unwrap_or_else(|| "17".to_string());
        let url = format!(
            "{}/api/v1/databases",
            self.config.endpoint.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "name": name,
            "pg_version": pg_version,
            "extensions": opts.extensions,
        });
        request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.config.api_key,
            &[],
            Some(body),
        )
        .await
    }
}

/// Shared HTTP helper used by AuthClient/AuthAdminClient/FunctionsClient/
/// create_database -- sends an authenticated JSON request and decodes a
/// 2xx JSON response into `T`, or returns PGBHubError::Api with the
/// server's own parsed error message on a non-2xx response.
async fn request_json<T: serde::de::DeserializeOwned>(
    http: &HttpClient,
    method: reqwest::Method,
    url: &str,
    api_key: &Option<String>,
    extra_headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> Result<T, PGBHubError> {
    let mut req = http
        .request(method, url)
        .header("Accept", "application/json");
    if let Some(ref key) = api_key {
        req = req.header("Authorization", format!("Bearer {}", key));
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    if let Some(ref b) = body {
        req = req.json(b);
    }

    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;

    if !status.is_success() {
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or(text);
        return Err(PGBHubError::Api {
            status: status.as_u16(),
            message,
        });
    }

    Ok(serde_json::from_str(&text)?)
}

// ─── Auth-as-a-Service ──────────────────────────────────────────────────

/// A user registered in a project's Built-In PGB Auth.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub is_verified: bool,
    #[serde(default)]
    pub raw_user_meta: serde_json::Value,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// An access/refresh token pair issued to an end user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Returned by sign_up, verify_otp, verify_magic_link, and refresh_session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResult {
    pub user: Option<AuthUser>,
    pub session: Option<AuthSession>,
}

/// Returned by sign_in. `session` is None and `mfa_required` is true
/// when the account has MFA enabled -- redeem `challenge_token` via a
/// future complete_mfa_login call.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoginResult {
    #[serde(default)]
    pub user: Option<AuthUser>,
    #[serde(default)]
    pub session: Option<AuthSession>,
    #[serde(default)]
    pub mfa_required: bool,
    #[serde(default)]
    pub challenge_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

/// Returned by request_otp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtpRequestResult {
    pub message: String,
    /// Only populated in sandbox/dev deployments where the server
    /// returns it directly instead of only emailing it.
    #[serde(default)]
    pub otp: Option<String>,
    pub expires_in: i64,
}

/// Returned by send_magic_link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MagicLinkRequestResult {
    pub message: String,
    /// Only populated in sandbox/dev deployments where the server
    /// returns it directly instead of only emailing it.
    #[serde(default)]
    pub token: Option<String>,
    pub expires_in: i64,
}

#[derive(Deserialize)]
struct UserEnvelope {
    user: Option<AuthUser>,
}

#[derive(Deserialize)]
struct UsersEnvelope {
    users: Vec<AuthUser>,
}

#[derive(Deserialize)]
struct SuccessEnvelope {
    success: bool,
}

/// Built-In PGB Auth-as-a-Service client (`client.auth`).
///
/// Two different bearer tokens are in play: the client's own API key (a
/// project Secret/Publishable key) identifies the *project*, sent as
/// `Authorization: Bearer <key>` on every call; the *end user's* own
/// access token (held in the session set by sign_up/sign_in/set_session)
/// is sent separately as `X-PGB-User-Token` on the routes that need to
/// know who's calling (get_user, sign_out).
pub struct AuthClient {
    endpoint: String,
    api_key: Option<String>,
    http: HttpClient,
    session: std::sync::Mutex<Option<AuthSession>>,
    /// Secret-key-only user management.
    pub admin: AuthAdminClient,
}

impl AuthClient {
    fn new(endpoint: String, api_key: Option<String>, http: HttpClient) -> Self {
        let admin = AuthAdminClient {
            endpoint: endpoint.clone(),
            api_key: api_key.clone(),
            http: http.clone(),
        };
        Self {
            endpoint,
            api_key,
            http,
            session: std::sync::Mutex::new(None),
            admin,
        }
    }

    /// Signs up a new user with email & password.
    pub async fn sign_up(
        &self,
        email: &str,
        password: Option<&str>,
        metadata: Option<serde_json::Value>,
    ) -> Result<AuthResult, PGBHubError> {
        let url = format!("{}/auth/signup", self.endpoint);
        let body = serde_json::json!({
            "email": email,
            "password": password,
            "metadata": metadata.unwrap_or_else(|| serde_json::json!({})),
        });
        let result: AuthResult = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        *self.session.lock().unwrap() = result.session.clone();
        Ok(result)
    }

    /// Signs in with email & password.
    pub async fn sign_in(
        &self,
        email: &str,
        password: Option<&str>,
    ) -> Result<LoginResult, PGBHubError> {
        let url = format!("{}/auth/login", self.endpoint);
        let body = serde_json::json!({"email": email, "password": password});
        let result: LoginResult = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        if result.session.is_some() {
            *self.session.lock().unwrap() = result.session.clone();
        }
        Ok(result)
    }

    /// Signs out and invalidates the current session. A no-op if there
    /// is no active session.
    pub async fn sign_out(&self) -> Result<(), PGBHubError> {
        let token = {
            self.session
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.access_token.clone())
        };
        if let Some(token) = token {
            let url = format!("{}/auth/logout", self.endpoint);
            let _ = request_json::<serde_json::Value>(
                &self.http,
                reqwest::Method::POST,
                &url,
                &self.api_key,
                &[("X-PGB-User-Token", token.as_str())],
                Some(serde_json::json!({})),
            )
            .await;
        }
        *self.session.lock().unwrap() = None;
        Ok(())
    }

    /// Returns the current authenticated user's details, or `None`
    /// without a network call if there is no active session.
    pub async fn get_user(&self) -> Result<Option<AuthUser>, PGBHubError> {
        let token = {
            self.session
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.access_token.clone())
        };
        let token = match token {
            Some(t) => t,
            None => return Ok(None),
        };
        let url = format!("{}/auth/user", self.endpoint);
        let result: UserEnvelope = request_json(
            &self.http,
            reqwest::Method::GET,
            &url,
            &self.api_key,
            &[("X-PGB-User-Token", token.as_str())],
            None,
        )
        .await?;
        Ok(result.user)
    }

    /// Returns the current active session, if any.
    pub fn get_session(&self) -> Option<AuthSession> {
        self.session.lock().unwrap().clone()
    }

    /// Sets the active session.
    pub fn set_session(&self, session: Option<AuthSession>) {
        *self.session.lock().unwrap() = session;
    }

    /// Exchanges a refresh token for a new session, rotating the refresh
    /// token on every use. Pass `None` to use the current session's
    /// refresh token.
    pub async fn refresh_session(
        &self,
        refresh_token: Option<&str>,
    ) -> Result<AuthResult, PGBHubError> {
        let token = match refresh_token {
            Some(t) => t.to_string(),
            None => {
                let current = self
                    .session
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|s| s.refresh_token.clone());
                current.ok_or_else(|| PGBHubError::Api {
                    status: 0,
                    message: "refresh_session requires a refresh token (none provided and no active session)".to_string(),
                })?
            }
        };
        let url = format!("{}/auth/refresh", self.endpoint);
        let body = serde_json::json!({"refresh_token": token});
        let result: AuthResult = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        *self.session.lock().unwrap() = result.session.clone();
        Ok(result)
    }

    /// Requests a one-time passcode for passwordless sign-in.
    pub async fn request_otp(&self, email: &str) -> Result<OtpRequestResult, PGBHubError> {
        let url = format!("{}/auth/otp", self.endpoint);
        request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(serde_json::json!({"email": email})),
        )
        .await
    }

    /// Verifies a one-time passcode and establishes a session.
    pub async fn verify_otp(&self, email: &str, token: &str) -> Result<AuthResult, PGBHubError> {
        let url = format!("{}/auth/otp/verify", self.endpoint);
        let body = serde_json::json!({"email": email, "token": token});
        let result: AuthResult = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        *self.session.lock().unwrap() = result.session.clone();
        Ok(result)
    }

    /// Sends a passwordless magic link.
    pub async fn send_magic_link(
        &self,
        email: &str,
    ) -> Result<MagicLinkRequestResult, PGBHubError> {
        let url = format!("{}/auth/magic", self.endpoint);
        request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(serde_json::json!({"email": email})),
        )
        .await
    }

    /// Verifies a magic link token and establishes a session.
    pub async fn verify_magic_link(&self, token: &str) -> Result<AuthResult, PGBHubError> {
        let url = format!("{}/auth/magic/verify", self.endpoint);
        let body = serde_json::json!({"token": token});
        let result: AuthResult = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        *self.session.lock().unwrap() = result.session.clone();
        Ok(result)
    }
}

/// Secret-key-only user management (`client.auth.admin`), mirroring
/// pgb-cloud's /auth/admin/users CRUD exactly.
pub struct AuthAdminClient {
    endpoint: String,
    api_key: Option<String>,
    http: HttpClient,
}

impl AuthAdminClient {
    /// Creates a user directly (bypasses signup email verification).
    pub async fn create_user(
        &self,
        email: &str,
        password: Option<&str>,
        metadata: Option<serde_json::Value>,
        is_verified: bool,
    ) -> Result<AuthUser, PGBHubError> {
        let url = format!("{}/auth/admin/users", self.endpoint);
        let body = serde_json::json!({
            "email": email,
            "password": password,
            "metadata": metadata.unwrap_or_else(|| serde_json::json!({})),
            "is_verified": is_verified,
        });
        #[derive(Deserialize)]
        struct Resp {
            user: AuthUser,
        }
        let result: Resp = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        Ok(result.user)
    }

    /// Lists users registered in this project.
    pub async fn list_users(&self) -> Result<Vec<AuthUser>, PGBHubError> {
        let url = format!("{}/auth/admin/users", self.endpoint);
        let result: UsersEnvelope = request_json(
            &self.http,
            reqwest::Method::GET,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(result.users)
    }

    /// Fetches a single user by ID, or `None` if not found.
    pub async fn get_user(&self, user_id: &str) -> Result<Option<AuthUser>, PGBHubError> {
        let url = format!("{}/auth/admin/users/{}", self.endpoint, user_id);
        match request_json::<UserEnvelope>(
            &self.http,
            reqwest::Method::GET,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await
        {
            Ok(r) => Ok(r.user),
            Err(PGBHubError::Api { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Updates a user's password, metadata, or verification status.
    /// `updates` should be a JSON object with any of "password",
    /// "metadata", "is_verified".
    pub async fn update_user(
        &self,
        user_id: &str,
        updates: serde_json::Value,
    ) -> Result<AuthUser, PGBHubError> {
        let url = format!("{}/auth/admin/users/{}", self.endpoint, user_id);
        #[derive(Deserialize)]
        struct Resp {
            user: AuthUser,
        }
        let result: Resp = request_json(
            &self.http,
            reqwest::Method::PATCH,
            &url,
            &self.api_key,
            &[],
            Some(updates),
        )
        .await?;
        Ok(result.user)
    }

    /// Deletes a user and invalidates their sessions.
    pub async fn delete_user(&self, user_id: &str) -> Result<bool, PGBHubError> {
        let url = format!("{}/auth/admin/users/{}", self.endpoint, user_id);
        let result: SuccessEnvelope = request_json(
            &self.http,
            reqwest::Method::DELETE,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(result.success)
    }
}

// ─── Storage ─────────────────────────────────────────────────────────────

/// A downloaded file's raw bytes and metadata.
#[derive(Debug, Clone)]
pub struct StorageFile {
    pub data: Vec<u8>,
    pub content_type: String,
    pub path: String,
    pub etag: Option<String>,
}

/// Accepted by `StorageClient::get_media` for forward compatibility
/// only -- see its doc comment.
#[derive(Debug, Clone, Default)]
pub struct MediaTransformOptions {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
    pub fit: Option<String>,
}

/// Covers file/media download (`client.storage`). Upload/list/delete
/// are not yet exposed in the Rust SDK -- tracked as follow-up breadth
/// work, matching the TS SDK's existing coverage.
pub struct StorageClient {
    endpoint: String,
    api_key: Option<String>,
    http: HttpClient,
}

impl StorageClient {
    /// Downloads a stored file's raw bytes.
    pub async fn get_file(&self, file_path: &str) -> Result<StorageFile, PGBHubError> {
        let clean_path = file_path.trim_start_matches('/');
        let url = format!("{}/storage/{}", self.endpoint, clean_path);
        let mut req = self.http.get(&url).header("Accept", "*/*");
        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
        let resp = req.send().await?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(PGBHubError::Api {
                status: status.as_u16(),
                message: text,
            });
        }

        let data = resp.bytes().await?.to_vec();
        let clean_path_owned = clean_path.to_string();
        Ok(StorageFile {
            data,
            content_type,
            path: clean_path_owned,
            etag,
        })
    }

    /// Downloads a stored image.
    ///
    /// The server does not implement on-the-fly image transforms yet
    /// (no width/height/format handling exists in its storage delivery
    /// route) -- `_options` is accepted here for forward compatibility
    /// only and currently has no effect. This is a thin alias over
    /// `get_file`; do not rely on resizing/format conversion actually
    /// happening until PGBHub ships real transform support.
    pub async fn get_media(
        &self,
        file_path: &str,
        _options: Option<MediaTransformOptions>,
    ) -> Result<StorageFile, PGBHubError> {
        self.get_file(file_path).await
    }
}

// ─── Functions ───────────────────────────────────────────────────────────

/// A serverless cloud function's stored configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudFunction {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub trigger_type: Option<String>,
    #[serde(default)]
    pub is_enabled: Option<bool>,
    #[serde(default)]
    pub timeout_ms: Option<i64>,
    #[serde(default)]
    pub memory_mb: Option<i64>,
}

/// Optional fields `FunctionsClient::deploy` accepts beyond name/code.
/// Every field left `None` either defaults server-side or keeps the
/// existing function's value on an update.
#[derive(Debug, Clone, Default)]
pub struct DeployFunctionOptions {
    pub title: Option<String>,
    pub description: Option<String>,
    pub runtime: Option<String>,
    pub entrypoint: Option<String>,
    pub trigger_type: Option<String>,
    pub trigger_config: Option<serde_json::Value>,
    pub env_vars: Option<serde_json::Value>,
    pub is_enabled: Option<bool>,
    pub timeout_ms: Option<i64>,
    pub memory_mb: Option<i64>,
}

/// Covers deploy/delete/invoke/list of serverless cloud functions
/// (`client.functions`).
pub struct FunctionsClient {
    endpoint: String,
    api_key: Option<String>,
    database: String,
    http: HttpClient,
}

impl FunctionsClient {
    /// Creates or updates a function's code and configuration.
    ///
    /// The server's save-function logic is an upsert regardless of
    /// which route calls it, so this always POSTs to the functions
    /// collection endpoint (name in the body, not the URL) rather than
    /// needing to check existence first and switch between POST/PUT.
    pub async fn deploy(
        &self,
        name: &str,
        code: &str,
        options: Option<DeployFunctionOptions>,
    ) -> Result<CloudFunction, PGBHubError> {
        let url = format!(
            "{}/api/v1/databases/{}/functions",
            self.endpoint, self.database
        );
        let mut body = serde_json::json!({"name": name, "code": code});
        if let Some(opts) = options {
            let obj = body.as_object_mut().expect("body is always a JSON object");
            if let Some(v) = opts.title {
                obj.insert("title".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.description {
                obj.insert("description".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.runtime {
                obj.insert("runtime".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.entrypoint {
                obj.insert("entrypoint".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.trigger_type {
                obj.insert("trigger_type".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.trigger_config {
                obj.insert("trigger_config".into(), v);
            }
            if let Some(v) = opts.env_vars {
                obj.insert("env_vars".into(), v);
            }
            if let Some(v) = opts.is_enabled {
                obj.insert("is_enabled".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.timeout_ms {
                obj.insert("timeout_ms".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.memory_mb {
                obj.insert("memory_mb".into(), serde_json::json!(v));
            }
        }
        #[derive(Deserialize)]
        struct Resp {
            function: CloudFunction,
        }
        let result: Resp = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        Ok(result.function)
    }

    /// Deletes a cloud function.
    pub async fn delete(&self, name: &str) -> Result<bool, PGBHubError> {
        let url = format!(
            "{}/api/v1/databases/{}/functions/{}",
            self.endpoint, self.database, name
        );
        let result: SuccessEnvelope = request_json(
            &self.http,
            reqwest::Method::DELETE,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(result.success)
    }

    /// Lists all cloud functions configured for the target database.
    pub async fn list(&self) -> Result<Vec<CloudFunction>, PGBHubError> {
        let url = format!(
            "{}/api/v1/databases/{}/functions",
            self.endpoint, self.database
        );
        #[derive(Deserialize)]
        struct Resp {
            functions: Vec<CloudFunction>,
        }
        let result: Resp = request_json(
            &self.http,
            reqwest::Method::GET,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(result.functions)
    }

    /// Invokes a serverless cloud function via its database-scoped
    /// route, returning the raw JSON response.
    pub async fn invoke(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, PGBHubError> {
        let url = format!(
            "{}/api/v1/databases/{}/functions/{}/invoke",
            self.endpoint, self.database, name
        );
        request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(payload),
        )
        .await
    }
}

// ─── Database provisioning ─────────────────────────────────────────────

/// Optional fields `PGBHubClient::create_database` accepts.
#[derive(Debug, Clone, Default)]
pub struct CreateDatabaseOptions {
    pub pg_version: Option<String>,
    pub extensions: Option<Vec<String>>,
}

/// Returned by `PGBHubClient::create_database`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDatabaseResult {
    pub success: bool,
    pub database: String,
    pub pg_version: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    pub user: String,
    pub password: String,
    pub public_host: String,
    pub public_port: i64,
    pub url: String,
    pub message: String,
}

// ─── Cron ──────────────────────────────────────────────────────────────

/// A scheduled cron job's stored configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub expression: String,
    #[serde(default)]
    pub timezone: Option<String>,
    pub target_type: String,
    #[serde(default)]
    pub target_config: serde_json::Value,
    #[serde(default)]
    pub is_enabled: bool,
    #[serde(default)]
    pub concurrency_policy: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<i64>,
    #[serde(default)]
    pub max_retries: Option<i64>,
    #[serde(default)]
    pub last_run_at: Option<String>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub next_run_at: Option<String>,
}

/// One execution record for a cron job.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronRun {
    pub id: String,
    pub cron_id: String,
    pub status: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Optional fields `CronClient::create` accepts beyond name/expression/
/// target_type/target_config.
#[derive(Debug, Clone, Default)]
pub struct CreateCronJobOptions {
    pub description: Option<String>,
    pub timezone: Option<String>,
    pub is_enabled: Option<bool>,
    pub concurrency_policy: Option<String>,
    pub timeout_seconds: Option<i64>,
    pub max_retries: Option<i64>,
    pub retry_delay_seconds: Option<i64>,
}

/// Optional fields `CronClient::update` accepts.
#[derive(Debug, Clone, Default)]
pub struct UpdateCronJobFields {
    pub name: Option<String>,
    pub description: Option<String>,
    pub expression: Option<String>,
    pub timezone: Option<String>,
    pub concurrency_policy: Option<String>,
    pub timeout_seconds: Option<i64>,
    pub max_retries: Option<i64>,
    pub target_type: Option<String>,
    pub target_config: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CronsEnvelope {
    crons: Vec<CronJob>,
}

#[derive(Deserialize)]
struct CronRunsEnvelope {
    runs: Vec<CronRun>,
}

/// Scheduled (cron) job management (`client.cron`), addressed by
/// database name. target_config's shape depends on target_type:
/// function -> {function_name, payload}, sql -> {sql},
/// action -> {action_name, params}, http -> {url, method, body?},
/// mail -> {to, subject, body}.
pub struct CronClient {
    endpoint: String,
    api_key: Option<String>,
    database: String,
    http: HttpClient,
}

impl CronClient {
    fn base(&self) -> String {
        format!("{}/api/v1/databases/{}/crons", self.endpoint, self.database)
    }

    /// Lists scheduled cron jobs.
    pub async fn list(&self) -> Result<Vec<CronJob>, PGBHubError> {
        let result: CronsEnvelope = request_json(
            &self.http,
            reqwest::Method::GET,
            &self.base(),
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(result.crons)
    }

    /// Creates a scheduled cron job.
    pub async fn create(
        &self,
        name: &str,
        expression: &str,
        target_type: &str,
        target_config: serde_json::Value,
        options: Option<CreateCronJobOptions>,
    ) -> Result<CronJob, PGBHubError> {
        let mut body = serde_json::json!({
            "name": name,
            "expression": expression,
            "target_type": target_type,
            "target_config": target_config,
        });
        if let Some(opts) = options {
            let obj = body.as_object_mut().expect("body is always a JSON object");
            if let Some(v) = opts.description {
                obj.insert("description".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.timezone {
                obj.insert("timezone".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.is_enabled {
                obj.insert("is_enabled".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.concurrency_policy {
                obj.insert("concurrency_policy".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.timeout_seconds {
                obj.insert("timeout_seconds".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.max_retries {
                obj.insert("max_retries".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.retry_delay_seconds {
                obj.insert("retry_delay_seconds".into(), serde_json::json!(v));
            }
        }
        request_json(
            &self.http,
            reqwest::Method::POST,
            &self.base(),
            &self.api_key,
            &[],
            Some(body),
        )
        .await
    }

    /// Enables or disables a cron job.
    pub async fn toggle(&self, cron_id: &str, enabled: bool) -> Result<CronJob, PGBHubError> {
        let url = format!("{}/{}/toggle", self.base(), cron_id);
        let body = serde_json::json!({"enabled": enabled});
        request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await
    }

    /// Manually triggers a cron job to run immediately.
    pub async fn run(&self, cron_id: &str) -> Result<serde_json::Value, PGBHubError> {
        let url = format!("{}/{}/run", self.base(), cron_id);
        request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await
    }

    /// Deletes a cron job.
    pub async fn delete(&self, cron_id: &str) -> Result<(), PGBHubError> {
        let url = format!("{}/{}", self.base(), cron_id);
        let _: serde_json::Value = request_json(
            &self.http,
            reqwest::Method::DELETE,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(())
    }

    /// Lists recent run history for a cron job.
    pub async fn list_runs(
        &self,
        cron_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<CronRun>, PGBHubError> {
        let url = format!(
            "{}/{}/runs?limit={}",
            self.base(),
            cron_id,
            limit.unwrap_or(50)
        );
        let result: CronRunsEnvelope = request_json(
            &self.http,
            reqwest::Method::GET,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(result.runs)
    }

    /// Updates a cron job's configuration.
    pub async fn update(
        &self,
        cron_id: &str,
        fields: UpdateCronJobFields,
    ) -> Result<CronJob, PGBHubError> {
        let mut body = serde_json::Map::new();
        if let Some(v) = fields.name {
            body.insert("name".into(), serde_json::json!(v));
        }
        if let Some(v) = fields.description {
            body.insert("description".into(), serde_json::json!(v));
        }
        if let Some(v) = fields.expression {
            body.insert("expression".into(), serde_json::json!(v));
        }
        if let Some(v) = fields.timezone {
            body.insert("timezone".into(), serde_json::json!(v));
        }
        if let Some(v) = fields.concurrency_policy {
            body.insert("concurrency_policy".into(), serde_json::json!(v));
        }
        if let Some(v) = fields.timeout_seconds {
            body.insert("timeout_seconds".into(), serde_json::json!(v));
        }
        if let Some(v) = fields.max_retries {
            body.insert("max_retries".into(), serde_json::json!(v));
        }
        if let Some(v) = fields.target_type {
            body.insert("target_type".into(), serde_json::json!(v));
        }
        if let Some(v) = fields.target_config {
            body.insert("target_config".into(), v);
        }
        let url = format!("{}/{}", self.base(), cron_id);
        request_json(
            &self.http,
            reqwest::Method::PATCH,
            &url,
            &self.api_key,
            &[],
            Some(serde_json::Value::Object(body)),
        )
        .await
    }
}

// ─── Jobs ──────────────────────────────────────────────────────────────

/// An on-demand asynchronous background job.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackgroundJob {
    pub id: i64,
    pub job_type: String,
    pub target_type: String,
    #[serde(default)]
    pub target_config: serde_json::Value,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub status: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub attempts: i64,
    #[serde(default)]
    pub max_attempts: i64,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Optional fields `JobsClient::enqueue` accepts.
#[derive(Debug, Clone, Default)]
pub struct EnqueueJobOptions {
    pub payload: Option<serde_json::Value>,
    pub idempotency_key: Option<String>,
    pub max_attempts: Option<i64>,
}

/// Filters `JobsClient::list`.
#[derive(Debug, Clone, Default)]
pub struct ListJobsOptions {
    pub status: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
struct JobEnvelope {
    job: BackgroundJob,
}

#[derive(Deserialize)]
struct JobsEnvelope {
    jobs: Vec<BackgroundJob>,
}

#[derive(Deserialize)]
struct CancelEnvelope {
    cancelled: bool,
}

/// On-demand asynchronous background job queue (`client.jobs`),
/// addressed by database name. Execution happens out-of-band in the
/// server's own jobs scheduler -- these methods only enqueue/inspect/
/// cancel.
pub struct JobsClient {
    endpoint: String,
    api_key: Option<String>,
    database: String,
    http: HttpClient,
}

impl JobsClient {
    fn base(&self) -> String {
        format!("{}/api/v1/databases/{}/jobs", self.endpoint, self.database)
    }

    /// Enqueues a new background job.
    pub async fn enqueue(
        &self,
        job_type: &str,
        target_type: &str,
        target_config: serde_json::Value,
        options: Option<EnqueueJobOptions>,
    ) -> Result<BackgroundJob, PGBHubError> {
        let mut body = serde_json::json!({"job_type": job_type, "target_type": target_type, "target_config": target_config});
        if let Some(opts) = options {
            let obj = body.as_object_mut().expect("body is always a JSON object");
            if let Some(v) = opts.payload {
                obj.insert("payload".into(), v);
            }
            if let Some(v) = opts.idempotency_key {
                obj.insert("idempotency_key".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.max_attempts {
                obj.insert("max_attempts".into(), serde_json::json!(v));
            }
        }
        let result: JobEnvelope = request_json(
            &self.http,
            reqwest::Method::POST,
            &self.base(),
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        Ok(result.job)
    }

    /// Fetches a single job by ID, or `None` if not found.
    pub async fn get(&self, job_id: i64) -> Result<Option<BackgroundJob>, PGBHubError> {
        let url = format!("{}/{}", self.base(), job_id);
        match request_json::<JobEnvelope>(
            &self.http,
            reqwest::Method::GET,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await
        {
            Ok(r) => Ok(Some(r.job)),
            Err(PGBHubError::Api { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Lists jobs, optionally filtered by status.
    pub async fn list(
        &self,
        options: Option<ListJobsOptions>,
    ) -> Result<Vec<BackgroundJob>, PGBHubError> {
        let opts = options.unwrap_or_default();
        let mut url = format!("{}?limit={}", self.base(), opts.limit.unwrap_or(100));
        if let Some(status) = opts.status {
            url.push_str(&format!("&status={}", status));
        }
        let result: JobsEnvelope = request_json(
            &self.http,
            reqwest::Method::GET,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(result.jobs)
    }

    /// Cancels a pending/running job. Returns `Ok(false)` if it could
    /// not be cancelled (already running or finished) instead of an error.
    pub async fn cancel(&self, job_id: i64) -> Result<bool, PGBHubError> {
        let url = format!("{}/{}/cancel", self.base(), job_id);
        match request_json::<CancelEnvelope>(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            None,
        )
        .await
        {
            Ok(r) => Ok(r.cancelled),
            Err(PGBHubError::Api { status: 409, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

// ─── Queues ──────────────────────────────────────────────────────────────

/// Returned by `QueuesClient::send`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SentMessage {
    pub message_id: String,
    pub queue_name: String,
    pub status: String,
    #[serde(default)]
    pub scheduled_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub deduplicated: bool,
}

/// One message returned by `QueuesClient::receive`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReceivedMessage {
    pub message_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Optional fields `QueuesClient::send` accepts.
#[derive(Debug, Clone, Default)]
pub struct SendMessageOptions {
    pub delay_seconds: Option<i64>,
    pub deduplication_id: Option<String>,
}

/// Optional fields `QueuesClient::receive` accepts.
#[derive(Debug, Clone, Default)]
pub struct ReceiveMessagesOptions {
    pub max_messages: Option<i64>,
    pub visibility_timeout: Option<i64>,
    pub consumer_id: Option<String>,
}

#[derive(Deserialize)]
struct SendEnvelope {
    message: SentMessage,
}

#[derive(Deserialize)]
struct ReceiveEnvelope {
    messages: Vec<ReceivedMessage>,
}

#[derive(Deserialize)]
struct OkEnvelope {
    ok: bool,
}

/// Durable message queues (`client.queues`). Unlike Cron/Jobs/
/// Functions, queue routes are addressed by project ID, not database
/// name (/api/v1/projects/{id}/queues/...) -- set
/// `PGBHubConfig::project_id` or `PGBHUB_PROJECT_ID`. Queue creation/
/// deletion is dashboard-only today (no API route); this client covers
/// send/receive/ack/nack against an already-created queue.
pub struct QueuesClient {
    endpoint: String,
    api_key: Option<String>,
    project_id: Option<String>,
    http: HttpClient,
}

impl QueuesClient {
    fn url(&self, queue_name: &str, action: &str) -> Result<String, PGBHubError> {
        let project_id = self.project_id.as_ref().ok_or_else(|| PGBHubError::Api {
            status: 0,
            message: "QueuesClient requires project_id -- set PGBHubConfig::project_id or PGBHUB_PROJECT_ID.".to_string(),
        })?;
        Ok(format!(
            "{}/api/v1/projects/{}/queues/{}/{}",
            self.endpoint, project_id, queue_name, action
        ))
    }

    /// Sends a message to a queue.
    pub async fn send(
        &self,
        queue_name: &str,
        payload: serde_json::Value,
        options: Option<SendMessageOptions>,
    ) -> Result<SentMessage, PGBHubError> {
        let url = self.url(queue_name, "send")?;
        let mut body = serde_json::json!({"payload": payload});
        if let Some(opts) = options {
            let obj = body.as_object_mut().expect("body is always a JSON object");
            if let Some(v) = opts.delay_seconds {
                obj.insert("delay_seconds".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.deduplication_id {
                obj.insert("deduplication_id".into(), serde_json::json!(v));
            }
        }
        let result: SendEnvelope = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        Ok(result.message)
    }

    /// Receives (and leases) messages from a queue.
    pub async fn receive(
        &self,
        queue_name: &str,
        options: Option<ReceiveMessagesOptions>,
    ) -> Result<Vec<ReceivedMessage>, PGBHubError> {
        let url = self.url(queue_name, "receive")?;
        let mut body = serde_json::json!({});
        if let Some(opts) = options {
            let obj = body.as_object_mut().expect("body is always a JSON object");
            if let Some(v) = opts.max_messages {
                obj.insert("max_messages".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.visibility_timeout {
                obj.insert("visibility_timeout".into(), serde_json::json!(v));
            }
            if let Some(v) = opts.consumer_id {
                obj.insert("consumer_id".into(), serde_json::json!(v));
            }
        }
        let result: ReceiveEnvelope = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        Ok(result.messages)
    }

    /// Acknowledges (deletes) a successfully processed message.
    pub async fn ack(&self, queue_name: &str, message_id: &str) -> Result<bool, PGBHubError> {
        let url = self.url(queue_name, "ack")?;
        let body = serde_json::json!({"message_id": message_id});
        let result: OkEnvelope = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        Ok(result.ok)
    }

    /// Negatively acknowledges a message -- returns it to the queue (or
    /// to the DLQ past max retries).
    pub async fn nack(
        &self,
        queue_name: &str,
        message_id: &str,
        error: Option<&str>,
    ) -> Result<bool, PGBHubError> {
        let url = self.url(queue_name, "nack")?;
        let body = serde_json::json!({"message_id": message_id, "error": error});
        let result: OkEnvelope = request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await?;
        Ok(result.ok)
    }
}

// ─── Realtime ────────────────────────────────────────────────────────────

/// Returned by `RealtimeClient::broadcast`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BroadcastResult {
    pub ok: bool,
    #[serde(default)]
    pub broadcast: serde_json::Value,
}

/// One event delivered by `RealtimeClient::subscribe`.
#[derive(Debug, Clone)]
pub struct RealtimeMsgEvent {
    pub event: String,
    pub data: Vec<u8>,
}

/// Broadcast/subscribe channels (`client.realtime`). Channel names are
/// namespaced server-side by project, so two different projects'
/// channels of the same name never collide.
pub struct RealtimeClient {
    endpoint: String,
    api_key: Option<String>,
    http: HttpClient,
}

impl RealtimeClient {
    /// Broadcasts a message to a realtime channel.
    pub async fn broadcast(
        &self,
        channel: &str,
        payload: serde_json::Value,
        event: Option<&str>,
    ) -> Result<BroadcastResult, PGBHubError> {
        let url = format!(
            "{}/api/v1/realtime/channels/{}/broadcast",
            self.endpoint, channel
        );
        let body = serde_json::json!({
            "channel": channel,
            "event": event.unwrap_or("message"),
            "payload": payload,
        });
        request_json(
            &self.http,
            reqwest::Method::POST,
            &url,
            &self.api_key,
            &[],
            Some(body),
        )
        .await
    }

    /// Subscribes to a realtime channel via Server-Sent Events, calling
    /// `on_event` for each event as it arrives. Runs until the
    /// connection ends or errors; the caller typically drives this
    /// inside its own `tokio::spawn`'d task to run it alongside other
    /// work and cancel it by aborting that task.
    pub async fn subscribe<F: FnMut(RealtimeMsgEvent)>(
        &self,
        channel: &str,
        mut on_event: F,
    ) -> Result<(), PGBHubError> {
        let url = format!(
            "{}/api/v1/realtime/stream?channel={}",
            self.endpoint, channel
        );
        let mut req = self.http.get(&url).header("Accept", "text/event-stream");
        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
        let mut resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let message = resp.text().await.unwrap_or_default();
            return Err(PGBHubError::Api { status, message });
        }

        let mut buffer: Vec<u8> = Vec::new();
        let mut current_event = "message".to_string();
        let mut current_data: Vec<String> = Vec::new();

        while let Some(chunk) = resp.chunk().await? {
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    if !current_data.is_empty() {
                        on_event(RealtimeMsgEvent {
                            event: current_event.clone(),
                            data: current_data.join("\n").into_bytes(),
                        });
                        current_event = "message".to_string();
                        current_data.clear();
                    }
                } else if let Some(rest) = line.strip_prefix("event:") {
                    current_event = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    current_data.push(rest.trim().to_string());
                }
            }
        }
        Ok(())
    }
}
