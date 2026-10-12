use reqwest::Client as ReqwestClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PGBHubError {
    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),
    /// The server answered with a non-2xx status. `message` is the server's
    /// own message; `code` is its machine-readable error code when it sent
    /// one, and `retry_after` is how many seconds a rate-limited (429)
    /// caller was told to wait.
    #[error("API error [{status}]: {message}")]
    Api {
        status: u16,
        message: String,
        code: Option<String>,
        retry_after: Option<f64>,
    },
    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

impl PGBHubError {
    /// An API-style error raised by the SDK itself (status 0 = never sent).
    fn local(code: &str, message: impl Into<String>) -> Self {
        PGBHubError::Api {
            status: 0,
            message: message.into(),
            code: Some(code.to_string()),
            retry_after: None,
        }
    }

    /// The HTTP status, if the server answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            PGBHubError::Api { status, .. } if *status != 0 => Some(*status),
            _ => None,
        }
    }

    /// The server's machine-readable error code, if it sent one.
    pub fn code(&self) -> Option<&str> {
        match self {
            PGBHubError::Api { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}

/// Client settings. Leave `endpoint` empty to use `PGBHUB_ENDPOINT` /
/// `PGBHUB_PROJECT_URL`, then the project's own address
/// `https://<project_id>.pgbhub.app` (domain from `PGBHUB_PROJECT_DOMAIN`),
/// then `https://cloud.pgbhub.com`. Leave `database` empty and the server
/// uses the database the key belongs to.
#[derive(Debug, Clone)]
pub struct PGBHubConfig {
    pub endpoint: String,
    pub api_key: Option<String>,
    pub database: String,
    /// Selects the project's address, and is required by QueuesClient,
    /// whose routes are addressed by project rather than database.
    pub project_id: Option<String>,
    /// Per-request timeout in seconds (default 30).
    pub timeout_secs: u64,
    /// Retries for reads (GET) on 502/503/504 and connection errors
    /// (default 2). Writes never retry.
    pub max_retries: u32,
    /// First backoff step in milliseconds, doubled each retry (default 300).
    pub retry_delay_ms: u64,
}

impl Default for PGBHubConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            api_key: std::env::var("PGBHUB_API_KEY")
                .ok()
                .or_else(|| std::env::var("PGBHUB_PUBLISHABLE_KEY").ok()),
            database: std::env::var("PGBHUB_DATABASE").unwrap_or_default(),
            project_id: std::env::var("PGBHUB_PROJECT_ID").ok(),
            timeout_secs: 30,
            max_retries: 2,
            retry_delay_ms: 300,
        }
    }
}

fn resolve_endpoint(config: &PGBHubConfig) -> String {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let chosen = if !config.endpoint.is_empty() {
        config.endpoint.clone()
    } else if let Some(v) = env("PGBHUB_ENDPOINT").or_else(|| env("PGBHUB_PROJECT_URL")) {
        v
    } else if let Some(ref id) = config.project_id {
        let domain = env("PGBHUB_PROJECT_DOMAIN").unwrap_or_else(|| "pgbhub.app".to_string());
        format!("https://{}.{}", id, domain)
    } else {
        "https://cloud.pgbhub.com".to_string()
    };
    chosen.trim_end_matches('/').to_string()
}

/// reqwest's client plus the retry policy every sub-client shares.
#[derive(Clone)]
struct HttpClient {
    inner: ReqwestClient,
    max_retries: u32,
    retry_delay: Duration,
}

impl HttpClient {
    fn new(config: &PGBHubConfig) -> Self {
        let inner = ReqwestClient::builder()
            .timeout(Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .unwrap_or_else(|_| ReqwestClient::new());
        Self {
            inner,
            max_retries: config.max_retries,
            retry_delay: Duration::from_millis(config.retry_delay_ms),
        }
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.inner.request(method, url)
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.inner.get(url)
    }
}

/// Appends query parameters (percent-encoded). `None` values are skipped.
fn with_query(path: &str, query: &[(&str, Option<String>)]) -> String {
    let pairs: Vec<String> = query
        .iter()
        .filter_map(|(k, v)| v.as_ref().filter(|v| !v.is_empty()).map(|v| format!("{}={}", k, encode_component(v))))
        .collect();
    if pairs.is_empty() {
        path.to_string()
    } else {
        format!("{}?{}", path, pairs.join("&"))
    }
}

/// Percent-encodes everything but unreserved characters.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResponse {
    pub action: String,
    pub status: String,
    pub dry_run: bool,
    pub duration_ms: f64,
    pub results: serde_json::Value,
    pub outbox_id: Option<i64>,
    /// How many of the action's effects were queued to run after the SQL committed.
    #[serde(default)]
    pub effects_queued: i64,
    /// On a dry run: what each effect would have done (nothing is run).
    #[serde(default)]
    pub effects_preview: Vec<serde_json::Value>,
    pub message: Option<String>,
}

