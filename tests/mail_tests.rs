//! Mail: reply_to on the wire, message_id (with the mail_id fallback) and
//! the typed 503 MAIL_PROVIDER_UNAVAILABLE error, against a wiremock server.

use pgbhub::{PGBHubClient, PGBHubConfig, PGBHubError, SendMailOptions, MAIL_PROVIDER_UNAVAILABLE};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
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

fn email() -> SendMailOptions {
    SendMailOptions {
        to: "a@b.co".to_string(),
        subject: "Hi".to_string(),
        text: "Body".to_string(),
        ..SendMailOptions::default()
    }
}

#[tokio::test]
async fn reply_to_is_sent_on_the_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mail/send"))
        .and(header("Authorization", "Bearer k"))
        .and(body_json(json!({"to": "a@b.co", "subject": "Hi", "text": "Body", "reply_to": "support@acme.dev"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true, "message_id": "m1", "mail_id": "m1", "provider": "ses"})))
        .expect(1)
        .mount(&server)
        .await;
    let res = client(server.uri())
        .mail
        .send(SendMailOptions { reply_to: Some("support@acme.dev".to_string()), ..email() })
        .await
        .unwrap();
    assert!(res.success);
    assert_eq!(res.message_id.as_deref(), Some("m1"));
    assert_eq!(res.provider.as_deref(), Some("ses"));
}

#[tokio::test]
async fn without_reply_to_no_key_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mail/send"))
        .and(body_json(json!({"to": "a@b.co", "subject": "Hi", "text": "Body", "html": "<p>Body</p>", "from": "no-reply@acme.dev"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true, "message_id": "m1"})))
        .expect(1)
        .mount(&server)
        .await;
    client(server.uri())
        .mail
        .send(SendMailOptions {
            html: Some("<p>Body</p>".to_string()),
            from: Some("no-reply@acme.dev".to_string()),
            ..email()
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn message_id_falls_back_to_mail_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mail/send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true, "mail_id": "old1"})))
        .mount(&server)
        .await;
    let res = client(server.uri()).mail.send(email()).await.unwrap();
    assert_eq!(res.message_id.as_deref(), Some("old1"));
    assert_eq!(res.mail_id.as_deref(), Some("old1"));
}

#[tokio::test]
async fn provider_unavailable_is_typed_and_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mail/send"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error": "Email dispatch failed: no provider", "code": "MAIL_PROVIDER_UNAVAILABLE"})))
        .expect(1)
        .mount(&server)
        .await;
    let err = client(server.uri()).mail.send(email()).await.unwrap_err();
    assert!(err.is_mail_provider_unavailable());
    assert_eq!(err.status(), Some(503));
    assert_eq!(err.code(), Some(MAIL_PROVIDER_UNAVAILABLE));
    match err {
        PGBHubError::Api { message, .. } => assert_eq!(message, "Email dispatch failed: no provider"),
        other => panic!("want Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_reply_to_keeps_the_server_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mail/send"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "Invalid reply_to email address: 'a@b.co, c@d.co'"})))
        .mount(&server)
        .await;
    let err = client(server.uri())
        .mail
        .send(SendMailOptions { reply_to: Some("a@b.co, c@d.co".to_string()), ..email() })
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(400));
    assert_eq!(err.code(), None);
    assert!(!err.is_mail_provider_unavailable());
    assert!(err.to_string().contains("Invalid reply_to"));
}
