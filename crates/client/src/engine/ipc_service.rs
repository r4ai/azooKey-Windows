use anyhow::{Context as _, Result};
use hyper_util::rt::TokioIo;
use shared::proto::{
    azookey_service_client::AzookeyServiceClient, window_service_client::WindowServiceClient,
    ComposingText,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::windows::named_pipe::ClientOptions, time};
use tonic::transport::Endpoint;
use tower::service_fn;
use windows::Win32::Foundation::ERROR_PIPE_BUSY;

// connect to kkc server
#[derive(Debug, Clone)]
pub struct IPCService {
    // kkc server client
    azookey_client: AzookeyServiceClient<tonic::transport::channel::Channel>,
    // candidate window server client
    window_client: WindowServiceClient<tonic::transport::channel::Channel>,
    runtime: Arc<tokio::runtime::Runtime>,
    ui_state: Arc<Mutex<UIState>>,
}

#[derive(Debug, Default)]
struct UIState {
    visible: Option<bool>,
    position: Option<(i32, i32, i32, i32)>,
    selection: Option<i32>,
    input_mode: Option<String>,
}

#[derive(Debug, Default)]
pub struct Candidates {
    pub texts: Vec<String>,
    pub sub_texts: Vec<String>,
    pub hiragana: Arc<String>,
    pub raw_input: Arc<String>,
    pub corresponding_count: Vec<i32>,
}

impl Candidates {
    fn from_composing_text(composing_text: Option<ComposingText>) -> anyhow::Result<Self> {
        let composing_text = composing_text.context("composing_text is None")?;
        let mut texts = Vec::with_capacity(composing_text.suggestions.len());
        let mut sub_texts = Vec::with_capacity(composing_text.suggestions.len());
        let mut corresponding_count = Vec::with_capacity(composing_text.suggestions.len());

        for suggestion in composing_text.suggestions {
            texts.push(suggestion.text);
            sub_texts.push(suggestion.subtext);
            corresponding_count.push(suggestion.corresponding_count);
        }

        let hiragana = composing_text.hiragana;
        let raw_input = Arc::new(composing_text.raw_input);
        if texts.is_empty() {
            // Conversion can legitimately yield no suggestions for partial roman input or
            // punctuation. Keep the composition usable and the selection index valid by
            // presenting the unconverted text as a local fallback candidate.
            corresponding_count.push(i32::try_from(hiragana.chars().count()).unwrap_or(i32::MAX));
            texts.push(hiragana.clone());
            sub_texts.push(String::new());
        }

        Ok(Self {
            texts,
            sub_texts,
            hiragana: Arc::new(hiragana),
            raw_input,
            corresponding_count,
        })
    }
}

impl IPCService {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Runtime::new()?;

        let server_channel = runtime.block_on(
            Endpoint::try_from("http://[::]:50051")?.connect_with_connector(service_fn(
                |_| async {
                    let client = loop {
                        match ClientOptions::new().open(r"\\.\pipe\azookey_server") {
                            Ok(client) => break client,
                            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => (),
                            Err(e) => return Err(e),
                        }

                        time::sleep(Duration::from_millis(50)).await;
                    };

                    Ok::<_, std::io::Error>(TokioIo::new(client))
                },
            )),
        )?;

        let ui_channel = runtime.block_on(
            Endpoint::try_from("http://[::]:50052")?.connect_with_connector(service_fn(
                |_| async {
                    let client = loop {
                        match ClientOptions::new().open(r"\\.\pipe\azookey_ui") {
                            Ok(client) => break client,
                            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => (),
                            Err(e) => return Err(e),
                        }

                        time::sleep(Duration::from_millis(50)).await;
                    };

                    Ok::<_, std::io::Error>(TokioIo::new(client))
                },
            )),
        )?;

        let azookey_client = AzookeyServiceClient::new(server_channel);
        let window_client = WindowServiceClient::new(ui_channel);
        tracing::debug!("Connected to server: {:?}", azookey_client);

        Ok(Self {
            azookey_client,
            window_client,
            runtime: Arc::new(runtime),
            ui_state: Arc::new(Mutex::new(UIState::default())),
        })
    }
}

// implement methods to interact with kkc server
impl IPCService {
    #[tracing::instrument]
    pub fn append_text(&mut self, text: String) -> anyhow::Result<Candidates> {
        let request = tonic::Request::new(shared::proto::AppendTextRequest {
            text_to_append: text,
        });

        let response = self
            .runtime
            .clone()
            .block_on(self.azookey_client.append_text(request))?;
        Candidates::from_composing_text(response.into_inner().composing_text)
    }