pub struct PGBHubClient {
    config: PGBHubConfig,
    endpoint: String,
    /// The configured database, or "postgres" for the URL-addressed calls when none was set.
    database: String,
    http: HttpClient,

    /// Built-In PGB Auth-as-a-Service client.
    pub auth: AuthClient,
    /// Invokes and manages actions and their effects.
    pub actions: ActionsClient,
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
        let endpoint = resolve_endpoint(&config);
        let database_explicit = !config.database.is_empty();
        let database = if database_explicit {
            config.database.clone()
        } else {
            "postgres".to_string()
        };
        let http = HttpClient::new(&config);
        let auth = AuthClient::new(endpoint.clone(), config.api_key.clone(), http.clone());
        let actions = ActionsClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            database: if database_explicit { Some(database.clone()) } else { None },
            http: http.clone(),
        };
        let storage = StorageClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            http: http.clone(),
        };
        let functions = FunctionsClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            database: database.clone(),
            http: http.clone(),
        };
        let cron = CronClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            database: database.clone(),
            http: http.clone(),
        };
        let jobs = JobsClient {
            endpoint: endpoint.clone(),
            api_key: config.api_key.clone(),
            database: database.clone(),
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
            endpoint,
            database,
            http,
            auth,
            actions,
            storage,
            functions,
            cron,
            jobs,
            queues,
            realtime,
        }
    }

    /// The address requests are sent to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn get_schema(&self) -> Result<serde_json::Value, PGBHubError> {
        let url = format!(
            "{}/api/v1/databases/{}/schema.json",
            self.endpoint,
            encode_component(&self.database)
        );
        request_json(&self.http, reqwest::Method::GET, &url, &self.config.api_key, &[], None).await
    }

    /// Runs an action by name. With `dry_run` the SQL is simulated and the
    /// response lists what each effect would have done.
    pub async fn invoke_action(
        &self,
        action: &str,
        params: HashMap<String, serde_json::Value>,
        dry_run: bool,
    ) -> Result<ActionResponse, PGBHubError> {
        self.actions.run(action, params, dry_run).await
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
        let url = format!("{}/api/v1/databases", self.endpoint);
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

fn api_error(status: u16, text: &str, retry_after: Option<f64>) -> PGBHubError {
    let parsed = serde_json::from_str::<serde_json::Value>(text).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    let message = field("error")
        .or_else(|| field("detail"))
        .or_else(|| field("message"))
        .unwrap_or_else(|| text.to_string());
    PGBHubError::Api {
        status,
        message,
        code: field("code"),
        retry_after,
    }
}

/// Shared HTTP helper used by every client -- sends an authenticated JSON
/// request and decodes a 2xx JSON response into `T`, or returns
/// PGBHubError::Api with the server's own parsed error message. GETs retry
/// 502/503/504 and connection errors with backoff; writes never retry.
async fn request_json<T: serde::de::DeserializeOwned>(
    http: &HttpClient,
    method: reqwest::Method,
    url: &str,
    api_key: &Option<String>,
    extra_headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> Result<T, PGBHubError> {
    let attempts = if method == reqwest::Method::GET { 1 + http.max_retries } else { 1 };
    let mut attempt: u32 = 0;
    loop {
        let last = attempt + 1 >= attempts;
        let mut req = http
            .request(method.clone(), url)
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

        let backoff = http.retry_delay * 2u32.saturating_pow(attempt);
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if !last {
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                    continue;
                }
                return Err(e.into());
            }
        };
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0);
        let text = resp.text().await?;

        if matches!(status.as_u16(), 502..=504) && !last {
            tokio::time::sleep(backoff).await;
            attempt += 1;
            continue;
        }
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text, retry_after));
        }
        return Ok(serde_json::from_str(if text.trim().is_empty() { "null" } else { &text })?);
    }
}

