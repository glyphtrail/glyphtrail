//! Shared multi-provider LLM client for the generative commands (`wiki` #52,
//! `story` #114). Facts are gathered offline and deterministically by each
//! command; this module only turns a (system, user) prompt pair into text via a
//! configurable provider. Hand-rolled on `ureq` to keep the dependency surface
//! small.

use anyhow::{Result, anyhow};
use clap::ValueEnum;
use serde_json::{Value, json};

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Provider {
    /// Anthropic Claude (`ANTHROPIC_API_KEY`).
    #[default]
    Claude,
    /// OpenAI (`OPENAI_API_KEY`).
    Openai,
    /// OpenRouter, OpenAI-compatible (`OPENROUTER_API_KEY`).
    Openrouter,
}

pub struct Llm {
    provider: Provider,
    model: String,
    key: String,
    base_url: Option<String>,
}

impl Llm {
    /// Resolve the API key from the provider's env var and pick a default model.
    /// Errors (with the env var name) when the key is unset; commands offer a
    /// `--dry-run` that never constructs an `Llm`.
    pub fn new(
        provider: Provider,
        model: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self> {
        let (env, default_model) = match provider {
            Provider::Claude => ("ANTHROPIC_API_KEY", "claude-sonnet-4-6"),
            Provider::Openai => ("OPENAI_API_KEY", "gpt-4o"),
            Provider::Openrouter => ("OPENROUTER_API_KEY", "anthropic/claude-3.5-sonnet"),
        };
        let key = std::env::var(env)
            .map_err(|_| anyhow!("set {env} (or use --dry-run) to call the LLM"))?;
        Ok(Self {
            provider,
            model: model.unwrap_or_else(|| default_model.to_string()),
            key,
            base_url,
        })
    }

    pub fn complete(&self, system: &str, user: &str) -> Result<String> {
        match self.provider {
            Provider::Claude => self.anthropic(system, user),
            Provider::Openai | Provider::Openrouter => self.openai_compatible(system, user),
        }
    }

    fn anthropic(&self, system: &str, user: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "system": system,
            "messages": [{ "role": "user", "content": user }],
        });
        let resp: Value = ureq::post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", self.key.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .send_json(body)?
            .into_body()
            .read_json()?;
        resp["content"][0]["text"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("unexpected Anthropic response: {resp}"))
    }

    fn openai_compatible(&self, system: &str, user: &str) -> Result<String> {
        let url = self.base_url.clone().unwrap_or_else(|| {
            match self.provider {
                Provider::Openai => "https://api.openai.com/v1/chat/completions",
                _ => "https://openrouter.ai/api/v1/chat/completions",
            }
            .to_string()
        });
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });
        let resp: Value = ureq::post(&url)
            .header("Authorization", format!("Bearer {}", self.key))
            .header("content-type", "application/json")
            .send_json(body)?
            .into_body()
            .read_json()?;
        resp["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("unexpected response: {resp}"))
    }
}
