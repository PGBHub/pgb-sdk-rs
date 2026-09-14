# pgbhub-rs

Official Rust client SDK for **PGBHub Cloud**.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
pgbhub = { git = "https://github.com/Hydratik/pgbhub-rs" }
tokio = { version = "1.0", features = ["full"] }
```

## Quick Start

```rust
use pgbhub::{PGBHubClient, PGBHubConfig};
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = PGBHubClient::new(PGBHubConfig {
        api_key: Some("your_api_key".to_string()),
        endpoint: "https://cloud.pgbhub.com".to_string(),
        database: "my_database".to_string(),
    });

    let mut params = HashMap::new();
    params.insert("orderId".to_string(), serde_json::json!("ord_102"));

    let res = client.invoke_action("approveOrder", params, false).await?;
    println!("Status: {} (took {:.2} ms)", res.status, res.duration_ms);

    Ok(())
}
```

## License

MIT © [Hydratik](https://github.com/Hydratik)
