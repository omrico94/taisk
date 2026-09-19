//! Ollama client — trait-based so the categorization pipeline (M5) can be
//! unit-tested against a fake implementation without a live Ollama process
//! (plan §5/§11: most milestones don't need Ollama running; this is the one
//! that provides the seam).
//!
//! Two separate calls, deliberately not conflated into one (see plan §5's
//! correction to the original PRD draft): an embedding-model call and an
//! instruct-model call are different Ollama endpoints and typically different
//! models (`nomic-embed-text` vs `qwen2.5:1.5b`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// How long Ollama should keep a model loaded between requests. Passed on
/// every call instead of running a separate keep-alive-ping scheduler —
/// Ollama already supports this natively (plan §5).
const KEEP_ALIVE: &str = "30m";

#[derive(Debug)]
pub enum OllamaError {
    Http(String),
    UnexpectedResponse(String),
}

impl std::fmt::Display for OllamaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OllamaError::Http(msg) => write!(f, "Ollama HTTP error: {msg}"),
            OllamaError::UnexpectedResponse(msg) => write!(f, "unexpected Ollama response: {msg}"),
        }
    }
}

impl std::error::Error for OllamaError {}

impl From<reqwest::Error> for OllamaError {
    fn from(e: reqwest::Error) -> Self {
        OllamaError::Http(e.to_string())
    }
}

/// Trait boundary the categorization pipeline depends on, so it can run
/// against `FakeOllamaClient` in tests and `HttpOllamaClient` for real.
#[async_trait]
pub trait OllamaClient: Send + Sync {
    async fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, OllamaError>;
    async fn generate(&self, model: &str, prompt: &str) -> Result<String, OllamaError>;
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    keep_alive: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
    keep_alive: &'a str,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

pub struct HttpOllamaClient {
    base_url: String,
    http: reqwest::Client,
}

impl HttpOllamaClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Default local Ollama endpoint.
    pub fn local() -> Self {
        Self::new("http://127.0.0.1:11434")
    }

    /// Whether Ollama is reachable at all — used by the first-run flow (§9)
    /// to decide whether to show install instructions.
    pub async fn is_reachable(&self) -> bool {
        self.http
            .get(format!("{}/api/version", self.base_url))
            .send()
            .await
            .is_ok()
    }

    /// Names of models already pulled (per Ollama's own `/api/tags`), e.g.
    /// `"nomic-embed-text:latest"`.
    pub async fn list_models(&self) -> Result<Vec<String>, OllamaError> {
        let resp = self.http.get(format!("{}/api/tags", self.base_url)).send().await?.error_for_status()?;
        let parsed: TagsResponse = resp
            .json()
            .await
            .map_err(|e| OllamaError::UnexpectedResponse(e.to_string()))?;
        Ok(parsed.models.into_iter().map(|m| m.name).collect())
    }

    /// Streams `ollama pull`'s own NDJSON progress events (plan §9's
    /// "Pull models" action) via `on_progress`, called once per line.
    pub async fn pull_model(
        &self,
        model: &str,
        mut on_progress: impl FnMut(serde_json::Value) + Send,
    ) -> Result<(), OllamaError> {
        use futures::StreamExt;

        let resp = self
            .http
            .post(format!("{}/api/pull", self.base_url))
            .json(&serde_json::json!({ "name": model }))
            .send()
            .await?
            .error_for_status()?;

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) {
                    on_progress(v);
                }
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    name: String,
}

#[async_trait]
impl OllamaClient for HttpOllamaClient {
    async fn embed(&self, model: &str, text: &str) -> Result<Vec<f32>, OllamaError> {
        let resp = self
            .http
            .post(format!("{}/api/embeddings", self.base_url))
            .json(&EmbedRequest {
                model,
                prompt: text,
                keep_alive: KEEP_ALIVE,
            })
            .send()
            .await?
            .error_for_status()?
            .json::<EmbedResponse>()
            .await
            .map_err(|e| OllamaError::UnexpectedResponse(e.to_string()))?;
        Ok(resp.embedding)
    }

