//! `semantic-search` tool — rig-backed semantic code search.
//!
//! Gated behind the `rig` cargo feature. Pulls in `rig-core` 0.37 with the
//! `derive` feature (for the `Embed` proc-macro) and connects to OpenAI to
//! produce embeddings.
//!
//! Design choices (v1):
//! - **One document per file**, no chunking. Skips files larger than
//!   [`SemanticIndexConfig::max_file_bytes`] (default 100 KiB) and files
//!   whose extension isn't in the text-y allowlist.
//! - **Lazy indexing**: the index is built on first invocation and reused
//!   for the lifetime of the tool instance. Subsequent calls reuse the
//!   embeddings (no rebuild for the same agent run).
//! - **In-memory store** only. For larger repos, switch to a persistent
//!   vector store from one of rig's vector-DB crates in a follow-up.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use grain_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, ToolDefinition, ToolUpdateCallback, UserContent,
};
use ignore::WalkBuilder;
use rig::Embed;
use rig::client::{EmbeddingsClient, ProviderClient};
use rig::embeddings::EmbeddingsBuilder;
use rig::providers::openai;
use rig::vector_store::VectorStoreIndex;
use rig::vector_store::in_memory_store::InMemoryVectorStore;
use rig::vector_store::request::VectorSearchRequest;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

const DEFAULT_TEXT_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "txt", "json", "yaml", "yml", "py", "js", "ts", "tsx", "jsx", "go", "java",
    "c", "cc", "cpp", "h", "hpp", "rb", "sh", "kt", "swift", "html", "css", "scss", "sql", "proto",
    "graphql", "lua", "zig", "ml", "scala",
];

const DEFAULT_MAX_FILE_BYTES: u64 = 100 * 1024;
const DEFAULT_TOP_N: usize = 5;

#[derive(Debug, Clone)]
pub struct SemanticIndexConfig {
    /// File-name extensions to consider for indexing (without leading `.`).
    pub allowed_extensions: HashSet<String>,
    /// Files larger than this are skipped.
    pub max_file_bytes: u64,
    /// OpenAI embedding model identifier (e.g. `text-embedding-3-small`).
    pub embedding_model: String,
}

impl Default for SemanticIndexConfig {
    fn default() -> Self {
        SemanticIndexConfig {
            allowed_extensions: DEFAULT_TEXT_EXTENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            embedding_model: openai::TEXT_EMBEDDING_3_SMALL.to_string(),
        }
    }
}

#[derive(Debug, Clone, Embed, Serialize, Deserialize, Eq, PartialEq)]
struct FileDoc {
    /// Workspace-relative path; doubles as the document id.
    id: String,
    /// Indexed text content.
    #[embed]
    content: String,
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    top_n: Option<usize>,
}

pub struct SemanticSearchTool {
    def: ToolDefinition,
    workspace: Arc<Workspace>,
    config: SemanticIndexConfig,
    client: openai::Client,
    /// Lazily-built index. `None` until the first call succeeds.
    index_slot: Mutex<Option<BuiltIndex>>,
}

struct BuiltIndex {
    store: InMemoryVectorStore<FileDoc>,
    model: openai::EmbeddingModel,
    doc_count: usize,
}

impl SemanticSearchTool {
    /// Build the tool using the OpenAI client from `OPENAI_API_KEY`.
    /// Returns an error if the env var is unset / empty.
    pub fn from_env(
        workspace: Arc<Workspace>,
        config: SemanticIndexConfig,
    ) -> Result<Self, SemanticInitError> {
        let client =
            openai::Client::from_env().map_err(|e| SemanticInitError::Client(e.to_string()))?;
        Ok(Self::new(workspace, config, client))
    }

    pub fn new(
        workspace: Arc<Workspace>,
        config: SemanticIndexConfig,
        client: openai::Client,
    ) -> Self {
        SemanticSearchTool {
            def: ToolDefinition {
                name: "semantic_search".into(),
                label: "Semantic Search".into(),
                description: format!(
                    "Find files in the workspace whose content is semantically related to a \
                     natural-language query. Backed by OpenAI embeddings ({}); files larger than \
                     {}KB and binary / unknown-extension files are skipped. The index is built \
                     lazily on first call and reused for the rest of the session.",
                    config.embedding_model,
                    config.max_file_bytes / 1024
                ),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Natural-language description of the code or text you want to find."
                        },
                        "top_n": {
                            "type": "integer",
                            "description": format!("Max number of results (default {DEFAULT_TOP_N}).")
                        }
                    },
                    "required": ["query"]
                }),
                execution_mode: None,
            },
            workspace,
            config,
            client,
            index_slot: Mutex::new(None),
        }
    }

    async fn ensure_index(&self) -> Result<usize, SemanticInitError> {
        let mut guard = self.index_slot.lock().await;
        if let Some(index) = guard.as_ref() {
            return Ok(index.doc_count);
        }

        let docs = collect_documents(&self.workspace, &self.config)
            .map_err(|e| SemanticInitError::Walk(e.to_string()))?;
        if docs.is_empty() {
            return Err(SemanticInitError::EmptyIndex);
        }

        let model = self.client.embedding_model(&self.config.embedding_model);
        let embeddings = EmbeddingsBuilder::new(model.clone())
            .documents(docs.clone())
            .map_err(|e| SemanticInitError::Embed(e.to_string()))?
            .build()
            .await
            .map_err(|e| SemanticInitError::Embed(e.to_string()))?;

        let store = InMemoryVectorStore::from_documents_with_id_f(embeddings, |doc| doc.id.clone());

        let doc_count = docs.len();
        *guard = Some(BuiltIndex {
            store,
            model,
            doc_count,
        });
        Ok(doc_count)
    }
}

