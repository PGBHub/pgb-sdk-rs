//! Regression tests for the Round 1 additions: AuthClient (+ admin),
//! StorageClient::get_file/get_media, FunctionsClient::deploy/delete,
//! and PGBHubClient::create_database.
//!
//! Runs the real client against a local wiremock server (not a
//! reimplementation), pinning the exact HTTP method/path/body/headers
//! each method sends.

use pgbhub::{
    CreateDatabaseOptions, DeployFunctionOptions, MediaTransformOptions, PGBHubClient,
    PGBHubConfig, PGBHubError,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_client(endpoint: String) -> PGBHubClient {
    PGBHubClient::new(PGBHubConfig {
        endpoint,
        api_key: Some("pgb_sec_test".to_string()),
        database: "appdb".to_string(),
    })
}

#[tokio::test]
async fn sign_up_posts_and_stores_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/signup"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "user": {"id": "usr_1", "email": "a@test.com", "is_verified": false},
            "session": {"access_token": "at1", "refresh_token": "rt1"}
        })))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let result = client
        .auth
        .sign_up("a@test.com", Some("pw"), None)
        .await
        .unwrap();

    assert_eq!(result.session.as_ref().unwrap().access_token, "at1");
    let session = client.auth.get_session().unwrap();
    assert_eq!(session.access_token, "at1");
}

#[tokio::test]
async fn sign_in_failure_surfaces_server_error_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": "Invalid email or password."})),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let err = client
        .auth
        .sign_in("a@test.com", Some("wrong"))
        .await
        .unwrap_err();

    match err {
        PGBHubError::Api { status, message } => {
            assert_eq!(status, 400);
            assert!(message.contains("Invalid email or password"));
        }
        other => panic!("expected PGBHubError::Api, got {other:?}"),
    }
    assert!(client.auth.get_session().is_none());
}

#[tokio::test]
async fn sign_out_sends_session_token_as_x_pgb_user_token_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/logout"))
        .and(header("X-PGB-User-Token", "at1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    client.auth.set_session(Some(pgbhub::AuthSession {
        access_token: "at1".to_string(),
        refresh_token: "rt1".to_string(),
        token_type: None,
        expires_in: None,
        expires_at: None,
    }));

    client.auth.sign_out().await.unwrap();
    assert!(client.auth.get_session().is_none());
}

#[tokio::test]
async fn get_user_without_session_returns_none_with_no_network_call() {
    let server = MockServer::start().await;
    // No mock registered at all -- any request would fail the test via
    // wiremock's unmatched-request panic in strict mode, but since we
    // don't call .expect() here, an actual network call would instead
    // surface as an HTTP error, which we also assert against.
    let client = make_client(server.uri());

    let result = client.auth.get_user().await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn admin_create_list_delete_user() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/admin/users"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "user": {"id": "usr_2", "email": "b@test.com", "is_verified": true}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/auth/admin/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "users": [{"id": "usr_2", "email": "b@test.com", "is_verified": true}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/auth/admin/users/usr_2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let created = client
        .auth
        .admin
        .create_user("b@test.com", None, None, true)
        .await
        .unwrap();
    assert_eq!(created.email, "b@test.com");

    let users = client.auth.admin.list_users().await.unwrap();
    assert_eq!(users.len(), 1);

    let deleted = client.auth.admin.delete_user("usr_2").await.unwrap();
    assert!(deleted);
}

#[tokio::test]
async fn storage_get_file_returns_bytes_and_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storage/notes/hello.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes("hello".as_bytes())
                .insert_header("content-type", "text/plain")
                .insert_header("etag", "\"abc\""),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let file = client.storage.get_file("/notes/hello.txt").await.unwrap();

    assert_eq!(file.data, b"hello");
    assert_eq!(file.content_type, "text/plain");
    assert_eq!(file.etag.as_deref(), Some("\"abc\""));
}

#[tokio::test]
async fn storage_get_media_is_alias_over_get_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storage/avatars/pic.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(vec![1, 2, 3])
                .insert_header("content-type", "image/png"),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let opts = MediaTransformOptions {
        width: Some(200),
        height: Some(200),
        format: None,
        fit: None,
    };
    let file = client
        .storage
        .get_media("/avatars/pic.png", Some(opts))
        .await
        .unwrap();
    assert_eq!(file.data, vec![1, 2, 3]);
}

#[tokio::test]
async fn functions_deploy_posts_name_and_code_in_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/databases/appdb/functions"))
        .and(body_json(
            json!({"name": "send-welcome", "code": "export default () => {}", "timeout_ms": 8000}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "success": true,
            "function": {"name": "send-welcome"}
        })))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let opts = DeployFunctionOptions {
        timeout_ms: Some(8000),
        ..Default::default()
    };
    let fn_result = client
        .functions
        .deploy("send-welcome", "export default () => {}", Some(opts))
        .await
        .unwrap();

    assert_eq!(fn_result.name, "send-welcome");
}

#[tokio::test]
async fn functions_delete_sends_name_in_url() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/databases/appdb/functions/send-welcome"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let ok = client.functions.delete("send-welcome").await.unwrap();
    assert!(ok);
}

#[tokio::test]
async fn create_database_posts_name_and_defaults_pg_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/databases"))
        .and(body_json(
            json!({"name": "newdb", "pg_version": "17", "extensions": null}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "success": true,
            "database": "newdb",
            "pg_version": "17",
            "extensions": [],
            "user": "u",
            "password": "p",
            "public_host": "h",
            "public_port": 5433,
            "url": "postgres://...",
            "message": "ready"
        })))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let result = client.create_database("newdb", None).await.unwrap();
    assert_eq!(result.database, "newdb");
}

#[tokio::test]
async fn create_database_surfaces_403_for_project_scoped_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/databases"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "This API key is scoped to a single project and cannot provision new databases."
        })))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let err = client
        .create_database("newdb", Some(CreateDatabaseOptions::default()))
        .await
        .unwrap_err();

    match err {
        PGBHubError::Api { status, message } => {
            assert_eq!(status, 403);
            assert!(message.contains("scoped to a single project"));
        }
        other => panic!("expected PGBHubError::Api, got {other:?}"),
    }
}