    async fn generate(&self, model: &str, prompt: &str) -> Result<String, OllamaError> {
        let resp = self
            .http
            .post(format!("{}/api/generate", self.base_url))
            .json(&GenerateRequest {
                model,
                prompt,
                stream: false,
                keep_alive: KEEP_ALIVE,
            })
            .send()
            .await?
            .error_for_status()?
            .json::<GenerateResponse>()
            .await
            .map_err(|e| OllamaError::UnexpectedResponse(e.to_string()))?;
        Ok(resp.response)
    }
}

/// Deterministic stand-in for a real Ollama connection. Used both by tests
/// across the crate (M5's fixture harness, M6's API tests, etc.) and by the
/// standalone dev-server example as a fallback when Ollama isn't installed
/// (plan §11) — so this module is always compiled, not test-only.
pub mod fake {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    /// Deterministic test double: `embed` hashes the input text into a fixed
    /// direction so identical text always yields identical vectors (useful
    /// for categorization-pipeline tests in M5 without depending on a real
    /// model's actual semantics), and `generate` returns a canned label.
    pub struct FakeOllamaClient {
        pub canned_labels: Mutex<HashMap<String, String>>,
        pub default_label: String,
    }

    impl FakeOllamaClient {
        pub fn new(default_label: impl Into<String>) -> Self {
            Self {
                canned_labels: Mutex::new(HashMap::new()),
                default_label: default_label.into(),
            }
        }

        /// Builds a client whose `generate()` always returns a canned JSON
        /// response matching the summary pipeline's expected shape
        /// (`{"task_summary"}`) — for testing `summarize_session`, which
        /// parses `generate()`'s output as JSON rather than a plain label.
        pub fn new_summarizing(task_summary: impl Into<String>) -> Self {
            let json = serde_json::json!({ "task_summary": task_summary.into() }).to_string();
            Self::new(json)
        }
    }

    #[async_trait]
    impl OllamaClient for FakeOllamaClient {
        async fn embed(&self, _model: &str, text: &str) -> Result<Vec<f32>, OllamaError> {
            // Simple, deterministic "embedding": every dimension derived from
            // a cheap hash of the text, normalized-ish by construction (all
            // dims share the same sign/magnitude so cosine distance behaves
            // sensibly in tests without needing a real model).
            let mut hash: u64 = 1469598103934665603; // FNV offset basis
            for b in text.bytes() {
                hash ^= b as u64;
                hash = hash.wrapping_mul(1099511628211);
            }
            let sign = if hash % 2 == 0 { 1.0 } else { -1.0 };
            Ok(vec![sign; 768])
        }

        async fn generate(&self, _model: &str, prompt: &str) -> Result<String, OllamaError> {
            let labels = self.canned_labels.lock().await;
            Ok(labels.get(prompt).cloned().unwrap_or_else(|| self.default_label.clone()))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn fake_embed_is_deterministic() {
            let client = FakeOllamaClient::new("Uncategorized");
            let a = client.embed("nomic-embed-text", "hello world").await.unwrap();
            let b = client.embed("nomic-embed-text", "hello world").await.unwrap();
            assert_eq!(a, b);
            assert_eq!(a.len(), 768);
        }

        #[tokio::test]
        async fn fake_generate_returns_default_label() {
            let client = FakeOllamaClient::new("Backend / API");
            let label = client.generate("qwen2.5:1.5b", "some prompt").await.unwrap();
            assert_eq!(label, "Backend / API");
        }
    }
}

/// Live-Ollama smoke test — `#[ignore]`d so the default `cargo test` run
/// (which most milestones rely on) never depends on Ollama actually being
/// installed (plan §11: Ollama isn't required until M5). Run explicitly with
/// `cargo test -p core-engine ollama:: -- --ignored` once Ollama + the two
/// required models are pulled.
#[cfg(test)]
mod live_smoke_test {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn embeds_and_generates_against_real_ollama() {
        let client = HttpOllamaClient::local();
        assert!(client.is_reachable().await, "Ollama must be running at 127.0.0.1:11434");

        let embedding = client
            .embed("nomic-embed-text", "refactor auth middleware to async/await")
            .await
            .expect("embed call should succeed");
        assert!(!embedding.is_empty());

        let label = client
            .generate(
                "qwen2.5:1.5b",
                "In 1-3 words, what kind of software work is this: 'refactor auth middleware to async/await'?",
            )
            .await
            .expect("generate call should succeed");
        assert!(!label.trim().is_empty());
    }
}