#[async_trait]
impl AgentTool for SemanticSearchTool {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }

    async fn execute(
        &self,
        _id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> Result<AgentToolResult, AgentToolError> {
        let args: SearchArgs =
            serde_json::from_value(args).map_err(|e| AgentToolError::Validation(e.to_string()))?;
        let top_n = args.top_n.unwrap_or(DEFAULT_TOP_N).max(1);

        // Build (or reuse) the index. Errors here are user-actionable — surface
        // the message so the model can react.
        let doc_count = self
            .ensure_index()
            .await
            .map_err(|e| AgentToolError::msg(e.to_string()))?;

        let guard = self.index_slot.lock().await;
        let built = guard
            .as_ref()
            .expect("ensure_index populated the slot just above");

        let req = VectorSearchRequest::builder()
            .query(args.query.clone())
            .samples(top_n as u64)
            .build();

        let index = built.store.clone().index(built.model.clone());
        let results = index
            .top_n::<FileDoc>(req)
            .await
            .map_err(|e| AgentToolError::msg(format!("vector search: {e}")))?;

        let mut lines = Vec::with_capacity(results.len());
        for (score, _id, doc) in &results {
            lines.push(format!("score={score:.4} {}", doc.id));
        }
        let body = if lines.is_empty() {
            "(no matches)\n".to_string()
        } else {
            let mut s = lines.join("\n");
            s.push('\n');
            s
        };

        let summary = format!(
            "[index: {doc_count} files] top {} results:\n{body}",
            results.len()
        );

        Ok(AgentToolResult {
            content: vec![UserContent::text(summary)],
            details: serde_json::json!({
                "query": args.query,
                "topN": top_n,
                "results": results.len(),
                "indexedFiles": doc_count,
                "model": self.config.embedding_model,
            }),
            terminate: None,
        })
    }
}

/// Walk the workspace and collect indexable file documents. Pure (no I/O
/// to the embedding API) so it can be exercised in tests without the
/// `rig` feature triggering a real OpenAI call.
fn collect_documents(
    workspace: &Workspace,
    config: &SemanticIndexConfig,
) -> Result<Vec<FileDoc>, std::io::Error> {
    let mut docs = Vec::new();
    for entry in WalkBuilder::new(workspace.root()).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        let ext = match path.extension().and_then(|s| s.to_str()) {
            Some(e) => e.to_ascii_lowercase(),
            None => continue,
        };
        if !config.allowed_extensions.contains(&ext) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > config.max_file_bytes {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // binary / invalid UTF-8 — skip
        };
        if content.trim().is_empty() {
            continue;
        }
        let id = workspace.display_relative(path);
        docs.push(FileDoc { id, content });
    }
    Ok(docs)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SemanticInitError {
    #[error("rig openai client: {0}")]
    Client(String),
    #[error("workspace walk: {0}")]
    Walk(String),
    #[error("embedding build: {0}")]
    Embed(String),
    #[error("no indexable files found in workspace")]
    EmptyIndex,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn workspace_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Arc<Workspace>) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (rel, body) in files {
            let p = dir.path().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&p, body).expect("write");
        }
        let ws = Arc::new(Workspace::new(dir.path()).expect("workspace"));
        (dir, ws)
    }

    #[test]
    fn collect_documents_filters_by_extension() {
        let (_dir, ws) = workspace_with(&[
            ("src/foo.rs", "fn main() {}"),
            ("notes.md", "# header"),
            ("binary.exe", "binary content here"),
            ("photo.png", "not an image really"),
        ]);
        let cfg = SemanticIndexConfig::default();
        let docs = collect_documents(&ws, &cfg).expect("ok");
        let ids: Vec<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"src/foo.rs"));
        assert!(ids.contains(&"notes.md"));
        assert!(!ids.contains(&"binary.exe"));
        assert!(!ids.contains(&"photo.png"));
    }

    #[test]
    fn collect_documents_skips_oversize_files() {
        let big = "x".repeat(200 * 1024);
        let (_dir, ws) = workspace_with(&[("big.rs", &big), ("small.rs", "fn x(){}")]);
        let cfg = SemanticIndexConfig {
            max_file_bytes: 100 * 1024,
            ..SemanticIndexConfig::default()
        };
        let docs = collect_documents(&ws, &cfg).expect("ok");
        let ids: Vec<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"small.rs"));
        assert!(!ids.contains(&"big.rs"));
    }

    #[test]
    fn collect_documents_skips_empty_files() {
        let (_dir, ws) = workspace_with(&[("empty.rs", "  \n  "), ("real.rs", "fn x(){}")]);
        let cfg = SemanticIndexConfig::default();
        let docs = collect_documents(&ws, &cfg).expect("ok");
        let ids: Vec<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"real.rs"));
        assert!(!ids.contains(&"empty.rs"));
    }

    #[test]
    fn default_extension_set_includes_common_languages() {
        let cfg = SemanticIndexConfig::default();
        for ext in ["rs", "py", "ts", "go", "md", "toml"] {
            assert!(cfg.allowed_extensions.contains(ext), "missing: {ext}");
        }
    }
}
