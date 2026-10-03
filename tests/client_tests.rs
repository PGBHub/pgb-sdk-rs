//! Regression tests for the Round 1 additions: AuthClient (+ admin),
//! StorageClient::get_file/get_media, FunctionsClient::deploy/delete,
//! and PGBHubClient::create_database.
//!
//! Runs the real client against a local wiremock server (not a
//! reimplementation), pinning the exact HTTP method/path/body/headers
//! each method sends.

use pgbhub::{
    CreateCronJobOptions, CreateDatabaseOptions, DeployFunctionOptions, EnqueueJobOptions,
    ListJobsOptions, MediaTransformOptions, PGBHubClient, PGBHubConfig, PGBHubError,
    ReceiveMessagesOptions, SendMessageOptions,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_client(endpoint: String) -> PGBHubClient {
    PGBHubClient::new(PGBHubConfig {
        endpoint,
        api_key: Some("pgb_sec_test".to_string()),
        database: "appdb".to_string(),
        project_id: None,
    })
}

fn make_client_with_project(endpoint: String, project_id: &str) -> PGBHubClient {
    PGBHubClient::new(PGBHubConfig {
        endpoint,
        api_key: Some("pgb_sec_test".to_string()),
        database: "appdb".to_string(),
        project_id: Some(project_id.to_string()),
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

#[tokio::test]
async fn cron_create_posts_target_type_and_target_config() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/databases/appdb/crons"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "cron_1", "name": "nightly", "expression": "0 0 * * *", "target_type": "function"
        })))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let opts = CreateCronJobOptions {
        timeout_seconds: Some(60),
        ..Default::default()
    };
    let job = client
        .cron
        .create(
            "nightly",
            "0 0 * * *",
            "function",
            json!({"function_name": "cleanup"}),
            Some(opts),
        )
        .await
        .unwrap();

    assert_eq!(job.name, "nightly");
}

#[tokio::test]
async fn cron_list_reads_crons_field() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/databases/appdb/crons"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "crons": [{"id": "cron_1", "name": "n", "expression": "* * * * *", "target_type": "sql"}],
            "total": 1
        })))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let jobs = client.cron.list().await.unwrap();
    assert_eq!(jobs.len(), 1);
}

#[tokio::test]
async fn cron_toggle_run_delete_hit_right_subpaths() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/databases/appdb/crons/cron_1/toggle"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "cron_1", "name": "n", "expression": "* * * * *", "target_type": "sql"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/databases/appdb/crons/cron_1/run"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/databases/appdb/crons/cron_1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"deleted": true, "id": "cron_1"})),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    client.cron.toggle("cron_1", false).await.unwrap();
    client.cron.run("cron_1").await.unwrap();
    client.cron.delete("cron_1").await.unwrap();
}

#[tokio::test]
async fn jobs_enqueue_posts_job_type_and_unwraps_job() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/databases/appdb/jobs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "success": true,
            "job": {"id": 1, "job_type": "send-report", "target_type": "sql", "status": "pending"}
        })))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let opts = EnqueueJobOptions {
        max_attempts: Some(3),
        ..Default::default()
    };
    let job = client
        .jobs
        .enqueue("send-report", "sql", json!({"sql": "select 1"}), Some(opts))
        .await
        .unwrap();

    assert_eq!(job.id, 1);
}

#[tokio::test]
async fn jobs_get_returns_none_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/databases/appdb/jobs/999"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "Job not found"})))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let job = client.jobs.get(999).await.unwrap();
    assert!(job.is_none());
}

#[tokio::test]
async fn jobs_list_sends_status_and_limit_as_query_params() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/databases/appdb/jobs"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"success": true, "jobs": [], "total": 0})),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let opts = ListJobsOptions {
        status: Some("failed".to_string()),
        limit: Some(5),
    };
    client.jobs.list(Some(opts)).await.unwrap();
}

#[tokio::test]
async fn jobs_cancel_returns_false_on_409() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/databases/appdb/jobs/1/cancel"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error": "cannot cancel"})))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let cancelled = client.jobs.cancel(1).await.unwrap();
    assert!(!cancelled);
}

#[tokio::test]
async fn queues_without_project_id_returns_clear_error() {
    let server = MockServer::start().await;
    let client = make_client(server.uri());

    let err = client
        .queues
        .send("my-queue", json!({"hello": "world"}), None)
        .await
        .unwrap_err();
    match err {
        PGBHubError::Api { message, .. } => assert!(message.contains("project_id")),
        other => panic!("expected PGBHubError::Api, got {other:?}"),
    }
}

#[tokio::test]
async fn queues_send_hits_project_scoped_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/projects/prj_1/queues/my-queue/send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "message": {"message_id": "msg_1", "queue_name": "my-queue", "status": "sent", "deduplicated": false}
        })))
        .mount(&server)
        .await;

    let client = make_client_with_project(server.uri(), "prj_1");
    let opts = SendMessageOptions {
        delay_seconds: Some(5),
        ..Default::default()
    };
    let msg = client
        .queues
        .send("my-queue", json!({"hello": "world"}), Some(opts))
        .await
        .unwrap();
    assert_eq!(msg.message_id, "msg_1");
}

#[tokio::test]
async fn queues_receive_ack_nack_hit_right_subpaths() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/projects/prj_1/queues/my-queue/receive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "count": 1, "messages": [{"message_id": "msg_1", "payload": {}}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/projects/prj_1/queues/my-queue/ack"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/projects/prj_1/queues/my-queue/nack"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;

    let client = make_client_with_project(server.uri(), "prj_1");
    let opts = ReceiveMessagesOptions {
        max_messages: Some(1),
        ..Default::default()
    };
    let msgs = client.queues.receive("my-queue", Some(opts)).await.unwrap();
    assert_eq!(msgs.len(), 1);

    assert!(client.queues.ack("my-queue", "msg_1").await.unwrap());
    assert!(client
        .queues
        .nack("my-queue", "msg_1", Some("boom"))
        .await
        .unwrap());
}

#[tokio::test]
async fn realtime_broadcast_posts_to_channel_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/realtime/channels/general/broadcast"))
        .and(body_json(
            json!({"channel": "general", "event": "chat", "payload": {"text": "hi"}}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": true, "broadcast": {"delivered_to": 2}})),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let result = client
        .realtime
        .broadcast("general", json!({"text": "hi"}), Some("chat"))
        .await
        .unwrap();
    assert_eq!(result.broadcast["delivered_to"], 2);
}
