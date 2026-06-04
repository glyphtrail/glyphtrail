//! Pluggable embedding providers for the atlas embedding layer (#338).
//!
//! The default is local and lexical ([`glyphtrail_core::HashingEmbedder`]) —
//! deterministic, dependency-free, nothing leaves the machine. Atlas is local-only
//! by default, but embedding (like the wiki/story summaries) is one of the few
//! functions explicitly **allowed** to send data off-machine when the user opts in
//! and is told which host receives it. So a second provider speaks the
//! OpenAI-compatible `/v1/embeddings` API, which covers hosted models *and* a local
//! neural server (e.g. Ollama / llama.cpp via `--base-url http://localhost:…`) —
//! local-first, with a real model dropped in behind the same seam.
//!
//! The store records the active model id (and base URL) in `Meta`, so `atlas
//! similar` re-embeds a free-text query with the same provider the repos were
//! embedded under.

use anyhow::{Result, anyhow, bail};
use clap::ValueEnum;
use glyphtrail_core::{Embedder, HashingEmbedder};
use serde_json::{Value, json};

/// Where embedding vectors come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum EmbedProvider {
    /// Local lexical hashing embedder — no network, the default.
    #[default]
    Local,
    /// An OpenAI-compatible `/v1/embeddings` endpoint (hosted, or a local server
    /// via `--base-url`). Sends the text to be embedded to that endpoint.
    Openai,
}

/// A resolved embedding configuration: which provider, the model, and (for the
/// API) the endpoint. Built once per `embed`/`similar` invocation.
#[derive(Clone, Debug)]
pub struct EmbedConfig {
    pub provider: EmbedProvider,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Vector width for the local provider; ignored by API providers, which set
    /// their own dimension.
    pub dim: usize,
}

/// The OpenAI embeddings default endpoint.
const OPENAI_EMBEDDINGS_URL: &str = "https://api.openai.com/v1/embeddings";
/// The default OpenAI embedding model.
const OPENAI_DEFAULT_MODEL: &str = "text-embedding-3-small";

impl EmbedConfig {
    /// The stable model id recorded with each vector, so a later run can tell which
    /// provider/model produced the index (`lexical-hash-v1` or `openai:<model>`).
    pub fn model_id(&self) -> String {
        match self.provider {
            EmbedProvider::Local => HashingEmbedder::default().id().to_string(),
            EmbedProvider::Openai => {
                format!("openai:{}", self.openai_model())
            }
        }
    }

    fn openai_model(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| OPENAI_DEFAULT_MODEL.to_string())
    }

    /// The endpoint an OpenAI-compatible run will POST to.
    pub fn endpoint(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| OPENAI_EMBEDDINGS_URL.to_string())
    }

    /// Whether this config sends text off the machine. `Local` never does; an
    /// OpenAI-compatible endpoint pointed at localhost (Ollama/llama.cpp) doesn't
    /// either.
    pub fn is_offmachine(&self) -> bool {
        match self.provider {
            EmbedProvider::Local => false,
            EmbedProvider::Openai => !is_local_url(&self.endpoint()),
        }
    }

    /// A one-line description of where embeddings come from, for the transparency
    /// banner every command prints.
    pub fn describe(&self) -> String {
        match self.provider {
            EmbedProvider::Local => "local lexical (no network)".to_string(),
            EmbedProvider::Openai => {
                format!("{} via {}", self.openai_model(), host_of(&self.endpoint()))
            }
        }
    }
}

/// Reconstruct the config a stored index was embedded under, from its recorded
/// model id and base URL (`Meta`) plus the stored vector width, so `similar`
/// re-embeds a query into the same space.
pub fn config_from_stored(model_id: &str, base_url: Option<String>, dim: usize) -> EmbedConfig {
    if let Some(model) = model_id.strip_prefix("openai:") {
        EmbedConfig {
            provider: EmbedProvider::Openai,
            model: Some(model.to_string()),
            base_url,
            dim,
        }
    } else {
        EmbedConfig {
            provider: EmbedProvider::Local,
            model: None,
            base_url: None,
            dim,
        }
    }
}

/// Embed many documents in order. The local provider runs in-process; the
/// OpenAI-compatible provider batches them into one request.
pub fn embed_docs(cfg: &EmbedConfig, docs: &[String]) -> Result<Vec<Vec<f32>>> {
    match cfg.provider {
        EmbedProvider::Local => {
            let e = HashingEmbedder::new(cfg.dim);
            Ok(docs.iter().map(|d| e.embed(d)).collect())
        }
        EmbedProvider::Openai => openai_embed(cfg, docs),
    }
}