// ─── Actions ─────────────────────────────────────────────────────────────

/// A registered action.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActionDefinition {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub sql_template: Option<String>,
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub sample_params: Option<serde_json::Value>,
    /// What the action does after its SQL commits. Each effect is an object
    /// with a `type` (function, queue, mail, event, job, realtime, container,
    /// action, webhook); values may use `{{params.x}}` and `{{results.0.id}}`.
    #[serde(default)]
    pub effects: Vec<serde_json::Value>,
    #[serde(default)]
    pub webhook_url: Option<String>,
}

/// An action to create or replace.
#[derive(Debug, Clone, Default)]
pub struct RegisterActionOptions {
    pub name: String,
    /// The SQL, using `%(param)s` placeholders.
    pub sql: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
    pub output_schema: Option<serde_json::Value>,
    pub sample_params: Option<serde_json::Value>,
    pub effects: Option<Vec<serde_json::Value>>,
    pub webhook_url: Option<String>,
}

/// One effect's outcome: delivered, waiting, or failed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActionDelivery {
    pub id: i64,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub effect: String,
    /// "pending", "dispatched" or "failed".
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub attempts: i64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Narrows `ActionsClient::deliveries`; `None` means "any".
#[derive(Debug, Clone, Default)]
pub struct DeliveryFilter {
    pub action: Option<String>,
    pub status: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
struct ActionsEnvelope {
    actions: Vec<ActionDefinition>,
}

#[derive(Deserialize)]
struct DeliveriesEnvelope {
    deliveries: Vec<ActionDelivery>,
}

/// Invokes and manages actions and their effects (`client.actions`).
pub struct ActionsClient {
    endpoint: String,
    api_key: Option<String>,
    /// Only sent when the caller configured a database.
    database: Option<String>,
    http: HttpClient,
}

impl ActionsClient {
    fn url(&self, path: &str, mut query: Vec<(&str, Option<String>)>) -> String {
        query.insert(0, ("dbname", self.database.clone()));
        format!("{}{}", self.endpoint, with_query(path, &query))
    }

    /// Runs an action. With `dry_run` nothing is changed and the response
    /// lists what each effect would have done.
    pub async fn run(
        &self,
        action: &str,
        params: HashMap<String, serde_json::Value>,
        dry_run: bool,
    ) -> Result<ActionResponse, PGBHubError> {
        let mut body = serde_json::json!({
            "action": action,
            "params": params,
            "dry_run": dry_run,
        });
        if let Some(ref db) = self.database {
            body["dbname"] = serde_json::json!(db);
        }
        let url = format!("{}/v1/actions/invoke", self.endpoint);
        request_json(&self.http, reqwest::Method::POST, &url, &self.api_key, &[], Some(body)).await
    }

    /// Every registered action.
    pub async fn list(&self) -> Result<Vec<ActionDefinition>, PGBHubError> {
        let env: ActionsEnvelope = request_json(
            &self.http,
            reqwest::Method::GET,
            &self.url("/v1/actions", vec![]),
            &self.api_key,
            &[],
            None,
        )
        .await?;
        Ok(env.actions)
    }

    /// One action by name.
    pub async fn get(&self, name: &str) -> Result<ActionDefinition, PGBHubError> {
        request_json(
            &self.http,
            reqwest::Method::GET,
            &self.url(&format!("/v1/actions/{}", encode_component(name)), vec![]),
            &self.api_key,
            &[],
            None,
        )
        .await
    }

    /// Creates or replaces an action and its effects.
    pub async fn register(&self, options: RegisterActionOptions) -> Result<serde_json::Value, PGBHubError> {
        if options.name.is_empty() || options.sql.is_empty() {
            return Err(PGBHubError::local(
                "invalid_request",
                "register needs a name and sql (use %(param)s placeholders in the SQL).",
            ));
        }
        let mut body = serde_json::json!({
            "name": options.name,
            "sql_template": options.sql,
            "description": options.description.unwrap_or_default(),
        });
        let optional = [
            ("title", options.title.map(serde_json::Value::from)),
            ("input_schema", options.input_schema),
            ("output_schema", options.output_schema),
            ("sample_params", options.sample_params),
            ("effects", options.effects.map(serde_json::Value::from)),
            ("webhook_url", options.webhook_url.map(serde_json::Value::from)),
        ];
        for (key, value) in optional {
            if let Some(v) = value {
                body[key] = v;
            }
        }
        request_json(
            &self.http,
            reqwest::Method::POST,
            &self.url("/v1/actions/register", vec![]),
            &self.api_key,
            &[],
            Some(body),
        )
        .await
    }

