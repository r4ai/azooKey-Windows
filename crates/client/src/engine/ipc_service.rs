use anyhow::{Context as _, Result};
use hyper_util::rt::TokioIo;
use shared::proto::{
    azookey_service_client::AzookeyServiceClient, window_service_client::WindowServiceClient,
    ComposingText,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};
use tokio::{net::windows::named_pipe::ClientOptions, time};
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;
use windows::Win32::Foundation::ERROR_PIPE_BUSY;
use windows_core::GUID;

use super::state::IMEState;

const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const CONVERSION_RPC_TIMEOUT: Duration = Duration::from_secs(2);
const UI_RPC_TIMEOUT: Duration = Duration::from_millis(250);
const UI_RECONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const PIPE_RETRY_INTERVAL: Duration = Duration::from_millis(50);

const SERVER_PIPE: &str = r"\\.\pipe\azookey_server";
const UI_PIPE: &str = r"\\.\pipe\azookey_ui";

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

type ConversionClient = AzookeyServiceClient<Channel>;
type WindowClient = WindowServiceClient<Channel>;

#[derive(Debug)]
struct ConnectionIdentity {
    id: u64,
    active: AtomicBool,
}

impl ConnectionIdentity {
    fn new() -> Self {
        Self {
            id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
            active: AtomicBool::new(false),
        }
    }

    fn activate(&self) {
        self.active.store(true, Ordering::Release);
    }