/// Embed a single text (a `similar` query) under `cfg`.
pub fn embed_one(cfg: &EmbedConfig, text: &str) -> Result<Vec<f32>> {
    Ok(embed_docs(cfg, std::slice::from_ref(&text.to_string()))?
        .into_iter()
        .next()
        .unwrap_or_default())
}

/// POST a batch to an OpenAI-compatible `/v1/embeddings` endpoint. The API key is
/// `OPENAI_API_KEY`; it's optional for a local endpoint (Ollama needs none) but
/// required for a remote one.
fn openai_embed(cfg: &EmbedConfig, docs: &[String]) -> Result<Vec<Vec<f32>>> {
    if docs.is_empty() {
        return Ok(Vec::new());
    }
    let url = cfg.endpoint();
    let key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    if key.is_none() && !is_local_url(&url) {
        bail!("set OPENAI_API_KEY to use a remote embeddings provider (or use --provider local)");
    }
    let body = json!({ "model": cfg.openai_model(), "input": docs });
    let mut req = ureq::post(&url).header("content-type", "application/json");
    if let Some(k) = &key {
        req = req.header("Authorization", format!("Bearer {k}"));
    }
    let resp: Value = req.send_json(body)?.into_body().read_json()?;
    let data = resp["data"]
        .as_array()
        .ok_or_else(|| anyhow!("unexpected embeddings response: {resp}"))?;
    if data.len() != docs.len() {
        bail!(
            "embeddings provider returned {} vectors for {} inputs",
            data.len(),
            docs.len()
        );
    }
    data.iter()
        .map(|d| {
            d["embedding"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(|x| x.as_f64().map(|f| f as f32))
                        .collect()
                })
                .ok_or_else(|| anyhow!("embedding row missing `embedding`: {d}"))
        })
        .collect()
}

/// Whether a URL targets the local machine (so an OpenAI-compatible server there
/// keeps data on-machine).
fn is_local_url(url: &str) -> bool {
    let host = host_of(url);
    host == "localhost" || host.starts_with("127.") || host == "::1" || host == "0.0.0.0"
}

/// The host portion of a URL, best-effort (no URL crate): strip scheme, take up to
/// the first `/` or `:`. Used by the transparency banner.
pub fn host_of(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    after_scheme
        .split(['/', ':'])
        .next()
        .unwrap_or(after_scheme)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn local_is_the_default_and_never_offmachine() {
        let cfg = EmbedConfig {
            provider: EmbedProvider::default(),
            model: None,
            base_url: None,
            dim: 256,
        };
        check!(cfg.provider == EmbedProvider::Local);
        check!(!cfg.is_offmachine());
        check!(cfg.model_id() == "lexical-hash-v1");
    }

    #[test]
    fn openai_model_id_and_offmachine() {
        let cfg = EmbedConfig {
            provider: EmbedProvider::Openai,
            model: Some("text-embedding-3-large".to_string()),
            base_url: None,
            dim: 256,
        };
        check!(cfg.model_id() == "openai:text-embedding-3-large");
        check!(cfg.is_offmachine());
        check!(cfg.describe().contains("api.openai.com"));
    }

    #[test]
    fn a_local_openai_endpoint_stays_on_machine() {
        let cfg = EmbedConfig {
            provider: EmbedProvider::Openai,
            model: Some("nomic-embed-text".to_string()),
            base_url: Some("http://localhost:11434/v1/embeddings".to_string()),
            dim: 256,
        };
        check!(!cfg.is_offmachine());
    }

    #[test]
    fn stored_model_id_round_trips_to_a_config() {
        let local = config_from_stored("lexical-hash-v1", None, 256);
        check!(local.provider == EmbedProvider::Local);
        let api = config_from_stored(
            "openai:text-embedding-3-small",
            Some("http://x".into()),
            1536,
        );
        check!(api.provider == EmbedProvider::Openai);
        check!(api.model.as_deref() == Some("text-embedding-3-small"));
    }

    #[test]
    fn host_extraction() {
        check!(host_of("https://api.openai.com/v1/embeddings") == "api.openai.com");
        check!(host_of("http://localhost:11434/v1/embeddings") == "localhost");
    }
}