    #[tracing::instrument]
    pub fn remove_text(&mut self) -> anyhow::Result<Candidates> {
        let request = tonic::Request::new(shared::proto::RemoveTextRequest {});
        let response = self
            .runtime
            .clone()
            .block_on(self.azookey_client.remove_text(request))?;
        Candidates::from_composing_text(response.into_inner().composing_text)
    }

    #[tracing::instrument]
    pub fn clear_text(&mut self) -> anyhow::Result<()> {
        let request = tonic::Request::new(shared::proto::ClearTextRequest {});
        let _response = self
            .runtime
            .clone()
            .block_on(self.azookey_client.clear_text(request))?;

        Ok(())
    }

    #[tracing::instrument]
    pub fn commit_prefix_and_append(
        &mut self,
        offset: i32,
        text: String,
    ) -> anyhow::Result<Candidates> {
        let request = tonic::Request::new(shared::proto::CommitPrefixAndAppendRequest {
            offset,
            text_to_append: text,
        });
        let response = self
            .runtime
            .clone()
            .block_on(self.azookey_client.commit_prefix_and_append(request))?;

        Candidates::from_composing_text(response.into_inner().composing_text)
    }

    pub fn set_context(&mut self, context: String) -> anyhow::Result<()> {
        let request = tonic::Request::new(shared::proto::SetContextRequest { context });
        let _response = self
            .runtime
            .clone()
            .block_on(self.azookey_client.set_context(request))?;

        Ok(())
    }
}

// implement methods to interact with candidate window server
impl IPCService {
    #[tracing::instrument]
    pub fn hide_window(&mut self) -> anyhow::Result<()> {
        if self
            .ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .visible
            == Some(false)
        {
            return Ok(());
        }

        let request = tonic::Request::new(shared::proto::EmptyResponse {});
        self.runtime
            .clone()
            .block_on(self.window_client.hide_window(request))?;
        self.ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .visible = Some(false);

        Ok(())
    }

    #[tracing::instrument]
    pub fn set_window_position(
        &mut self,
        top: i32,
        left: i32,
        bottom: i32,
        right: i32,
    ) -> anyhow::Result<()> {
        let position = (top, left, bottom, right);
        if self
            .ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .position
            == Some(position)
        {
            return Ok(());
        }

        let request = tonic::Request::new(shared::proto::SetPositionRequest {
            position: Some(shared::proto::WindowPosition {
                top,
                left,
                bottom,
                right,
            }),
        });
        self.runtime
            .clone()
            .block_on(self.window_client.set_window_position(request))?;
        self.ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .position = Some(position);

        Ok(())
    }

    #[tracing::instrument]
    pub fn set_candidate_state(
        &mut self,
        candidates: &[String],
        selection: i32,
    ) -> anyhow::Result<()> {
        let request = tonic::Request::new(shared::proto::SetCandidateStateRequest {
            candidates: candidates.to_vec(),
            selection,
        });
        self.runtime
            .clone()
            .block_on(self.window_client.set_candidate_state(request))?;

        let mut state = self
            .ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        state.selection = Some(selection);
        state.visible = Some(true);
        // Candidate size affects edge-aware placement even when the caret rectangle does not.
        // The UI immediately repositions from its saved anchor; invalidating this cache also
        // guarantees that the next TSF layout update re-sends the anchor.
        state.position = None;

        Ok(())
    }

    #[tracing::instrument]
    pub fn set_selection(&mut self, index: i32) -> anyhow::Result<()> {
        if self
            .ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .selection
            == Some(index)
        {
            return Ok(());
        }

        let request = tonic::Request::new(shared::proto::SetSelectionRequest { index });
        self.runtime
            .clone()
            .block_on(self.window_client.set_selection(request))?;
        self.ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .selection = Some(index);

        Ok(())
    }

    #[tracing::instrument]
    pub fn set_input_mode(&mut self, mode: &str) -> anyhow::Result<()> {
        if self
            .ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .input_mode
            .as_deref()
            == Some(mode)
        {
            return Ok(());
        }

        let request = tonic::Request::new(shared::proto::SetInputModeRequest {
            mode: mode.to_string(),
        });
        self.runtime
            .clone()
            .block_on(self.window_client.set_input_mode(request))?;
        self.ui_state
            .lock()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .input_mode = Some(mode.to_owned());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_suggestions_fall_back_to_unconverted_text() {
        let candidates = Candidates::from_composing_text(Some(ComposingText {
            hiragana: "みへんかん".to_owned(),
            suggestions: Vec::new(),
            raw_input: "mihenkann".to_owned(),
        }))
        .unwrap();

        assert_eq!(candidates.texts, ["みへんかん"]);
        assert_eq!(candidates.sub_texts, [""]);
        assert_eq!(candidates.corresponding_count, [5]);
        assert_eq!(candidates.raw_input.as_str(), "mihenkann");
    }
}
