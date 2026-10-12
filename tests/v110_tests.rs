//! 1.1.0: project address, error details, GET retries, the full auth
//! surface and Actions management, against a wiremock server.

use pgbhub::{
    AuthSession, DeliveryFilter, PGBHubClient, PGBHubConfig, PGBHubError, RegisterActionOptions,
};
use serde_json::json;
use std::collections::HashMap;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(endpoint: String) -> PGBHubClient {
    PGBHubClient::new(PGBHubConfig {
        endpoint,
        api_key: Some("k".to_string()),
        database: String::new(),
        project_id: None,
        retry_delay_ms: 1,
        ..PGBHubConfig::default()
    })
}

fn signed_in(c: &PGBHubClient) {
    c.auth.set_session(Some(AuthSession {
        access_token: "at".to_string(),
        refresh_token: "rt".to_string(),
        token_type: None,
        expires_in: None,
        expires_at: None,
    }));
}

#[test]
fn project_id_selects_the_project_address() {
    let make = |id: Option<&str>| {
        PGBHubClient::new(PGBHubConfig {
            endpoint: String::new(),
            api_key: None,
            database: String::new(),
            project_id: id.map(|s| s.to_string()),
            ..PGBHubConfig::default()
        })
    };
    // Environment overrides would change the result; the suite does not set them.
    if std::env::var("PGBHUB_ENDPOINT").is_err() && std::env::var("PGBHUB_PROJECT_URL").is_err() && std::env::var("PGBHUB_PROJECT_DOMAIN").is_err() {
        assert_eq!(make(Some("shop")).endpoint(), "https://shop.pgbhub.app");
        assert_eq!(make(None).endpoint(), "https://cloud.pgbhub.com");
    }
}

#[tokio::test]
async fn errors_carry_code_and_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_json(json!({"error": "Too many", "code": "rate_limited"})),
        )
        .mount(&server)
        .await;
    let err = client(server.uri()).auth.sign_in("a@b.c", Some("pw")).await.unwrap_err();
    match &err {
        PGBHubError::Api { status, message, code, retry_after } => {
            assert_eq!(*status, 429);
            assert_eq!(message, "Too many");
            assert_eq!(code.as_deref(), Some("rate_limited"));
            assert_eq!(*retry_after, Some(7.0));
        }
        other => panic!("expected Api, got {other:?}"),
    }
    assert_eq!(err.status(), Some(429));
    assert_eq!(err.code(), Some("rate_limited"));
}

#[tokio::test]
async fn gets_retry_transient_failures_but_writes_do_not() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/actions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error": "busy"})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"actions": [{"name": "a"}]})))
        .mount(&server)
        .await;
    let actions = client(server.uri()).actions.list().await.unwrap();
    assert_eq!(actions.len(), 1);

    let write_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error": "busy"})))
        .expect(1)
        .mount(&write_server)
        .await;
    assert!(client(write_server.uri()).auth.sign_in("a", Some("b")).await.is_err());
}

#[tokio::test]
async fn sign_up_with_verification_keeps_no_session_and_sends_redirect() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/signup"))
        .and(body_json(json!({"email": "a@b.c", "password": "pw", "metadata": {}, "redirect_to": "https://app/verified"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"user": {"id": "u", "email": "a@b.c"}, "session": null, "verification_required": true})))
        .mount(&server)
        .await;
    let c = client(server.uri());
    let r = c.auth.sign_up_with_redirect("a@b.c", Some("pw"), None, Some("https://app/verified")).await.unwrap();
    assert!(r.verification_required);
    assert!(c.auth.get_session().is_none());
}

