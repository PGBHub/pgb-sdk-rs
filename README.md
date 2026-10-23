# pgbhub-rs

Official Rust client SDK for **PGBHub Cloud**.

## Installation

```toml
[dependencies]
pgbhub = { git = "https://github.com/PGBHub/pgb-sdk-rs" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Quick Start

```rust
use pgbhub::{PGBHubClient, PGBHubConfig};
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // project_id alone targets https://my-project.pgbhub.app. The key picks
    // the database; set `database` only to name one explicitly.
    let client = PGBHubClient::new(PGBHubConfig {
        api_key: Some("your_api_key".to_string()),
        project_id: Some("my-project".to_string()),
        ..PGBHubConfig::default()
    });

    let mut params = HashMap::new();
    params.insert("orderId".to_string(), serde_json::json!("ord_102"));

    let res = client.invoke_action("approveOrder", params, false).await?;
    println!("Status: {} (took {:.2} ms)", res.status, res.duration_ms);
    Ok(())
}
```

## Configuration

`PGBHubConfig::default()` reads the environment: `PGBHUB_API_KEY` (or `PGBHUB_PUBLISHABLE_KEY`), `PGBHUB_PROJECT_ID`, `PGBHUB_DATABASE`. An empty `endpoint` resolves from `PGBHUB_ENDPOINT` / `PGBHUB_PROJECT_URL`, then the project address (domain from `PGBHUB_PROJECT_DOMAIN`, default `pgbhub.app`), then `https://console.pgbhub.com`.
`timeout_secs` (30), `max_retries` (2) and `retry_delay_ms` (300) tune requests. Reads retry on 502/503/504 and connection errors; writes never retry.

## Errors

Server failures are `PGBHubError::Api { status, message, code, retry_after }`, with the server's own message. `err.status()` and `err.code()` read them without a match; `retry_after` is seconds for rate limits.

## Auth

```rust
let auth = &client.auth;

let res = auth.sign_up_with_redirect("a@example.com", Some("a-long-password"), None, Some("https://app.example.com/welcome")).await?;
if res.verification_required { /* no session yet: the user must click the emailed link */ }

let login = auth.sign_in("a@example.com", Some("a-long-password")).await?;
if login.mfa_required {
    auth.verify_mfa(&login.challenge_token.unwrap(), "123456").await?;
}

auth.forgot_password("a@example.com", Some("https://app.example.com/reset")).await?;
auth.reset_password(&token_from_the_link, "a-new-long-password").await?;
auth.request_otp("a@example.com").await?;   auth.verify_otp("a@example.com", "123456").await?;
auth.send_magic_link_to("a@example.com", Some("https://app.example.com/in")).await?;

auth.change_password(Some("old"), "new").await?;
auth.update_user(serde_json::json!({"plan": "pro"})).await?;
auth.get_events().await?;  auth.list_sessions().await?;  auth.revoke_other_sessions().await?;
auth.delete_account(Some("password")).await?;

auth.oauth_start("google", "https://app.example.com/callback").await?; // authorize_url, state
auth.get_settings().await?; // which methods are on, password policy, providers
```

One-time codes and links are only ever emailed; the API never returns them.

## Actions

```rust
let preview = client.actions.run("saveUser", params, true).await?; // dry run
println!("{:?}", preview.effects_preview);

client.actions.register(RegisterActionOptions {
    name: "signup".into(),
    sql: "INSERT INTO users (email) VALUES (%(email)s) RETURNING id".into(),
    effects: Some(vec![
        serde_json::json!({"type": "mail", "to": "{{params.email}}", "subject": "Welcome", "body": "Hi!"}),
        serde_json::json!({"type": "queue", "queue": "onboarding", "payload": {"user": "{{results.0.id}}"}}),
    ]),
    ..Default::default()
}).await?;
client.actions.list().await?;  client.actions.get("signup").await?;  client.actions.delete("signup").await?;
client.actions.deliveries(DeliveryFilter { status: Some("failed".into()), ..Default::default() }).await?;
client.actions.retry_delivery(12).await?;
```

Effects run after the SQL commits, retry on their own, and can target a function, queue, mail, event, job, realtime channel, container, another action, or a webhook.

## Mail

```rust
use pgbhub::SendMailOptions;

match client.mail.send(SendMailOptions {
    to: "alex@example.com".into(),
    subject: "Your receipt".into(),
    text: "Thanks for your order.".into(),
    html: Some("<p>Thanks for your order.</p>".into()),
    reply_to: Some("support@acme.dev".into()), // optional: one address for the Reply-To header
    ..Default::default()
}).await {
    Ok(sent) => println!("{:?}", sent.message_id),
    // 503, code "MAIL_PROVIDER_UNAVAILABLE": no mail provider configured right now
    Err(e) if e.is_mail_provider_unavailable() => { /* retry later */ }
    Err(e) => return Err(e.into()),
}
```

Sending needs the project's **secret key**. `reply_to` is a single address: line breaks, commas and semicolons are refused with a 400. `message_id` identifies the sent message (on an older server it is filled from `mail_id`). Sends are never retried.

## License

MIT © [Hydratik](https://github.com/Hydratik)
