use crate::core::{
    config::Config, gemini, ollama, session::ChatMessage, EngineEvent, TurnConfig,
};
use anyhow::Result;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Ollama,
    Gemini,
}

impl Provider {
    pub fn from_model(model: &str) -> Self {
        let m = model.trim();
        if m.starts_with("gemini-") || m.starts_with("models/gemini-") {
            Self::Gemini
        } else {
            Self::Ollama
        }
    }

    pub fn timeouts(&self, config: &Config) -> (Duration, Duration) {
        match self {
            Self::Ollama => (
                Duration::from_secs(config.ollama.status_timeout.max(5)),
                Duration::from_secs(config.ollama.chat_timeout),
            ),
            Self::Gemini => (
                Duration::from_secs(config.gemini.status_timeout.max(5)),
                Duration::from_secs(config.gemini.chat_timeout),
            ),
        }
    }

    pub fn build_client(&self, config: &Config) -> Result<Client> {
        let (connect_timeout, read_timeout) = self.timeouts(config);
        Ok(Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()?)
    }

    pub fn effective_default_ctx(&self, configured_ctx: u32) -> u32 {
        match self {
            // If the user hasn't customized the 32k Ollama default, use Gemini's 1M context window
            Self::Gemini if configured_ctx == 32768 => 1_048_576,
            _ => configured_ctx,
        }
    }

    pub async fn run_preflight(&self, model: &str, config: &Config) -> Result<()> {
        match self {
            Self::Ollama => ollama::run_preflight(model, config).await,
            Self::Gemini => gemini::run_preflight(model, config).await,
        }
    }

    pub async fn stream_turn(
        &self,
        client: Client,
        model: String,
        config: Config,
        messages: Vec<ChatMessage>,
        turn: TurnConfig,
        tools_schema: Option<Vec<Value>>,
        tx: UnboundedSender<EngineEvent>,
    ) {
        match self {
            Self::Ollama => {
                let payload = ollama::build_payload(
                    &model,
                    &messages,
                    &turn.system_prompt,
                    turn.num_ctx,
                    tools_schema,
                );
                ollama::stream_chat(client, config.ollama.base_url, payload, tx).await;
            }
            Self::Gemini => {
                let Some(api_key) = config.gemini.resolve_api_key() else {
                    let _ = tx.send(EngineEvent::StreamError(
                        "Missing Gemini API key. Set GEMINI_API_KEY in .env or config.toml".into(),
                    ));
                    return;
                };
                let payload = gemini::build_payload(
                    &messages,
                    &turn.system_prompt,
                    tools_schema,
                    config.app.show_thinking,
                );
                gemini::stream_chat(
                    client,
                    config.gemini.base_url,
                    api_key,
                    model,
                    payload,
                    tx,
                )
                .await;
            }
        }
    }
}
