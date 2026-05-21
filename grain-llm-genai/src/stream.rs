//! [`grain_agent_core::LlmStream`] implementation backed by `genai 0.5`.
//!
//! The actual streaming logic lives here; the message ↔ event translation
//! lives under [`crate::mapping`]. This file mostly orchestrates:
//!
//! 1. Translate the incoming [`LlmContext`] into a `genai::chat::ChatRequest`.
//! 2. Call `Client::exec_chat_stream`.
//! 3. Wrap the returned `ChatStream` with our [`InboundState`] state machine
//!    and surface each genai event as the appropriate grain
//!    [`AssistantMessageEvent`].
//! 4. Honor the [`tokio_util::sync::CancellationToken`]: when cancelled, drop
//!    the upstream stream and emit a terminal `Aborted` error event.

use async_trait::async_trait;
use futures::StreamExt;
use genai::chat::ChatOptions;
use grain_agent_core::{
    AssistantMessageEvent, AssistantStream, LlmContext, LlmStream, Model, StreamError,
    StreamOptions,
};
use tokio_util::sync::CancellationToken;

use crate::mapping::inbound::InboundState;
use crate::mapping::outbound::{baseline_chat_options, to_chat_request};

/// [`LlmStream`] implementation backed by [`genai::Client`].
///
/// PR 3b ships the minimal viable wrapper:
/// - Auto-detect provider from the model id (genai's default behavior).
/// - Default [`ChatOptions`] enable content / usage / tool-call / reasoning
///   capture so the terminal `StreamEnd` carries usage even when streaming.
///
/// PR 3c will introduce `GenaiStreamBuilder` with the env-var key resolver,
/// OpenAI-compatible provider presets, and registry-aware model lookup.
pub struct GenaiStream {
    client: genai::Client,
    chat_options: ChatOptions,
}

impl Default for GenaiStream {
    fn default() -> Self {
        GenaiStream::new()
    }
}

impl GenaiStream {
    /// Build a `GenaiStream` with [`genai::Client::default`] and
    /// [`baseline_chat_options`].
    pub fn new() -> Self {
        GenaiStream {
            client: genai::Client::default(),
            chat_options: baseline_chat_options(),
        }
    }

    /// Build with a caller-provided client and options. Lets tests inject
    /// mock clients without going through the (forthcoming) builder.
    pub fn with_client_and_options(
        client: genai::Client,
        chat_options: ChatOptions,
    ) -> Self {
        GenaiStream {
            client,
            chat_options,
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

        // Our registry keys are `"<provider>/<model>"`; genai dispatches on the
        // model name alone (e.g. `claude-sonnet-4-5`). Strip the prefix when
        // it's there. PR 3c will resolve this through the registry properly.
        let model_for_genai = model
            .id
            .split_once('/')
            .map(|(_, m)| m.to_string())
            .unwrap_or_else(|| model.id.clone());

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