    /// Removes an action.
    pub async fn delete(&self, name: &str) -> Result<(), PGBHubError> {
        request_json::<serde_json::Value>(
            &self.http,
            reqwest::Method::DELETE,
            &self.url(&format!("/v1/actions/{}", encode_component(name)), vec![]),
            &self.api_key,
            &[],
            None,
        )
        .await
        .map(|_| ())
    }

    /// What the actions' effects did: delivered, waiting, or failed (with the reason).
    pub async fn deliveries(&self, filter: DeliveryFilter) -> Result<Vec<ActionDelivery>, PGBHubError> {
        let url = self.url(
            "/v1/actions/deliveries",
            vec![
                ("action", filter.action),
                ("status", filter.status),
                ("limit", filter.limit.map(|l| l.to_string())),
            ],
        );
        let env: DeliveriesEnvelope =
            request_json(&self.http, reqwest::Method::GET, &url, &self.api_key, &[], None).await?;
        Ok(env.deliveries)
    }

    /// Puts a failed delivery back in the queue.
    pub async fn retry_delivery(&self, id: i64) -> Result<(), PGBHubError> {
        request_json::<serde_json::Value>(
            &self.http,
            reqwest::Method::POST,
            &self.url(&format!("/v1/actions/deliveries/{}/retry", id), vec![]),
            &self.api_key,
            &[],
            Some(serde_json::json!({})),
        )
        .await
        .map(|_| ())
    }
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

/// Returned by sign_up, verify_otp, verify_magic_link, verify_mfa,
/// oauth_callback and refresh_session. After sign_up, `session` is None and
/// `verification_required` is true when the project wants the email
/// confirmed first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResult {
    pub user: Option<AuthUser>,
    pub session: Option<AuthSession>,
    #[serde(default)]
    pub verification_required: bool,
    #[serde(default)]
    pub message: Option<String>,
}

/// Returned by sign_in. `session` is None and `mfa_required` is true
/// when the account has two-factor on: finish with `verify_mfa`.
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

/// Returned by request_otp. The code itself is only ever emailed; the API
/// never returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtpRequestResult {
    pub message: String,
    pub expires_in: i64,
}

/// Returned by send_magic_link. The link is only ever emailed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MagicLinkRequestResult {
    pub message: String,
    pub expires_in: i64,
}

/// The plain acknowledgement many auth calls return.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageResult {
    #[serde(default)]
    pub message: String,
    /// On change_password: how many other sessions were signed out.
    #[serde(default)]
    pub sessions_revoked: i64,
}

/// One entry in a user's sign-in and security history.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthEvent {
    #[serde(rename = "type", default)]
    pub event_type: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub detail: serde_json::Value,
}

/// One signed-in device. `is_current` marks the caller's own.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthSessionInfo {
    pub id: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub is_current: bool,
}

/// Returned by enroll_mfa: show `provisioning_uri` as a QR code (or `secret` as text).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MfaEnrollment {
    pub secret: String,
    pub provisioning_uri: String,
}

/// Returned by confirm_mfa. Show the backup codes once.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MfaConfirmation {
    pub enabled: bool,
    #[serde(default)]
    pub backup_codes: Vec<String>,
}