    fn deactivate(&self) -> bool {
        self.active.swap(false, Ordering::AcqRel)
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

#[derive(Debug, Default)]
struct ServerSessionState {
    initialized_epoch: u64,
    initialized_reset_generation: u64,
}

impl ServerSessionState {
    fn needs_reset(&self, connection_epoch: u64, reset_generation: u64) -> bool {
        self.initialized_epoch != connection_epoch
            || self.initialized_reset_generation != reset_generation
    }

    fn note_reset(&mut self, connection_epoch: u64, reset_generation: u64) {
        self.initialized_epoch = connection_epoch;
        self.initialized_reset_generation = reset_generation;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamped<T> {
    sequence: u64,
    value: T,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CandidateState {
    candidates: Arc<Vec<String>>,
    selection: i32,
}

#[derive(Clone, Debug)]
enum UICommand {
    Candidate(Stamped<CandidateState>),
    Position(Stamped<(i32, i32, i32, i32)>),
    Selection(Stamped<i32>),
    Visibility(Stamped<bool>),
    InputMode(Stamped<String>),
}

impl UICommand {
    fn order_key(&self) -> (u64, u8) {
        match self {
            // Candidate state subsumes selection and visibility at the same sequence.
            Self::Candidate(value) => (value.sequence, 0),
            Self::Position(value) => (value.sequence, 1),
            Self::Selection(value) => (value.sequence, 2),
            Self::Visibility(value) => (value.sequence, 3),
            Self::InputMode(value) => (value.sequence, 4),
        }
    }
}

#[derive(Debug, Default)]
struct UIDesiredState {
    visible: Option<Stamped<bool>>,
    position: Option<Stamped<(i32, i32, i32, i32)>>,
    candidate: Option<Stamped<CandidateState>>,
    selection: Option<Stamped<i32>>,
    input_mode: Option<Stamped<String>>,
}

#[derive(Debug, Default)]
struct UISentState {
    visible: Option<bool>,
    position: Option<(i32, i32, i32, i32)>,
    candidates: Option<Arc<Vec<String>>>,
    selection: Option<i32>,
    input_mode: Option<String>,
}

impl UISentState {
    fn note_success(&mut self, command: &UICommand) {
        match command {
            UICommand::Candidate(value) => {
                self.candidates = Some(Arc::clone(&value.value.candidates));
                self.selection = Some(value.value.selection);
                self.visible = Some(true);
                // Candidate size affects edge-aware placement even at the same caret rectangle.
                self.position = None;
            }
            UICommand::Position(value) => self.position = Some(value.value),
            UICommand::Selection(value) => self.selection = Some(value.value),
            UICommand::Visibility(value) => self.visible = Some(value.value),
            UICommand::InputMode(value) => self.input_mode = Some(value.value.clone()),
        }
    }
}

#[derive(Clone, Debug)]
struct UIChannel {
    id: u64,
    client: WindowClient,
    transport_epoch: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UIReconnectAttempt {
    id: u64,
    channel_id: u64,
}

#[derive(Debug, Default)]
struct UIConnectionState {
    client: Option<UIChannel>,
    next_attempt_id: u64,
    next_channel_id: u64,
    active_attempt: Option<UIReconnectAttempt>,
    last_attempt: Option<Instant>,
    next_sequence: u64,
    desired: UIDesiredState,
    sent: UISentState,
    sent_channel: Option<(u64, u64)>,
}

impl UIConnectionState {
    fn begin_reconnect(&mut self, now: Instant) -> Option<UIReconnectAttempt> {
        if self.client.is_some()
            || self.active_attempt.is_some()
            || self.last_attempt.is_some_and(|last_attempt| {
                now.saturating_duration_since(last_attempt) < UI_RECONNECT_RETRY_INTERVAL
            })
        {
            return None;
        }

        self.next_attempt_id = self.next_attempt_id.wrapping_add(1);
        self.next_channel_id = self.next_channel_id.wrapping_add(1);
        let attempt = UIReconnectAttempt {
            id: self.next_attempt_id,
            channel_id: self.next_channel_id,
        };
        self.active_attempt = Some(attempt);
        self.last_attempt = Some(now);
        Some(attempt)
    }

    fn finish_reconnect(&mut self, attempt: UIReconnectAttempt, client: Option<UIChannel>) -> bool {
        if self.active_attempt != Some(attempt) {
            return false;
        }

        self.active_attempt = None;
        let Some(client) = client else {
            return false;
        };
        if self.client.is_some() || client.id != attempt.channel_id {
            return false;
        }

        let epoch = client.transport_epoch.load(Ordering::Acquire);
        self.client = Some(client);
        self.sent = UISentState::default();
        self.sent_channel = Some((attempt.channel_id, epoch));
        true
    }

    fn refresh_transport_epoch(&mut self) {
        let Some(client) = self.client.as_ref() else {
            self.sent = UISentState::default();
            self.sent_channel = None;
            return;
        };
        let epoch = client.transport_epoch.load(Ordering::Acquire);
        if self.sent_channel != Some((client.id, epoch)) {
            self.sent = UISentState::default();
            self.sent_channel = Some((client.id, epoch));
        }
    }

    fn invalidate_channel(&mut self, channel_id: u64, transport_epoch: u64) -> bool {
        let Some(client) = self.client.as_ref() else {
            return false;
        };
        if client.id != channel_id
            || client.transport_epoch.load(Ordering::Acquire) != transport_epoch
        {
            return false;
        }

        self.client = None;
        self.sent = UISentState::default();
        self.sent_channel = None;
        true
    }

    fn next_sequence(&mut self) -> u64 {
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.next_sequence
    }

    fn set_visible(&mut self, visible: bool) {
        if self.desired.visible.as_ref().map(|value| value.value) == Some(visible) {
            return;
        }
        let sequence = self.next_sequence();
        self.desired.visible = Some(Stamped {
            sequence,
            value: visible,
        });
    }

    fn set_position(&mut self, position: (i32, i32, i32, i32)) {
        if self.desired.position.as_ref().map(|value| value.value) == Some(position) {
            return;
        }
        let sequence = self.next_sequence();
        self.desired.position = Some(Stamped {
            sequence,
            value: position,
        });
    }

    fn set_candidate_state(&mut self, candidates: &[String], selection: i32) {
        let unchanged = self.desired.candidate.as_ref().is_some_and(|value| {
            value.value.selection == selection
                && value.value.candidates.as_slice() == candidates
                && self.desired.visible.as_ref().map(|value| value.value) == Some(true)
        });
        if unchanged {
            return;
        }

        let sequence = self.next_sequence();
        let candidate = CandidateState {
            candidates: Arc::new(candidates.to_vec()),
            selection,
        };
        self.desired.candidate = Some(Stamped {
            sequence,
            value: candidate,
        });
        self.desired.selection = Some(Stamped {
            sequence,
            value: selection,
        });
        self.desired.visible = Some(Stamped {
            sequence,
            value: true,
        });
    }

    fn set_selection(&mut self, selection: i32) {
        if self.desired.selection.as_ref().map(|value| value.value) == Some(selection) {
            return;
        }
        let sequence = self.next_sequence();
        self.desired.selection = Some(Stamped {
            sequence,
            value: selection,
        });
    }

    fn set_input_mode(&mut self, mode: &str) {
        if self
            .desired
            .input_mode
            .as_ref()
            .map(|value| value.value.as_str())
            == Some(mode)
        {
            return;
        }
        let sequence = self.next_sequence();
        self.desired.input_mode = Some(Stamped {
            sequence,
            value: mode.to_owned(),
        });
    }

    fn next_command(&self) -> Option<UICommand> {
        let mut commands = Vec::with_capacity(5);
        let hidden = self.desired.visible.as_ref().map(|value| value.value) == Some(false);
        let mut candidate_pending = false;
        if !hidden {
            if let Some(value) = self.desired.candidate.as_ref() {
                let latest_selection = self.desired.selection.as_ref();
                let effective = Stamped {
                    sequence: value.sequence,
                    value: CandidateState {
                        candidates: Arc::clone(&value.value.candidates),
                        selection: latest_selection
                            .map_or(value.value.selection, |selection| selection.value),
                    },
                };
                if self.sent.candidates.as_ref() != Some(&effective.value.candidates) {
                    commands.push(UICommand::Candidate(effective));
                    candidate_pending = true;
                }
            }
            if !candidate_pending {
                if let Some(value) = self.desired.position.as_ref() {
                    if self.sent.position != Some(value.value) {
                        commands.push(UICommand::Position(value.clone()));
                    }
                }
                if let Some(value) = self.desired.selection.as_ref() {
                    if self.sent.selection != Some(value.value) {
                        commands.push(UICommand::Selection(value.clone()));
                    }
                }
            }
        }
        if !candidate_pending {
            if let Some(value) = self.desired.visible.as_ref() {
                if self.sent.visible != Some(value.value) {
                    commands.push(UICommand::Visibility(value.clone()));
                }
            }
        }
        if let Some(value) = self.desired.input_mode.as_ref() {
            if self.sent.input_mode.as_deref() != Some(value.value.as_str()) {
                commands.push(UICommand::InputMode(value.clone()));
            }
        }
        commands.into_iter().min_by_key(UICommand::order_key)
    }

    fn note_success(&mut self, work: &UIWork) -> bool {
        let is_current = self.client.as_ref().is_some_and(|client| {
            client.id == work.channel_id
                && client.transport_epoch.load(Ordering::Acquire) == work.transport_epoch
        });
        if is_current {
            self.sent.note_success(&work.command);
        }
        is_current
    }
}

#[derive(Debug, Clone)]
pub struct IPCService {
    identity: Arc<ConnectionIdentity>,
    session_token: Arc<str>,
    azookey_client: ConversionClient,
    runtime: Arc<tokio::runtime::Runtime>,
    server_connection_epoch: Arc<AtomicU64>,
    server_reset_generation: Arc<AtomicU64>,
    server_session: Arc<Mutex<ServerSessionState>>,
    ui_connection: Arc<Mutex<UIConnectionState>>,
    ui_drain_scheduled: Arc<AtomicBool>,
}

#[derive(Debug)]
struct UIWork {
    channel_id: u64,
    transport_epoch: u64,
    client: WindowClient,
    command: UICommand,
}

#[derive(Clone, Debug)]
struct UIActorContext {
    identity: Arc<ConnectionIdentity>,
    connection: Arc<Mutex<UIConnectionState>>,
    scheduled: Arc<AtomicBool>,
}

enum UIDrainStep {
    Stop,
    Reconnect(UIReconnectAttempt),
    Rpc(UIWork),
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

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn request_with_session_token<T>(message: T, session_token: &str) -> Result<tonic::Request<T>> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        shared::IPC_SESSION_METADATA_KEY,
        session_token
            .parse()
            .context("invalid generated conversion session token")?,
    );
    Ok(request)
}

async fn open_pipe(
    path: &'static str,
) -> std::io::Result<TokioIo<tokio::net::windows::named_pipe::NamedPipeClient>> {
    loop {
        match ClientOptions::new().open(path) {
            Ok(client) => return Ok(TokioIo::new(client)),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {}
            Err(error) => return Err(error),
        }

        time::sleep(PIPE_RETRY_INTERVAL).await;
    }
}

fn server_endpoint() -> Result<Endpoint> {
    Ok(Endpoint::try_from("http://[::]:50051")?
        .connect_timeout(PIPE_CONNECT_TIMEOUT)
        .timeout(CONVERSION_RPC_TIMEOUT))
}

fn ui_endpoint() -> Result<Endpoint> {
    Ok(Endpoint::try_from("http://[::]:50052")?
        .connect_timeout(PIPE_CONNECT_TIMEOUT)
        .timeout(UI_RPC_TIMEOUT))
}

impl IPCService {
    pub fn new() -> Result<Self> {
        let runtime = Arc::new(tokio::runtime::Runtime::new()?);
        let server_connection_epoch = Arc::new(AtomicU64::new(0));
        let epoch_for_connector = Arc::clone(&server_connection_epoch);
        let server_channel = runtime.block_on(server_endpoint()?.connect_with_connector(
            service_fn(move |_| {
                let epoch = Arc::clone(&epoch_for_connector);
                async move {
                    let client = open_pipe(SERVER_PIPE).await?;
                    epoch.fetch_add(1, Ordering::AcqRel);
                    Ok::<_, std::io::Error>(client)
                }
            }),
        ))?;

        let service = Self {
            identity: Arc::new(ConnectionIdentity::new()),
            session_token: format!("{:032x}", GUID::new()?.to_u128()).into(),
            azookey_client: ConversionClient::new(server_channel),
            runtime,
            server_connection_epoch,
            server_reset_generation: Arc::new(AtomicU64::new(0)),
            server_session: Arc::new(Mutex::new(ServerSessionState::default())),
            ui_connection: Arc::new(Mutex::new(UIConnectionState::default())),
            ui_drain_scheduled: Arc::new(AtomicBool::new(false)),
        };
        tracing::debug!(
            connection_id = service.connection_id(),
            "Connected to conversion server"
        );
        Ok(service)
    }

    pub fn connection_id(&self) -> u64 {
        self.identity.id
    }

    pub fn is_active(&self) -> bool {
        self.identity.is_active()
    }

    pub(crate) fn activate(&self) {
        self.identity.activate();
    }

    pub(crate) fn deactivate(&self) {
        self.identity.deactivate();
    }

    /// Forces a lazy ClearText before the next focused stateful request without doing IPC from
    /// Deactivate or a background worker. A generation avoids losing a concurrent dirty mark.
    pub(crate) fn mark_server_session_dirty(&self) {
        self.server_reset_generation.fetch_add(1, Ordering::AcqRel);
    }

    fn invalidate(&self, error: &anyhow::Error) {
        if self.identity.deactivate() {
            tracing::warn!(
                connection_id = self.connection_id(),
                "Invalidating conversion IPC after error: {error:?}"
            );
            IMEState::invalidate_ipc(self.connection_id());
        }
    }

    fn ensure_current(&self) -> Result<()> {
        if self.identity.is_active() {
            Ok(())
        } else {
            anyhow::bail!(
                "conversion IPC connection {} is stale",
                self.connection_id()
            )
        }
    }

    fn stateful_request<T>(&self, message: T) -> Result<tonic::Request<T>> {
        request_with_session_token(message, &self.session_token)
    }

    fn raw_clear_text(
        &self,
        runtime: &tokio::runtime::Runtime,
        client: &mut ConversionClient,
    ) -> Result<()> {
        let request = self.stateful_request(shared::proto::ClearTextRequest {})?;
        runtime.block_on(client.clear_text(request))?;
        Ok(())
    }

    fn with_server_rpc<T>(
        &self,
        reset_operation: bool,
        operation: impl FnOnce(&tokio::runtime::Runtime, &mut ConversionClient) -> Result<T>,
    ) -> Result<T> {
        let result = (|| -> Result<T> {
            self.ensure_current()?;
            let mut session = lock_or_recover(&self.server_session);
            self.ensure_current()?;

            let mut epoch = self.server_connection_epoch.load(Ordering::Acquire);
            let reset_generation = self.server_reset_generation.load(Ordering::Acquire);
            let mut client = self.azookey_client.clone();
            if !reset_operation && session.needs_reset(epoch, reset_generation) {
                self.raw_clear_text(&self.runtime, &mut client)
                    .context("failed to initialize conversion server session")?;
                let current_epoch = self.server_connection_epoch.load(Ordering::Acquire);
                if current_epoch != epoch {
                    anyhow::bail!(
                        "conversion transport changed while initializing session ({epoch} -> {current_epoch})"
                    );
                }
                session.note_reset(epoch, reset_generation);
            }

            let value = operation(&self.runtime, &mut client)?;
            let current_epoch = self.server_connection_epoch.load(Ordering::Acquire);
            if current_epoch != epoch {
                anyhow::bail!(
                    "conversion transport changed during RPC ({epoch} -> {current_epoch})"
                );
            }
            epoch = current_epoch;
            if reset_operation {
                session.note_reset(epoch, reset_generation);
            }
            Ok(value)
        })();

        if let Err(error) = &result {
            self.invalidate(error);
        }
        result
    }

    fn schedule_ui_drain(&self) {
        if !self.identity.is_active()
            || self
                .ui_drain_scheduled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }

        let runtime = Arc::clone(&self.runtime);
        let actor = UIActorContext {
            identity: Arc::clone(&self.identity),
            connection: Arc::clone(&self.ui_connection),
            scheduled: Arc::clone(&self.ui_drain_scheduled),
        };
        runtime.spawn(async move {
            actor.drain().await;
        });
    }
}

impl UIActorContext {
    fn next_step(&self) -> UIDrainStep {
        let mut state = lock_or_recover(&self.connection);
        if !self.identity.is_active() {
            self.scheduled.store(false, Ordering::Release);
            return UIDrainStep::Stop;
        }

        state.refresh_transport_epoch();
        let Some(command) = state.next_command() else {
            // The flag is cleared while the desired-state mutex is held. A concurrent setter
            // either ran before this check (and is visible here) or runs after it and schedules
            // a new drain, so no wake-up can be lost.
            self.scheduled.store(false, Ordering::Release);
            return UIDrainStep::Stop;
        };
        let Some(channel) = state.client.as_ref() else {
            let Some(attempt) = state.begin_reconnect(Instant::now()) else {
                self.scheduled.store(false, Ordering::Release);
                return UIDrainStep::Stop;
            };
            return UIDrainStep::Reconnect(attempt);
        };

        UIDrainStep::Rpc(UIWork {
            channel_id: channel.id,
            transport_epoch: channel.transport_epoch.load(Ordering::Acquire),
            client: channel.client.clone(),
            command,
        })
    }

    async fn reconnect(&self, attempt: UIReconnectAttempt) {
        let identity = Arc::clone(&self.identity);
        let transport_epoch = Arc::new(AtomicU64::new(0));
        let epoch_for_connector = Arc::clone(&transport_epoch);
        let identity_for_connector = Arc::clone(&identity);
        let connection_result = async {
            let channel = ui_endpoint()?
                .connect_with_connector(service_fn(move |_| {
                    let epoch = Arc::clone(&epoch_for_connector);
                    let identity = Arc::clone(&identity_for_connector);
                    async move {
                        let client = open_pipe(UI_PIPE).await?;
                        if !identity.is_active() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::NotConnected,
                                "candidate UI owner is stale",
                            ));
                        }
                        // This epoch belongs only to this immutable channel id. An old channel
                        // can reconnect without mutating the installed channel state.
                        epoch.fetch_add(1, Ordering::AcqRel);
                        Ok::<_, std::io::Error>(client)
                    }
                }))
                .await?;
            Ok::<_, anyhow::Error>(UIChannel {
                id: attempt.channel_id,
                client: WindowClient::new(channel),
                transport_epoch,
            })
        }
        .await;

        let mut discarded_channel = None;
        {
            let mut state = lock_or_recover(&self.connection);
            if !identity.is_active() {
                discarded_channel = connection_result.ok();
                state.finish_reconnect(attempt, None);
            } else {
                match connection_result {
                    Ok(client) => {
                        if !state.finish_reconnect(attempt, Some(client.clone())) {
                            discarded_channel = Some(client);
                        }
                    }
                    Err(error) => {
                        state.finish_reconnect(attempt, None);
                        tracing::debug!("Candidate UI reconnect is not ready yet: {error:?}");
                    }
                }
            }
        }
        drop(discarded_channel);
    }

    async fn send(work: &mut UIWork) -> Result<(), tonic::Status> {
        match &work.command {
            UICommand::Candidate(value) => {
                let request = tonic::Request::new(shared::proto::SetCandidateStateRequest {
                    candidates: value.value.candidates.as_ref().clone(),
                    selection: value.value.selection,
                });
                work.client.set_candidate_state(request).await?;
            }
            UICommand::Position(value) => {
                let (top, left, bottom, right) = value.value;
                let request = tonic::Request::new(shared::proto::SetPositionRequest {
                    position: Some(shared::proto::WindowPosition {
                        top,
                        left,
                        bottom,
                        right,
                    }),
                });
                work.client.set_window_position(request).await?;
            }
            UICommand::Selection(value) => {
                let request =
                    tonic::Request::new(shared::proto::SetSelectionRequest { index: value.value });
                work.client.set_selection(request).await?;
            }
            UICommand::Visibility(value) => {
                let request = tonic::Request::new(shared::proto::EmptyResponse {});
                if value.value {
                    work.client.show_window(request).await?;
                } else {
                    work.client.hide_window(request).await?;
                }
            }
            UICommand::InputMode(value) => {
                let request = tonic::Request::new(shared::proto::SetInputModeRequest {
                    mode: value.value.clone(),
                });
                work.client.set_input_mode(request).await?;
            }
        }
        Ok(())
    }

    async fn drain(self) {
        loop {
            match self.next_step() {
                UIDrainStep::Stop => return,
                UIDrainStep::Reconnect(attempt) => {
                    self.reconnect(attempt).await;
                }
                UIDrainStep::Rpc(mut work) => {
                    let result = Self::send(&mut work).await;
                    let invalidated = {
                        let mut state = lock_or_recover(&self.connection);
                        match &result {
                            Ok(()) => {
                                state.note_success(&work);
                                false
                            }
                            Err(_) => {
                                state.invalidate_channel(work.channel_id, work.transport_epoch)
                            }
                        }
                    };
                    if let Err(error) = result {
                        tracing::warn!(
                            channel_id = work.channel_id,
                            "Candidate UI RPC failed: {error:?}"
                        );
                    }
                    if invalidated {
                        // The next loop observes the missing client, clears the scheduled flag,
                        // and launches at most one throttled reconnect task.
                        continue;
                    }
                }
            }
        }
    }
}

// Stateful conversion-server calls. Every method is serialized, initializes a fresh transport
// only when a focused TIP first uses it, and invalidates this exact connection on any failure.
impl IPCService {
    #[tracing::instrument(skip(self))]
    pub fn append_text(&mut self, text: String) -> Result<Candidates> {
        let request = self.stateful_request(shared::proto::AppendTextRequest {
            text_to_append: text,
        })?;
        self.with_server_rpc(false, move |runtime, client| {
            let response = runtime.block_on(client.append_text(request))?;
            Candidates::from_composing_text(response.into_inner().composing_text)
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn remove_text(&mut self) -> Result<Candidates> {
        let request = self.stateful_request(shared::proto::RemoveTextRequest {})?;
        self.with_server_rpc(false, |runtime, client| {
            let response = runtime.block_on(client.remove_text(request))?;
            Candidates::from_composing_text(response.into_inner().composing_text)
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn clear_text(&mut self) -> Result<()> {
        self.with_server_rpc(true, |runtime, client| self.raw_clear_text(runtime, client))
    }

    #[tracing::instrument(skip(self))]
    pub fn commit_prefix_and_append(&mut self, offset: i32, text: String) -> Result<Candidates> {
        let request = self.stateful_request(shared::proto::CommitPrefixAndAppendRequest {
            offset,
            text_to_append: text,
        })?;
        self.with_server_rpc(false, move |runtime, client| {
            let response = runtime.block_on(client.commit_prefix_and_append(request))?;
            Candidates::from_composing_text(response.into_inner().composing_text)
        })
    }

    pub fn set_context(&mut self, context: String) -> Result<()> {
        let request = self.stateful_request(shared::proto::SetContextRequest { context })?;
        self.with_server_rpc(false, move |runtime, client| {
            runtime.block_on(client.set_context(request))?;
            Ok(())
        })
    }
}

// Candidate UI calls are deliberately best-effort. A missing or timed-out UI never prevents
// conversion state from being written back to TSF; it only schedules an independent reconnect.
impl IPCService {
    #[tracing::instrument(skip(self))]
    pub fn hide_window(&mut self) -> Result<()> {
        if !self.identity.is_active() {
            return Ok(());
        }
        lock_or_recover(&self.ui_connection).set_visible(false);
        self.schedule_ui_drain();
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn set_window_position(
        &mut self,
        top: i32,
        left: i32,
        bottom: i32,
        right: i32,
    ) -> Result<()> {
        if !self.identity.is_active() {
            return Ok(());
        }
        lock_or_recover(&self.ui_connection).set_position((top, left, bottom, right));
        self.schedule_ui_drain();
        Ok(())
    }

    #[tracing::instrument(skip(self, candidates))]
    pub fn set_candidate_state(&mut self, candidates: &[String], selection: i32) -> Result<()> {
        if !self.identity.is_active() {
            return Ok(());
        }
        lock_or_recover(&self.ui_connection).set_candidate_state(candidates, selection);
        self.schedule_ui_drain();
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn set_selection(&mut self, index: i32) -> Result<()> {
        if !self.identity.is_active() {
            return Ok(());
        }
        lock_or_recover(&self.ui_connection).set_selection(index);
        self.schedule_ui_drain();
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn set_input_mode(&mut self, mode: &str) -> Result<()> {
        if !self.identity.is_active() {
            return Ok(());
        }
        lock_or_recover(&self.ui_connection).set_input_mode(mode);
        self.schedule_ui_drain();
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

    #[test]
    fn server_session_resets_once_per_transport_epoch() {
        let mut session = ServerSessionState::default();
        assert!(session.needs_reset(1, 0));
        session.note_reset(1, 0);
        assert!(!session.needs_reset(1, 0));
        assert!(session.needs_reset(2, 0));
        assert!(session.needs_reset(1, 1));
    }

    #[test]
    fn connection_identity_rejects_stale_clones() {
        let identity = ConnectionIdentity::new();
        assert!(!identity.is_active());
        identity.activate();
        assert!(identity.is_active());
        assert!(identity.deactivate());
        assert!(!identity.deactivate());
        assert!(!identity.is_active());
    }

    #[test]
    fn stateful_request_carries_the_shared_session_metadata() {
        let request = request_with_session_token((), "0123456789abcdef").unwrap();
        assert_eq!(
            request
                .metadata()
                .get(shared::IPC_SESSION_METADATA_KEY)
                .unwrap()
                .to_str()
                .unwrap(),
            "0123456789abcdef"
        );
    }

    #[test]
    fn stale_ui_worker_cannot_finish_new_attempt() {
        let now = Instant::now();
        let mut state = UIConnectionState::default();
        let first = state.begin_reconnect(now).unwrap();
        state.active_attempt = None;
        state.last_attempt = None;
        let second = state.begin_reconnect(now).unwrap();
        assert_ne!(first, second);
        assert!(!state.finish_reconnect(first, None));
        assert_eq!(state.active_attempt, Some(second));
    }

    #[test]
    fn candidate_success_updates_subsumed_state_and_requeues_position() {
        let mut sent = UISentState {
            visible: Some(false),
            position: Some((1, 2, 3, 4)),
            selection: Some(0),
            input_mode: Some("あ".to_owned()),
            ..UISentState::default()
        };
        let command = UICommand::Candidate(Stamped {
            sequence: 4,
            value: CandidateState {
                candidates: Arc::new(vec!["候補".to_owned()]),
                selection: 2,
            },
        });

        sent.note_success(&command);

        assert_eq!(sent.visible, Some(true));
        assert_eq!(sent.selection, Some(2));
        assert_eq!(sent.position, None);
        assert_eq!(sent.input_mode.as_deref(), Some("あ"));
    }

    #[test]
    fn hidden_desired_state_suppresses_stale_candidate_replay() {
        let mut state = UIConnectionState::default();
        state.set_candidate_state(&["候補".to_owned()], 0);
        state.set_visible(false);

        assert!(matches!(
            state.next_command(),
            Some(UICommand::Visibility(Stamped { value: false, .. }))
        ));
        state.sent.visible = Some(false);
        assert!(state.next_command().is_none());
    }

    #[test]
    fn reconnect_candidate_uses_latest_desired_selection() {
        let mut state = UIConnectionState::default();
        state.set_candidate_state(&["第一".to_owned(), "第二".to_owned()], 0);
        state.set_selection(1);

        let Some(UICommand::Candidate(candidate)) = state.next_command() else {
            panic!("candidate command was not queued");
        };
        assert_eq!(candidate.value.selection, 1);
    }

    #[test]
    fn desired_updates_coalesce_to_the_latest_value() {
        let mut state = UIConnectionState::default();
        state.set_position((1, 2, 3, 4));
        state.set_position((5, 6, 7, 8));

        assert!(matches!(
            state.next_command(),
            Some(UICommand::Position(Stamped {
                value: (5, 6, 7, 8),
                ..
            }))
        ));
    }

    #[test]
    fn reconnect_clears_sent_state_but_preserves_desired_state() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut state = UIConnectionState::default();
        state.set_input_mode("あ");
        state.sent.input_mode = Some("あ".to_owned());
        let attempt = state.begin_reconnect(Instant::now()).unwrap();
        let epoch = Arc::new(AtomicU64::new(1));
        let channel = UIChannel {
            id: attempt.channel_id,
            client: WindowClient::new(Endpoint::from_static("http://[::]:50052").connect_lazy()),
            transport_epoch: epoch,
        };

        assert!(state.finish_reconnect(attempt, Some(channel)));
        assert!(state.sent.input_mode.is_none());
        assert!(matches!(
            state.next_command(),
            Some(UICommand::InputMode(_))
        ));
    }

    #[test]
    fn stale_channel_or_transport_cannot_invalidate_current_ui() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut state = UIConnectionState::default();
        let attempt = state.begin_reconnect(Instant::now()).unwrap();
        let epoch = Arc::new(AtomicU64::new(3));
        let channel = UIChannel {
            id: attempt.channel_id,
            client: WindowClient::new(Endpoint::from_static("http://[::]:50052").connect_lazy()),
            transport_epoch: Arc::clone(&epoch),
        };
        assert!(state.finish_reconnect(attempt, Some(channel)));

        assert!(!state.invalidate_channel(attempt.channel_id.wrapping_add(1), 3));
        assert!(!state.invalidate_channel(attempt.channel_id, 2));
        assert!(state.client.is_some());
        assert!(state.invalidate_channel(attempt.channel_id, 3));
        assert!(state.client.is_none());
    }

    #[test]
    fn transport_deadlines_are_bounded() {
        assert_eq!(PIPE_CONNECT_TIMEOUT, Duration::from_millis(250));
        assert_eq!(CONVERSION_RPC_TIMEOUT, Duration::from_secs(2));
        assert_eq!(UI_RPC_TIMEOUT, Duration::from_millis(250));
    }
}
