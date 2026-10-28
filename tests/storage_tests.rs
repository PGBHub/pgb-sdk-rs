//! Storage: signed URLs (POST /storage/sign), uploads to a signed URL, and
//! private buckets (create with a visibility; going public needs
//! confirm_public), against a wiremock server.

use pgbhub::{
    BucketVisibility, PGBHubClient, PGBHubConfig, SignedUrlMethod, SignedUrlOptions, CONFIRM_PUBLIC_REQUIRED,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn client(endpoint: String, project_id: Option<&str>) -> PGBHubClient {
    PGBHubClient::new(PGBHubConfig {
        endpoint,
        api_key: Some("k".to_string()),
        database: String::new(),
        project_id: project_id.map(|p| p.to_string()),
        retry_delay_ms: 1,
        ..PGBHubConfig::default()
    })
}

fn signed_body(method: &str) -> serde_json::Value {
    json!({
        "success": true,
        "url": "https://acme.pgbhub.app/storage/_signed/tok",
        "path": "/storage/_signed/tok",
        "method": method,
        "bucket": "receipts",
        "file_path": "2026/inv-1.pdf",
        "expires_in": 900,
        "expires_at": 1700000000,
        "content_type": null,
        "bucket_visibility": "private"
    })
}

#[tokio::test]
async fn create_signed_url_defaults_to_get_with_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storage/sign"))
        .and(header("Authorization", "Bearer k"))
        .and(body_json(json!({"bucket": "receipts", "path": "2026/inv-1.pdf", "method": "GET"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(signed_body("GET")))
        .expect(1)
        .mount(&server)
        .await;
    let out = client(server.uri(), None)
        .storage
        .create_signed_url("receipts", "/2026/inv-1.pdf", SignedUrlOptions::default())
        .await
        .unwrap();
    assert_eq!(out.url, "https://acme.pgbhub.app/storage/_signed/tok");
    assert_eq!(out.method, SignedUrlMethod::Get);
    assert_eq!(out.expires_at, 1700000000);
    assert_eq!(out.bucket_visibility, BucketVisibility::Private);
    assert_eq!(out.content_type, None);
}

#[tokio::test]
async fn create_signed_url_put_sends_expiry_and_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storage/sign"))
        .and(body_json(json!({
            "bucket": "uploads", "path": "a.png", "method": "PUT", "expires_in": 600, "content_type": "image/png"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(signed_body("PUT")))
        .expect(1)
        .mount(&server)
        .await;
    let out = client(server.uri(), None)
        .storage
        .create_signed_url(
            "uploads",
            "a.png",
            SignedUrlOptions {
                method: SignedUrlMethod::Put,
                expires_in_secs: Some(600),
                content_type: Some("image/png".to_string()),
            },
        )
        .await
        .unwrap();
    assert_eq!(out.method, SignedUrlMethod::Put);
}

#[tokio::test]
async fn content_type_on_get_is_refused_without_a_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let err = client(server.uri(), None)
        .storage
        .create_signed_url(
            "b",
            "a",
            SignedUrlOptions { content_type: Some("image/png".to_string()), ..SignedUrlOptions::default() },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("invalid_request"));
}

#[tokio::test]
async fn server_refusal_is_an_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storage/sign"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "Bucket 'nope' not found."})))
        .mount(&server)
        .await;
    let err = client(server.uri(), None)
        .storage
        .create_signed_url("nope", "a", SignedUrlOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(400));
    assert!(err.to_string().contains("Bucket 'nope' not found."));
}

#[tokio::test]
async fn upload_to_signed_url_puts_bytes_without_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/storage/_signed/tok"))
        .and(header("Content-Type", "image/png"))
        .and(|req: &Request| !req.headers.contains_key("authorization") && req.body == b"abc")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"success": true, "bucket": "uploads", "file_path": "a.png", "size_bytes": 3})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let c = client(server.uri(), None);
    let out = c.storage.upload_to_signed_url("/storage/_signed/tok", b"abc".to_vec(), "image/png").await.unwrap();
    assert!(out.success);
    assert_eq!(out.size_bytes, 3);
    assert_eq!(out.file_path, "a.png");
}

#[tokio::test]
async fn upload_refusal_is_an_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(415).set_body_json(json!({"error": "This content type can only be uploaded through a signed URL minted for it (content_type)."})))
        .mount(&server)
        .await;
    let url = format!("{}/storage/_signed/tok", server.uri());
    let err = client(server.uri(), None)
        .storage
        .upload_to_signed_url(&url, "<svg/>", "image/svg+xml")
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(415));
}

#[tokio::test]
async fn create_bucket_default_is_public_and_private_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/projects/prj_1/storage/buckets"))
        .and(body_json(json!({"name": "assets", "visibility": "public"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"success": true, "bucket": "assets", "visibility": "public"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/projects/prj_1/storage/buckets"))
        .and(body_json(json!({"name": "receipts", "visibility": "private"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"success": true, "bucket": "receipts", "visibility": "private"})))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(server.uri(), Some("prj_1"));
    let b = c.storage.create_bucket("assets", BucketVisibility::default()).await.unwrap();
    assert_eq!((b.name.as_str(), b.visibility), ("assets", BucketVisibility::Public));
    let b = c.storage.create_bucket("receipts", BucketVisibility::Private).await.unwrap();
    assert_eq!(b.visibility, BucketVisibility::Private);
}

#[tokio::test]
async fn bucket_calls_without_a_project_explain() {
    let c = client("https://console.pgbhub.com".to_string(), None);
    let err = c.storage.create_bucket("x", BucketVisibility::Private).await.unwrap_err();
    assert_eq!(err.code(), Some("missing_project_id"));
}

#[tokio::test]
async fn set_private_needs_no_confirmation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/projects/prj_1/storage/buckets/receipts/visibility"))
        .and(body_json(json!({"visibility": "private"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "bucket": "receipts", "visibility": "private", "previous_visibility": "public", "note": "n"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let out = client(server.uri(), Some("prj_1"))
        .storage
        .set_bucket_visibility("receipts", BucketVisibility::Private, false)
        .await
        .unwrap();
    assert_eq!(out.previous_visibility, BucketVisibility::Public);
    assert_eq!(out.note.as_deref(), Some("n"));
}

#[tokio::test]
async fn set_public_needs_confirm_public() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/projects/prj_1/storage/buckets/assets/visibility"))
        .and(body_json(json!({"visibility": "public", "confirm_public": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "bucket": "assets", "visibility": "public", "previous_visibility": "private"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(server.uri(), Some("prj_1"));
    let err = c.storage.set_bucket_visibility("assets", BucketVisibility::Public, false).await.unwrap_err();
    assert_eq!(err.code(), Some(CONFIRM_PUBLIC_REQUIRED));
    assert_eq!(err.status(), None);
    assert!(err.to_string().contains("confirm_public = true"));
    let out = c.storage.set_bucket_visibility("assets", BucketVisibility::Public, true).await.unwrap();
    assert_eq!(out.visibility, BucketVisibility::Public);
}