#[tokio::test]
async fn user_calls_need_a_session_and_send_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/user/password"))
        .and(header("X-PGB-User-Token", "at"))
        .and(body_json(json!({"current_password": "old", "new_password": "new"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": "ok", "sessions_revoked": 2})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/auth/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sessions": [{"id": "s1", "is_current": true}]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/sessions/revoke-all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"revoked": 3})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/user/delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": "deleted"})))
        .mount(&server)
        .await;
    let c = client(server.uri());

    let err = c.auth.change_password(Some("old"), "new").await.unwrap_err();
    assert_eq!(err.code(), Some("not_signed_in"));

    signed_in(&c);
    assert_eq!(c.auth.change_password(Some("old"), "new").await.unwrap().sessions_revoked, 2);
    let sessions = c.auth.list_sessions().await.unwrap();
    assert!(sessions[0].is_current);
    assert_eq!(c.auth.revoke_other_sessions().await.unwrap(), 3);
    c.auth.delete_account(Some("pw")).await.unwrap();
    assert!(c.auth.get_session().is_none());
}

#[tokio::test]
async fn mfa_login_then_verify_stores_the_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"mfa_required": true, "challenge_token": "ch"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/mfa/verify"))
        .and(body_json(json!({"challenge_token": "ch", "code": "123456"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"session": {"access_token": "at2", "refresh_token": "rt2"}})))
        .mount(&server)
        .await;
    let c = client(server.uri());
    let login = c.auth.sign_in("a", Some("b")).await.unwrap();
    assert!(login.mfa_required && c.auth.get_session().is_none());
    c.auth.verify_mfa(&login.challenge_token.unwrap(), "123456").await.unwrap();
    assert_eq!(c.auth.get_session().unwrap().access_token, "at2");
}

#[tokio::test]
async fn password_flows_and_oauth_paths() {
    let server = MockServer::start().await;
    for p in ["/auth/forgot-password", "/auth/reset-password", "/auth/resend-verification"] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": "ok"})))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/auth/oauth/google/start"))
        .and(query_param("redirect_uri", "https://app/cb"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"authorize_url": "https://g", "state": "s"})))
        .mount(&server)
        .await;
    let c = client(server.uri());
    c.auth.forgot_password("a@b.c", Some("https://app/reset")).await.unwrap();
    c.auth.reset_password("tok", "pw").await.unwrap();
    c.auth.resend_verification("a@b.c", None).await.unwrap();
    let o = c.auth.oauth_start("google", "https://app/cb").await.unwrap();
    assert_eq!(o.authorize_url, "https://g");
}

#[tokio::test]
async fn get_user_is_none_for_an_expired_session_but_errors_otherwise() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/auth/user"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "expired"})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/auth/user"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
        .mount(&server)
        .await;
    let c = client(server.uri());
    signed_in(&c);
    assert!(c.auth.get_user().await.unwrap().is_none());
    assert!(c.auth.get_user().await.is_err());
}

#[tokio::test]
async fn actions_run_leaves_the_database_out_unless_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/actions/invoke"))
        .and(body_json(json!({"action": "a", "params": {"x": 1}, "dry_run": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "action": "a", "status": "simulated", "dry_run": true, "duration_ms": 1.0, "results": [],
            "effects_preview": [{"type": "mail", "would_run": {"to": "x"}}]
        })))
        .mount(&server)
        .await;
    let mut params = HashMap::new();
    params.insert("x".to_string(), json!(1));
    let r = client(server.uri()).invoke_action("a", params, true).await.unwrap();
    assert_eq!(r.effects_preview.len(), 1);
}

#[tokio::test]
async fn actions_management() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/actions/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "registered"})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/actions/a%20b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "deleted"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/actions/deliveries"))
        .and(query_param("status", "failed"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"deliveries": [{"id": 4, "status": "failed", "effect": "mail"}]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/actions/deliveries/4/retry"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(server.uri());

    c.actions
        .register(RegisterActionOptions {
            name: "signup".into(),
            sql: "SELECT 1".into(),
            effects: Some(vec![json!({"type": "mail"})]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(c.actions.register(RegisterActionOptions { name: "x".into(), ..Default::default() }).await.is_err());
    c.actions.delete("a b").await.unwrap();
    let d = c
        .actions
        .deliveries(DeliveryFilter { status: Some("failed".into()), limit: Some(5), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(d[0].id, 4);
    c.actions.retry_delivery(4).await.unwrap();
}

#[tokio::test]
async fn configured_database_is_passed_as_dbname() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/actions"))
        .and(query_param("dbname", "app"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"actions": []})))
        .expect(1)
        .mount(&server)
        .await;
    let c = PGBHubClient::new(PGBHubConfig {
        endpoint: server.uri(),
        api_key: Some("k".into()),
        database: "app".into(),
        project_id: None,
        ..PGBHubConfig::default()
    });
    c.actions.list().await.unwrap();
}
