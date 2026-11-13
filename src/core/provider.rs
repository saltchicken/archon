use crate::{
    core::{
        EngineEvent, TurnConfig,
        config::{Config, EndpointSettings},
        gemini, ollama,
        session::ChatMessage,
    },
    tools,
};
use anyhow::Result;
use futures_util::{Stream, TryStreamExt};
use reqwest::{Client, RequestBuilder};
use std::{io, time::Duration};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::{
    codec::{FramedRead, LinesCodec, LinesCodecError},
    io::StreamReader,
};
use tracing::{error, info};

pub async fn open_line_stream(
    req: RequestBuilder,
    tx: &UnboundedSender<EngineEvent>,
) -> Option<impl Stream<Item = std::result::Result<String, LinesCodecError>> + Unpin> {
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            error!("HTTP Request failed: {e}");
            let _ = tx.send(EngineEvent::StreamError(format!("Request failed: {e}")));
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        error!("API returned error status {status}: {text}");
        let _ = tx.send(EngineEvent::StreamError(format!("HTTP {status}: {text}")));
        return None;
    }

    info!("Stream connection established successfully.");
    let byte_stream = resp.bytes_stream().map_err(io::Error::other);
    Some(FramedRead::new(
        StreamReader::new(byte_stream),
        LinesCodec::new(),
    ))
}

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

    pub fn endpoint<'a>(&self, config: &'a Config) -> &'a EndpointSettings {
        match self {
            Self::Ollama => &config.ollama,
            Self::Gemini => &config.gemini,
        }
    }

    pub fn build_client(&self, config: &Config) -> Result<Client> {
        let ep = self.endpoint(config);
        Ok(Client::builder()
            .connect_timeout(Duration::from_secs(ep.status_timeout.max(5)))
            .read_timeout(Duration::from_secs(ep.chat_timeout))
            .build()?)
    }

    pub fn fallback_ctx(&self) -> u32 {
        match self {
            Self::Ollama => 32_768,
            Self::Gemini => 1_048_576,
        }
    }

    pub fn fallback_max_tool_output(&self) -> usize {
        match self {
            Self::Ollama => 131_072,
            Self::Gemini => 524_288,
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
        tx: UnboundedSender<EngineEvent>,
    ) {
        let tools_schema = turn
            .enable_tools
            .then(|| tools::schemas(&config))
            .filter(|t| !t.is_empty());
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
                let payload = gemini::build_payload(&messages, &turn.system_prompt, tools_schema);
                gemini::stream_chat(client, config.gemini.base_url, api_key, model, payload, tx)
                    .await;
            }
        }
    }
}
