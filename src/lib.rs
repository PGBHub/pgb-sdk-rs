use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PGBoxError {
    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error [{status}]: {message}")]
    Api { status: u16, message: String },
    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct PGBoxConfig {
    pub endpoint: String,
    pub api_key: Option<String>,
    pub database: String,
}

impl Default for PGBoxConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://cloud.pgboxhub.com".to_string(),
            api_key: std::env::var("PGBOX_API_KEY").ok(),
            database: "postgres".to_string(),
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

pub struct PGBoxClient {
    config: PGBoxConfig,
    http: HttpClient,
}

impl PGBoxClient {
    pub fn new(config: PGBoxConfig) -> Self {
        Self {
            config,
            http: HttpClient::new(),
        }
    }

    pub async fn get_schema(&self) -> Result<serde_json::Value, PGBoxError> {
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
            return Err(PGBoxError::Api { status, message: msg });
        }

        Ok(resp.json::<serde_json::Value>().await?)
    }

    pub async fn invoke_action(
        &self,
        action: &str,
        params: HashMap<String, serde_json::Value>,
        dry_run: bool,
    ) -> Result<ActionResponse, PGBoxError> {
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
            return Err(PGBoxError::Api { status, message: msg });
        }

        Ok(resp.json::<ActionResponse>().await?)
    }
}
