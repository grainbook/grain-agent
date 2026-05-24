//! `refresh-models` — fetch `models.dev/api.json` and overwrite the vendored
//! snapshot at `data/models-dev.json`.
//!
//! Run from the workspace root:
//!
//! ```bash
//! cargo run -p grain-llm-models --features fetch --bin refresh-models
//! ```
//!
//! Output is sorted by model id so diffs between refreshes stay reviewable.

use std::path::PathBuf;

use grain_llm_models::{MODELS_DEV_URL, fetch_models_dev, registry_to_snapshot};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("MODELS_DEV_URL").unwrap_or_else(|_| MODELS_DEV_URL.to_string());
    eprintln!("fetching {url}");

    let registry = if url == MODELS_DEV_URL {
        fetch_models_dev().await?
    } else {
        grain_llm_models::fetch_from_url(&url).await?
    };

    let snapshot = registry_to_snapshot(&registry);
    let mut json = serde_json::to_string_pretty(&snapshot)?;
    json.push('\n');

    let out_path = snapshot_path();
    std::fs::write(&out_path, json)?;

    eprintln!(
        "wrote {} models to {}",
        snapshot.models.len(),
        out_path.display()
    );
    Ok(())
}

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("models-dev.json")
}
