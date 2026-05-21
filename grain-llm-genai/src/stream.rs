//! [`grain_agent_core::LlmStream`] implementation backed by `genai 0.5`.
//!
//! The streaming logic lives here; the message ↔ event translation lives in
//! [`crate::mapping`]; the client construction + provider routing lives in
//! [`crate::builder`].

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use genai::chat::ChatOptions;
use grain_agent_core::{
    AssistantMessageEvent, AssistantStream, LlmContext, LlmStream, Model, StreamError,
    StreamOptions,
};
use grain_llm_models::Registry;
use tokio_util::sync::CancellationToken;

use crate::builder::GenaiStreamBuilder;
use crate::config::ProviderRouter;
use crate::mapping::inbound::InboundState;
use crate::mapping::outbound::{baseline_chat_options, to_chat_request};

/// [`LlmStream`] implementation backed by [`genai::Client`].
///
/// Build via [`GenaiStream::builder()`] for full configuration (env-var
/// key resolution, OpenAI-compat presets, model registry); [`GenaiStream::new`]
/// retains the zero-config behavior from PR 3b.
pub struct GenaiStream {
    client: genai::Client,
    chat_options: ChatOptions,
    provider_router: ProviderRouter,
    #[allow(dead_code)] // Reserved for harness hooks / future adapters.
    registry: Option<Arc<Registry>>,
}

impl Default for GenaiStream {
    fn default() -> Self {
        GenaiStream::new()
    }
}

impl GenaiStream {
    /// Zero-config: default genai client + [`baseline_chat_options`].
    pub fn new() -> Self {
        GenaiStream {
            client: genai::Client::default(),
            chat_options: baseline_chat_options(),
            provider_router: ProviderRouter::default(),
            registry: None,
        }
    }

    /// Start a configuration chain. See [`GenaiStreamBuilder`].
    pub fn builder() -> GenaiStreamBuilder {
        GenaiStreamBuilder::new()
    }

    /// Construct from a fully-configured client. Used by tests that want to
    /// inject a mock client without going through the builder.
    pub fn with_client_and_options(
        client: genai::Client,
        chat_options: ChatOptions,
    ) -> Self {
        GenaiStream {
            client,
            chat_options,
            provider_router: ProviderRouter::default(),
            registry: None,
        }
    }

    /// Construct from a builder-prepared client. Public for [`GenaiStreamBuilder::build`]
    /// to plumb its config in.
    pub fn with_client_options_and_router(
        client: genai::Client,
        chat_options: ChatOptions,
        provider_router: ProviderRouter,
        registry: Option<Arc<Registry>>,
    ) -> Self {
        GenaiStream {
            client,
            chat_options,
            provider_router,
            registry,
        }
    }

    /// Translate a grain model id (`"anthropic/claude-sonnet-4-5"`) into the
    /// `"<namespace>::<model>"` form genai dispatches on. Provider names with
    /// no `/` pass through unchanged so callers can also feed genai-native
    /// identifiers directly.
    pub fn translate_model_id(&self, model_id: &str) -> String {
        match model_id.split_once('/') {
            Some((provider, name)) => {
                let ns = self.provider_router.namespace_for(provider);
                format!("{ns}::{name}")
            }
            None => model_id.to_string(),
        }
    }
}

#[async_trait]
impl LlmStream for GenaiStream {
    async fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        _options: &StreamOptions,
        cancel: CancellationToken,
    ) -> Result<AssistantStream, StreamError> {
        let chat_req = to_chat_request(context);
        let chat_options = self.chat_options.clone();
        let model_for_genai = self.translate_model_id(&model.id);

        let stream_resp = match self
            .client
            .exec_chat_stream(&model_for_genai, chat_req, Some(&chat_options))
            .await
        {
            Ok(r) => r,
            Err(err) => {
                // `LlmStream` contract: don't return `Err` for runtime failures.
                // Synthesize a terminal Error event with the failure message.
                let state = InboundState::new(model);
                let event = state.into_error_msg(format!("genai exec_chat_stream: {err}"));
                let one_shot = futures::stream::iter(std::iter::once(event));
                return Ok(Box::pin(one_shot));
            }
        };

        let model_for_state = model.clone();
        let inner = stream_resp.stream;

        let out = async_stream::stream! {
            let mut state = InboundState::new(&model_for_state);
            let mut inner = inner;
            let cancel = cancel.clone();

            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        yield state.into_aborted();
                        break;
                    }
                    event = inner.next() => {
                        match event {
                            Some(Ok(ev)) => {
                                let mut terminal = false;
                                for grain_event in state.on_event(ev) {
                                    if matches!(
                                        grain_event,
                                        AssistantMessageEvent::Done { .. }
                                            | AssistantMessageEvent::Error { .. }
                                    ) {
                                        terminal = true;
                                    }
                                    yield grain_event;
                                }
                                if terminal {
                                    break;
                                }
                            }
                            Some(Err(err)) => {
                                yield state.into_error_msg(format!("genai stream error: {err}"));
                                break;
                            }
                            None => {
                                yield state.into_error_msg("stream ended without terminal event");
                                break;
                            }
                        }
                    }
                }
            }
        };

        Ok(Box::pin(out))
    }
}