/// Returned by oauth_start: send the user to `authorize_url` and check `state` on return.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuthStart {
    pub authorize_url: String,
    pub state: String,
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
/// is sent separately as `X-PGB-User-Token` on the calls that act as that user.
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

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        user_token: Option<&str>,
    ) -> Result<T, PGBHubError> {
        let url = format!("{}{}", self.endpoint, path);
        let headers: Vec<(&str, &str)> = match user_token {
            Some(t) => vec![("X-PGB-User-Token", t)],
            None => vec![],
        };
        request_json(&self.http, method, &url, &self.api_key, &headers, body).await
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T, PGBHubError> {
        self.call(reqwest::Method::POST, path, Some(body), None).await
    }

    /// The signed-in user's access token, or a `not_signed_in` error.
    fn user_token(&self) -> Result<String, PGBHubError> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.access_token.clone())
            .ok_or_else(|| {
                PGBHubError::local(
                    "not_signed_in",
                    "PGBHub Auth: no signed-in user (sign in first, or call set_session).",
                )
            })
    }

    fn keep(&self, session: &Option<AuthSession>) {
        if session.is_some() {
            *self.session.lock().unwrap() = session.clone();
        }
    }

    fn redirect(mut body: serde_json::Value, redirect_to: Option<&str>) -> serde_json::Value {
        if let Some(r) = redirect_to.filter(|r| !r.is_empty()) {
            body["redirect_to"] = serde_json::json!(r);
        }
        body
    }

    /// The project's sign-in rules (methods on, password policy, social
    /// providers) so a UI can adapt. No sign-in needed.
    pub async fn get_settings(&self) -> Result<serde_json::Value, PGBHubError> {
        self.call(reqwest::Method::GET, "/auth/settings", None, None).await
    }

    /// Signs up a new user with email & password.
    pub async fn sign_up(
        &self,
        email: &str,
        password: Option<&str>,
        metadata: Option<serde_json::Value>,
    ) -> Result<AuthResult, PGBHubError> {
        self.sign_up_with_redirect(email, password, metadata, None).await
    }

    /// Like `sign_up`, with a redirect for the verification email (it must be
    /// allowed in the project's auth settings). When the project requires
    /// verification the result has no session.
    pub async fn sign_up_with_redirect(
        &self,
        email: &str,
        password: Option<&str>,
        metadata: Option<serde_json::Value>,
        redirect_to: Option<&str>,
    ) -> Result<AuthResult, PGBHubError> {
        let body = Self::redirect(
            serde_json::json!({
                "email": email,
                "password": password,
                "metadata": metadata.unwrap_or_else(|| serde_json::json!({})),
            }),
            redirect_to,
        );
        let result: AuthResult = self.post("/auth/signup", body).await?;
        self.keep(&result.session);
        Ok(result)
    }

    /// Signs in with email & password.
    pub async fn sign_in(
        &self,
        email: &str,
        password: Option<&str>,
    ) -> Result<LoginResult, PGBHubError> {
        let result: LoginResult = self
            .post("/auth/login", serde_json::json!({"email": email, "password": password}))
            .await?;
        self.keep(&result.session);
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
            // A failed server-side revoke must not block clearing local state.
            let _ = self
                .call::<serde_json::Value>(
                    reqwest::Method::POST,
                    "/auth/logout",
                    Some(serde_json::json!({})),
                    Some(token.as_str()),
                )
                .await;
        }
        *self.session.lock().unwrap() = None;
        Ok(())
    }

    /// Returns the current user, or `None` without a network call when
    /// signed out. An expired session (401/403) also returns `None`; other
    /// failures are errors.
    pub async fn get_user(&self) -> Result<Option<AuthUser>, PGBHubError> {
        let token = match self.user_token() {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        match self
            .call::<UserEnvelope>(reqwest::Method::GET, "/auth/user", None, Some(token.as_str()))
            .await
        {
            Ok(r) => Ok(r.user),
            Err(PGBHubError::Api { status: 401 | 403, .. }) => Ok(None),
            Err(e) => Err(e),
        }
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
                current.ok_or_else(|| {
                    PGBHubError::local(
                        "no_refresh_token",
                        "refresh_session requires a refresh token (none provided and no active session)",
                    )
                })?
            }
        };
        let result: AuthResult = self
            .post("/auth/refresh", serde_json::json!({"refresh_token": token}))
            .await?;
        self.keep(&result.session);
        Ok(result)
    }

    /// Emails a one-time passcode for passwordless sign-in.
    pub async fn request_otp(&self, email: &str) -> Result<OtpRequestResult, PGBHubError> {
        self.post("/auth/otp", serde_json::json!({"email": email})).await
    }

    /// Verifies a one-time passcode and establishes a session.
    pub async fn verify_otp(&self, email: &str, token: &str) -> Result<AuthResult, PGBHubError> {
        let result: AuthResult = self
            .post("/auth/otp/verify", serde_json::json!({"email": email, "token": token}))
            .await?;
        self.keep(&result.session);
        Ok(result)
    }

    /// Emails a passwordless magic link.
    pub async fn send_magic_link(
        &self,
        email: &str,
    ) -> Result<MagicLinkRequestResult, PGBHubError> {
        self.send_magic_link_to(email, None).await
    }

    /// Like `send_magic_link`, with a redirect allowed in the project's settings.
    pub async fn send_magic_link_to(
        &self,
        email: &str,
        redirect_to: Option<&str>,
    ) -> Result<MagicLinkRequestResult, PGBHubError> {
        self.post(
            "/auth/magic",
            Self::redirect(serde_json::json!({"email": email}), redirect_to),
        )
        .await
    }

    /// Verifies a magic link token and establishes a session.
    pub async fn verify_magic_link(&self, token: &str) -> Result<AuthResult, PGBHubError> {
        let result: AuthResult = self
            .post("/auth/magic/verify", serde_json::json!({"token": token}))
            .await?;
        self.keep(&result.session);
        Ok(result)
    }

    /// Sends the email-verification message again.
    pub async fn resend_verification(
        &self,
        email: &str,
        redirect_to: Option<&str>,
    ) -> Result<MessageResult, PGBHubError> {
        self.post(
            "/auth/resend-verification",
            Self::redirect(serde_json::json!({"email": email}), redirect_to),
        )
        .await
    }

    /// Emails a password-reset link. It always succeeds, so it never
    /// reveals which emails have accounts.
    pub async fn forgot_password(
        &self,
        email: &str,
        redirect_to: Option<&str>,
    ) -> Result<MessageResult, PGBHubError> {
        self.post(
            "/auth/forgot-password",
            Self::redirect(serde_json::json!({"email": email}), redirect_to),
        )
        .await
    }

    /// Sets a new password using the token from the reset email.
    pub async fn reset_password(&self, token: &str, password: &str) -> Result<MessageResult, PGBHubError> {
        self.post(
            "/auth/reset-password",
            serde_json::json!({"token": token, "password": password}),
        )
        .await
    }

    /// Changes the signed-in user's password. Other sessions are signed
    /// out; this one stays.
    pub async fn change_password(
        &self,
        current_password: Option<&str>,
        new_password: &str,
    ) -> Result<MessageResult, PGBHubError> {
        let token = self.user_token()?;
        self.call(
            reqwest::Method::POST,
            "/auth/user/password",
            Some(serde_json::json!({
                "current_password": current_password.unwrap_or(""),
                "new_password": new_password,
            })),
            Some(token.as_str()),
        )
        .await
    }

    /// Merges values into the user's profile (`user_metadata`). A key set
    /// to null is removed.
    pub async fn update_user(&self, metadata: serde_json::Value) -> Result<Option<AuthUser>, PGBHubError> {
        let token = self.user_token()?;
        let env: UserEnvelope = self
            .call(
                reqwest::Method::POST,
                "/auth/user/update",
                Some(serde_json::json!({"user_metadata": metadata})),
                Some(token.as_str()),
            )
            .await?;
        Ok(env.user)
    }

    /// Permanently deletes the signed-in user's own account and clears the
    /// local session.
    pub async fn delete_account(&self, password: Option<&str>) -> Result<MessageResult, PGBHubError> {
        let token = self.user_token()?;
        let result: MessageResult = self
            .call(
                reqwest::Method::POST,
                "/auth/user/delete",
                Some(serde_json::json!({"password": password.unwrap_or("")})),
                Some(token.as_str()),
            )
            .await?;
        *self.session.lock().unwrap() = None;
        Ok(result)
    }

    /// The user's recent sign-ins and security events.
    pub async fn get_events(&self) -> Result<Vec<AuthEvent>, PGBHubError> {
        #[derive(Deserialize)]
        struct Env {
            #[serde(default)]
            events: Vec<AuthEvent>,
        }
        let token = self.user_token()?;
        let env: Env = self
            .call(reqwest::Method::GET, "/auth/user/events", None, Some(token.as_str()))
            .await?;
        Ok(env.events)
    }

    /// The user's active sessions (devices).
    pub async fn list_sessions(&self) -> Result<Vec<AuthSessionInfo>, PGBHubError> {
        #[derive(Deserialize)]
        struct Env {
            #[serde(default)]
            sessions: Vec<AuthSessionInfo>,
        }
        let token = self.user_token()?;
        let env: Env = self
            .call(reqwest::Method::GET, "/auth/sessions", None, Some(token.as_str()))
            .await?;
        Ok(env.sessions)
    }

    /// Signs out one session (for example an old phone).
    pub async fn revoke_session(&self, session_id: &str) -> Result<(), PGBHubError> {
        let token = self.user_token()?;
        self.call::<serde_json::Value>(
            reqwest::Method::DELETE,
            &format!("/auth/sessions/{}", encode_component(session_id)),
            None,
            Some(token.as_str()),
        )
        .await
        .map(|_| ())
    }

    /// Signs out every session except this one; returns how many were ended.
    pub async fn revoke_other_sessions(&self) -> Result<i64, PGBHubError> {
        #[derive(Deserialize)]
        struct Env {
            #[serde(default)]
            revoked: i64,
        }
        let token = self.user_token()?;
        let env: Env = self
            .call(
                reqwest::Method::POST,
                "/auth/sessions/revoke-all",
                Some(serde_json::json!({})),
                Some(token.as_str()),
            )
            .await?;
        Ok(env.revoked)
    }

    /// Starts two-factor enrolment; finish with `confirm_mfa`.
    pub async fn enroll_mfa(&self) -> Result<MfaEnrollment, PGBHubError> {
        let token = self.user_token()?;
        self.call(
            reqwest::Method::POST,
            "/auth/mfa/enroll",
            Some(serde_json::json!({})),
            Some(token.as_str()),
        )
        .await
    }

    /// Confirms enrolment with a current code and returns one-time backup codes.
    pub async fn confirm_mfa(&self, code: &str) -> Result<MfaConfirmation, PGBHubError> {
        let token = self.user_token()?;
        self.call(
            reqwest::Method::POST,
            "/auth/mfa/confirm",
            Some(serde_json::json!({"code": code})),
            Some(token.as_str()),
        )
        .await
    }

    /// Turns two-factor off (needs a current code).
    pub async fn disable_mfa(&self, code: &str) -> Result<(), PGBHubError> {
        let token = self.user_token()?;
        self.call::<serde_json::Value>(
            reqwest::Method::POST,
            "/auth/mfa/disable",
            Some(serde_json::json!({"code": code})),
            Some(token.as_str()),
        )
        .await
        .map(|_| ())
    }

    /// Finishes a sign-in that returned `mfa_required`.
    pub async fn verify_mfa(&self, challenge_token: &str, code: &str) -> Result<AuthResult, PGBHubError> {
        let result: AuthResult = self
            .post(
                "/auth/mfa/verify",
                serde_json::json!({"challenge_token": challenge_token, "code": code}),
            )
            .await?;
        self.keep(&result.session);
        Ok(result)
    }

    /// Returns the provider's authorize URL and the `state` to check on return.
    pub async fn oauth_start(&self, provider: &str, redirect_uri: &str) -> Result<OAuthStart, PGBHubError> {
        let path = with_query(
            &format!("/auth/oauth/{}/start", encode_component(provider)),
            &[("redirect_uri", Some(redirect_uri.to_string()))],
        );
        self.call(reqwest::Method::GET, &path, None, None).await
    }

    /// Finishes a social sign-in with the `code` and `state` the provider sent back.
    pub async fn oauth_callback(
        &self,
        provider: &str,
        code: &str,
        state: &str,
        redirect_uri: &str,
    ) -> Result<AuthResult, PGBHubError> {
        let result: AuthResult = self
            .post(
                &format!("/auth/oauth/{}/callback", encode_component(provider)),
                serde_json::json!({"code": code, "state": state, "redirect_uri": redirect_uri}),
            )
            .await?;
        self.keep(&result.session);
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
            return Err(api_error(status.as_u16(), &text, None));
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
        let project_id = self.project_id.as_ref().ok_or_else(|| {
            PGBHubError::local(
                "missing_project_id",
                "QueuesClient requires project_id -- set PGBHubConfig::project_id or PGBHUB_PROJECT_ID.",
            )
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
            return Err(api_error(status, &message, None));
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
