use std::{
    cmp::{max, min},
    sync::Arc,
};

use crate::{
    engine::user_action::UserAction,
    extension::VKeyExt as _,
    tsf::factory::{TextServiceFactory, TextServiceFactory_Impl},
};

use super::{
    client_action::{ClientAction, SetSelectionType, SetTextType},
    full_width::{to_fullwidth, to_halfwidth},
    input_mode::InputMode,
    ipc_service::Candidates,
    state::IMEState,
    text_util::{to_half_katakana, to_katakana},
    user_action::{Function, Navigation},
};
use windows::Win32::{
    Foundation::WPARAM,
    UI::{
        Input::KeyboardAndMouse::{VK_CONTROL, VK_MENU},
        TextServices::{ITfComposition, ITfCompositionSink_Impl, ITfContext},
    },
};

use anyhow::{Context, Result};

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompositionState {
    #[default]
    None,
    Composing,
    Previewing,
    Selecting,
}

fn is_last_composing_character(text: &str) -> bool {
    text.chars().nth(1).is_none()
}

fn text_with_type(set_type: &SetTextType, raw_input: &str, raw_hiragana: &str) -> String {
    match set_type {
        SetTextType::Hiragana => raw_hiragana.to_owned(),
        SetTextType::Katakana => to_katakana(raw_hiragana),
        SetTextType::HalfKatakana => to_half_katakana(raw_hiragana),
        SetTextType::FullLatin => to_fullwidth(raw_input, true),
        SetTextType::HalfLatin => to_halfwidth(raw_input),
    }
}

fn candidate_display_text(text: &str, sub_text: &str) -> String {
    let mut display = String::with_capacity(text.len() + sub_text.len());
    display.push_str(text);
    display.push_str(sub_text);
    display
}

fn displayed_utf16_count(preview: &str, suffix: &str) -> usize {
    preview
        .encode_utf16()
        .count()
        .saturating_add(suffix.encode_utf16().count())
}

fn snapshot_is_current(snapshot_generation: u64, current_generation: u64) -> bool {
    snapshot_generation == current_generation
}

fn input_mode_transition(
    state: CompositionState,
    current_mode: InputMode,
    requested_mode: InputMode,
) -> (CompositionState, Vec<ClientAction>) {
    let next_state = if current_mode == requested_mode {
        state
    } else {
        CompositionState::None
    };

    (next_state, vec![ClientAction::SetIMEMode(requested_mode)])
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ActionProgress {
    started_composition: bool,
    visible_or_committed_effect: bool,
    must_eat_on_failure: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailedKeyDisposition {
    PassThrough,
    Eat,
}

enum ActionExecution {
    Applied,
    Failed {
        error: anyhow::Error,
        disposition: FailedKeyDisposition,
    },
}

fn failed_key_disposition(
    progress: ActionProgress,
    cleanup_succeeded: bool,
) -> FailedKeyDisposition {
    if !cleanup_succeeded || progress.visible_or_committed_effect || progress.must_eat_on_failure {
        FailedKeyDisposition::Eat
    } else {
        FailedKeyDisposition::PassThrough
    }
}

#[derive(Default, Debug)]
pub struct Composition {
    // These strings are snapshotted while synchronous RPC/TSF calls run. Arc keeps that
    // snapshot cheap for long compositions while preserving a stable state for re-entrant TSF
    // callbacks.
    pub preview: Arc<String>, // text to be previewed
    pub suffix: Arc<String>,  // text to be appended after preview
    pub raw_input: Arc<String>,
    pub raw_hiragana: Arc<String>,

    pub corresponding_count: i32, // corresponding count of the preview

    pub selection_index: i32,
    pub candidates: Arc<Candidates>,

    pub state: CompositionState,
    pub tip_composition: Option<ITfComposition>,
    pub generation: u64,
}

impl Composition {
    pub(crate) fn reset(&mut self) {
        self.preview = Arc::default();
        self.suffix = Arc::default();
        self.raw_input = Arc::default();
        self.raw_hiragana = Arc::default();
        self.corresponding_count = 0;
        self.selection_index = 0;
        self.candidates = Arc::default();
        self.state = CompositionState::None;
        self.tip_composition = None;
        self.generation = self.generation.wrapping_add(1);
    }
}

impl ITfCompositionSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        _pcomposition: Option<&ITfComposition>,
    ) -> Result<()> {
        // if user clicked outside the composition, the composition will be terminated
        tracing::debug!("OnCompositionTerminated");

        self.handle_composition_terminated()?;

        Ok(())
    }
}

impl TextServiceFactory {
    fn composition_generation(&self) -> Result<u64> {
        let text_service = self.borrow()?;
        let generation = text_service.borrow_composition()?.generation;
        Ok(generation)
    }

    fn handle_composition_terminated(&self) -> Result<()> {
        {
            let text_service = self.borrow()?;
            let mut composition = text_service.borrow_mut_composition()?;
            composition.reset();
        }
        {
            let text_service = self.borrow()?;
            text_service.pending_input_mode_transition.set(None);
            text_service.pending_composition_cleanup.set(false);
        }

        let ipc_service = IMEState::ipc_snapshot();
        if let Some(mut ipc_service) = ipc_service {
            if let Err(error) = ipc_service.hide_window() {
                tracing::warn!("Failed to hide candidate window after termination: {error:?}");
            }
            if let Err(error) = ipc_service.clear_text() {
                tracing::warn!("Failed to clear server composition after termination: {error:?}");
            }
        }

        Ok(())
    }

    pub fn has_pending_input_mode_transition(&self) -> Result<bool> {
        Ok(self.borrow()?.pending_input_mode_transition.get().is_some())
    }

    pub fn has_pending_key_cleanup(&self) -> Result<bool> {
        let text_service = self.borrow()?;
        Ok(text_service.pending_composition_cleanup.get()
            || text_service.pending_input_mode_transition.get().is_some())
    }

    fn finish_pending_composition_cleanup(&self) -> Result<()> {
        if !self.borrow()?.pending_composition_cleanup.get() {
            return Ok(());
        }

        self.end_composition()?;
        self.borrow()?.pending_composition_cleanup.set(true);
        let still_active = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            composition.tip_composition.is_some() || composition.state != CompositionState::None
        };
        if still_active {
            self.handle_composition_terminated()?;
        } else {
            self.borrow()?.pending_composition_cleanup.set(false);
        }
        Ok(())
    }

    fn recover_failed_action(&self, started_composition: bool) -> Result<()> {
        // Arm the retry before any borrow or COM call below can fail. It is cleared only after
        // local TSF state is known to be consistent again.
        self.borrow()?.pending_composition_cleanup.set(true);
        let has_active_composition = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            composition.tip_composition.is_some() || composition.state != CompositionState::None
        };

        if has_active_composition || started_composition {
            self.end_composition()
                .context("failed to commit composition after action error")?;
            // A synchronous OnCompositionTerminated callback clears the flag as part of its
            // normal reset. Re-arm it until the fallback consistency check below has completed.
            self.borrow()?.pending_composition_cleanup.set(true);

            // Some hosts do not synchronously call OnCompositionTerminated. Complete the same
            // idempotent reset after EndComposition has succeeded.
            let still_active = {
                let text_service = self.borrow()?;
                let composition = text_service.borrow_composition()?;
                composition.tip_composition.is_some() || composition.state != CompositionState::None
            };
            if still_active {
                self.handle_composition_terminated()
                    .context("failed to reset composition after action error")?;
            }
        }

        self.borrow()?.pending_composition_cleanup.set(false);
        Ok(())
    }

    /// Applies an external TSF mode transition at the first real key-processing safe point.
    /// OnChange cannot request an edit session because compartment callbacks may be synchronous
    /// with SetValue. By waiting until OnKeyDown, the composition can be ended before this same
    /// key is classified again under the new mode.
    fn finish_pending_input_mode_transition(&self) -> Result<bool> {
        let pending = self.borrow()?.pending_input_mode_transition.get();
        if pending.is_none() {
            return Ok(false);
        }

        let tip_exists = {
            let text_service = self.borrow()?;
            let tip_exists = text_service.borrow_composition()?.tip_composition.is_some();
            tip_exists
        };
        if tip_exists {
            self.end_composition()?;
        }

        // EndComposition normally invokes OnCompositionTerminated synchronously. If a host does
        // not invoke it, perform the same idempotent server/UI and local-state cleanup here.
        if self.has_pending_input_mode_transition()? {
            self.handle_composition_terminated()?;
        }

        Ok(true)
    }

    #[tracing::instrument]
    pub fn process_key(
        &self,
        context: Option<&ITfContext>,
        wparam: WPARAM,
    ) -> Result<Option<(Vec<ClientAction>, CompositionState)>> {
        if context.is_none() {
            return Ok(None);
        };

        let (composition_state, tip_exists, is_last_input, suffix_is_empty, mode) = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            let mode = text_service.mode.get();
            (
                composition.state,
                composition.tip_composition.is_some(),
                is_last_composing_character(&composition.raw_hiragana),
                composition.suffix.is_empty(),
                mode,
            )
        };

        let action = UserAction::try_from(wparam.0)?;

        // Activation is intentionally allowed to complete without IPC so the TIP remains
        // selectable during launcher/server startup races. If a composition was already active
        // when IPC disappeared, claim one key to end it locally instead of letting the host edit
        // underneath stale preedit. With no active composition, ordinary text passes through.
        let ipc_available = IMEState::ipc_available_or_start_reconnect();
        if !ipc_available
            && !matches!(
                &action,
                UserAction::SetInputMode(_) | UserAction::ToggleInputMode
            )
        {
            return if tip_exists || composition_state != CompositionState::None {
                Ok(Some((
                    vec![ClientAction::EndComposition],
                    CompositionState::None,
                )))
            } else {
                Ok(None)
            };
        }

        // Normal shortcuts pass through, but an offline active composition above must first be
        // claimed and ended so the host cannot edit underneath stale preedit.
        if VK_CONTROL.is_pressed() || VK_MENU.is_pressed() {
            return Ok(None);
        }

        // Mode commands are valid in every composition state, including candidate selection.
        // Handling them before the state-specific key table also keeps F3/F4 explicitly
        // idempotent instead of treating both as a blind toggle.
        let requested_mode = match &action {
            UserAction::SetInputMode(requested_mode) => Some(*requested_mode),
            UserAction::ToggleInputMode => Some(mode.toggled()),
            _ => None,
        };
        if let Some(requested_mode) = requested_mode {
            let (transition, actions) =
                input_mode_transition(composition_state, mode, requested_mode);
            return Ok(Some((actions, transition)));
        }

        let (transition, actions) = match composition_state {
            CompositionState::None => match action {
                UserAction::Input(char) if mode == InputMode::Kana => (
                    CompositionState::Composing,
                    vec![
                        ClientAction::StartComposition,
                        ClientAction::AppendText(char.to_string()),
                    ],
                ),
                UserAction::Number(number) if mode == InputMode::Kana => (
                    CompositionState::Composing,
                    vec![
                        ClientAction::StartComposition,
                        ClientAction::AppendText(number.to_string()),
                    ],
                ),
                _ => {
                    return Ok(None);
                }
            },
            CompositionState::Composing => match action {
                UserAction::Input(char) => (
                    CompositionState::Composing,
                    vec![ClientAction::AppendText(char.to_string())],
                ),
                UserAction::Number(number) => (
                    CompositionState::Composing,
                    vec![ClientAction::AppendText(number.to_string())],
                ),
                UserAction::Backspace => {
                    if is_last_input {
                        (
                            CompositionState::None,
                            vec![ClientAction::DiscardComposition],
                        )
                    } else {
                        (CompositionState::Composing, vec![ClientAction::RemoveText])
                    }
                }
                UserAction::Enter => {
                    if suffix_is_empty {
                        (CompositionState::None, vec![ClientAction::EndComposition])
                    } else {
                        (
                            CompositionState::Composing,
                            vec![ClientAction::CommitPrefixAndAppend("".to_string())],
                        )
                    }
                }
                UserAction::Escape => (
                    CompositionState::None,
                    vec![ClientAction::DiscardComposition],
                ),
                UserAction::Navigation(direction) => match direction {
                    Navigation::Right => (
                        CompositionState::Composing,
                        vec![ClientAction::MoveCursor(1)],
                    ),
                    Navigation::Left => (
                        CompositionState::Composing,
                        vec![ClientAction::MoveCursor(-1)],
                    ),
                    Navigation::Up => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetSelection(SetSelectionType::Up)],
                    ),
                    Navigation::Down => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetSelection(SetSelectionType::Down)],
                    ),
                },
                UserAction::Space | UserAction::Tab => (
                    CompositionState::Previewing,
                    vec![ClientAction::SetSelection(SetSelectionType::Down)],
                ),
                UserAction::Function(key) => match key {
                    Function::Six => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::Hiragana)],
                    ),
                    Function::Seven => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::Katakana)],
                    ),
                    Function::Eight => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::HalfKatakana)],
                    ),
                    Function::Nine => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::FullLatin)],
                    ),
                    Function::Ten => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::HalfLatin)],
                    ),
                },
                _ => {
                    return Ok(None);
                }
            },
            CompositionState::Previewing => match action {
                UserAction::Input(char) => (
                    CompositionState::Composing,
                    vec![ClientAction::CommitPrefixAndAppend(char.to_string())],
                ),
                UserAction::Number(number) => (
                    CompositionState::Composing,
                    vec![ClientAction::CommitPrefixAndAppend(number.to_string())],
                ),
                UserAction::Backspace => {
                    if is_last_input {
                        (
                            CompositionState::None,
                            vec![ClientAction::DiscardComposition],
                        )
                    } else {
                        (CompositionState::Composing, vec![ClientAction::RemoveText])
                    }
                }
                UserAction::Enter => {
                    if suffix_is_empty {
                        (CompositionState::None, vec![ClientAction::EndComposition])
                    } else {
                        (
                            CompositionState::Composing,
                            vec![ClientAction::CommitPrefixAndAppend("".to_string())],
                        )
                    }
                }
                UserAction::Escape => (
                    CompositionState::None,
                    vec![ClientAction::DiscardComposition],
                ),
                UserAction::Navigation(direction) => match direction {
                    Navigation::Right => (
                        CompositionState::Composing,
                        vec![ClientAction::MoveCursor(1)],
                    ),
                    Navigation::Left => (
                        CompositionState::Composing,
                        vec![ClientAction::MoveCursor(-1)],
                    ),
                    Navigation::Up => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetSelection(SetSelectionType::Up)],
                    ),
                    Navigation::Down => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetSelection(SetSelectionType::Down)],
                    ),
                },
                UserAction::Space | UserAction::Tab => (
                    CompositionState::Previewing,
                    vec![ClientAction::SetSelection(SetSelectionType::Down)],
                ),
                UserAction::Function(key) => match key {
                    Function::Six => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::Hiragana)],
                    ),
                    Function::Seven => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::Katakana)],
                    ),
                    Function::Eight => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::HalfKatakana)],
                    ),
                    Function::Nine => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::FullLatin)],
                    ),
                    Function::Ten => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::HalfLatin)],
                    ),
                },
                _ => {
                    return Ok(None);
                }
            },
            _ => {
                return Ok(None);
            }
        };

        Ok(Some((actions, transition)))
    }

    #[tracing::instrument]
    pub fn handle_key(&self, context: Option<&ITfContext>, wparam: WPARAM) -> Result<bool> {
        if let Some(context) = context {
            self.borrow_mut()?.context = Some(context.clone());
        } else {
            return Ok(false);
        };

        if let Err(error) = self.finish_pending_composition_cleanup() {
            // The previous failure may have left a live TSF range. Keep claiming keys until it
            // can be ended; passing this key to the host could edit underneath that range.
            tracing::warn!("Failed to finish pending composition cleanup: {error:?}");
            return Ok(true);
        }

        if let Err(error) = self.finish_pending_input_mode_transition() {
            // Keep the pending transition for the next key. Passing this key to the host while
            // the old TIP composition is still active would mix host input with stale preedit.
            tracing::warn!("Failed to finish pending input-mode transition: {error:?}");
            return Ok(true);
        }

        // A reconnect creates a fresh UI channel whose deduplication cache is empty. Synchronize
        // the per-TextService mode immediately before the first handled key, without making UI
        // availability a prerequisite for TSF mode keys.
        let mode = self.borrow()?.mode.get();
        let ipc_service = IMEState::ipc_snapshot();
        if let Some(mut ipc_service) = ipc_service {
            if let Err(error) = ipc_service.set_input_mode(mode.indicator()) {
                tracing::warn!("Failed to synchronize candidate UI input mode: {error:?}");
            }
        }

        let Some((actions, transition)) = self.process_key(context, wparam)? else {
            return Ok(false);
        };

        match self.execute_actions(&actions, transition) {
            ActionExecution::Applied => Ok(true),
            ActionExecution::Failed { error, disposition } => {
                tracing::warn!("Failed to handle key action: {error:?}");
                Ok(disposition == FailedKeyDisposition::Eat)
            }
        }
    }

    /// Handles a TSF input-mode command independently of the physical modifier state.
    /// Preserved keys can be delivered after the original modifier state has changed, and the
    /// VK_KANJI registration intentionally ignores modifiers, so routing back through
    /// `process_key` would incorrectly reject a valid command while Ctrl happens to be down.
    pub fn handle_input_mode_toggle(&self, context: Option<&ITfContext>) -> Result<bool> {
        let Some(context) = context else {
            return Ok(false);
        };
        self.borrow_mut()?.context = Some(context.clone());

        if let Err(error) = self.finish_pending_composition_cleanup() {
            tracing::warn!("Failed to finish pending composition cleanup: {error:?}");
            return Ok(true);
        }
        if let Err(error) = self.finish_pending_input_mode_transition() {
            tracing::warn!("Failed to finish pending input-mode transition: {error:?}");
            return Ok(true);
        }

        let (state, mode) = {
            let text_service = self.borrow()?;
            let state = text_service.borrow_composition()?.state;
            (state, text_service.mode.get())
        };
        let (transition, actions) = input_mode_transition(state, mode, mode.toggled());

        match self.execute_actions(&actions, transition) {
            ActionExecution::Applied => Ok(true),
            ActionExecution::Failed { error, disposition } => {
                tracing::warn!("Failed to handle preserved input-mode command: {error:?}");
                Ok(disposition == FailedKeyDisposition::Eat)
            }
        }
    }

    #[tracing::instrument]
    pub fn handle_action(
        &self,
        actions: &[ClientAction],
        transition: CompositionState,
    ) -> Result<()> {
        match self.execute_actions(actions, transition) {
            ActionExecution::Applied => Ok(()),
            ActionExecution::Failed { error, .. } => Err(error),
        }
    }

    fn execute_actions(
        &self,
        actions: &[ClientAction],
        transition: CompositionState,
    ) -> ActionExecution {
        let mut progress = ActionProgress::default();
        match self.handle_action_inner(actions, transition, &mut progress) {
            Ok(()) => ActionExecution::Applied,
            Err(error) => {
                let cleanup_succeeded =
                    match self.recover_failed_action(progress.started_composition) {
                        Ok(()) => true,
                        Err(cleanup_error) => {
                            tracing::warn!(
                            "Failed to clean up composition after action error: {cleanup_error:?}"
                        );
                            false
                        }
                    };
                ActionExecution::Failed {
                    error,
                    disposition: failed_key_disposition(progress, cleanup_succeeded),
                }
            }
        }
    }

    fn handle_action_inner(
        &self,
        actions: &[ClientAction],
        transition: CompositionState,
        progress: &mut ActionProgress,
    ) -> Result<()> {
        let (
            mut preview,
            mut suffix,
            mut raw_input,
            mut raw_hiragana,
            mut corresponding_count,
            mut candidates,
            mut selection_index,
            mut generation,
            mut mode,
        ) = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            let mode = text_service.mode.get();
            (
                Arc::clone(&composition.preview),
                Arc::clone(&composition.suffix),
                Arc::clone(&composition.raw_input),
                Arc::clone(&composition.raw_hiragana),
                composition.corresponding_count,
                Arc::clone(&composition.candidates),
                composition.selection_index,
                composition.generation,
                mode,
            )
        };
        let mut ipc_service = IMEState::ipc_snapshot();
        let mut transition = transition;
        macro_rules! require_ipc_service {
            () => {
                ipc_service.as_mut().context("ipc_service is None")?
            };
        }
        macro_rules! abort_if_reentered {
            () => {
                if !snapshot_is_current(generation, self.composition_generation()?) {
                    tracing::warn!("Discard stale composition action after TSF re-entry");
                    if let Some(ipc_service) = ipc_service.as_mut() {
                        if let Err(error) = ipc_service.hide_window() {
                            tracing::warn!("Failed to hide window after TSF re-entry: {error:?}");
                        }
                        if let Err(error) = ipc_service.clear_text() {
                            tracing::warn!("Failed to clear server after TSF re-entry: {error:?}");
                        }
                    }
                    return Ok(());
                }
            };
        }

        for action in actions {
            match action {
                ClientAction::StartComposition => {
                    // Surrounding text is stable for the lifetime of a composition. Fetch it
                    // once here instead of issuing a TSF edit session and an RPC for every key.
                    self.update_context(0, "")?;
                    abort_if_reentered!();
                    self.start_composition()?;
                    progress.started_composition = true;
                    abort_if_reentered!();
                    self.update_pos()?;
                    abort_if_reentered!();
                }
                ClientAction::EndComposition => {
                    progress.must_eat_on_failure = true;
                    self.end_composition()?;
                    progress.visible_or_committed_effect = true;
                    // EndComposition may synchronously call OnCompositionTerminated. That
                    // callback has already established the desired None state, so accept its
                    // generation and continue only with idempotent cleanup.
                    generation = self.composition_generation()?;
                    selection_index = 0;
                    corresponding_count = 0;
                    preview = Arc::default();
                    suffix = Arc::default();
                    raw_input = Arc::default();
                    raw_hiragana = Arc::default();
                    candidates = Arc::default();
                    if let Some(ipc_service) = ipc_service.as_mut() {
                        ipc_service.hide_window()?;
                        ipc_service.clear_text()?;
                    }
                }
                ClientAction::DiscardComposition => {
                    progress.must_eat_on_failure = true;
                    // Clearing the TSF range before ending commits an empty string. In
                    // particular, do not call RemoveText here: its response runs an expensive
                    // conversion whose result is immediately thrown away.
                    self.set_text("", "")?;
                    progress.visible_or_committed_effect = true;
                    abort_if_reentered!();
                    self.end_composition()?;
                    progress.visible_or_committed_effect = true;
                    generation = self.composition_generation()?;
                    selection_index = 0;
                    corresponding_count = 0;
                    preview = Arc::default();
                    suffix = Arc::default();
                    raw_input = Arc::default();
                    raw_hiragana = Arc::default();
                    candidates = Arc::default();
                    if let Some(ipc_service) = ipc_service.as_mut() {
                        ipc_service.hide_window()?;
                        ipc_service.clear_text()?;
                    }
                }
                ClientAction::AppendText(text) => {
                    let text = match mode {
                        InputMode::Kana => to_fullwidth(text, false),
                        InputMode::Latin => text.to_string(),
                    };

                    candidates = Arc::new(require_ipc_service!().append_text(text)?);
                    selection_index = 0;
                    let text = candidates
                        .texts
                        .first()
                        .context("candidate text is empty")?;
                    let sub_text = candidates
                        .sub_texts
                        .first()
                        .context("candidate subtext is empty")?;
                    corresponding_count = *candidates
                        .corresponding_count
                        .first()
                        .context("candidate corresponding_count is empty")?;

                    self.set_text(text, sub_text)?;
                    progress.visible_or_committed_effect = true;
                    abort_if_reentered!();
                    preview = Arc::new(text.clone());
                    suffix = Arc::new(sub_text.clone());
                    raw_input = Arc::clone(&candidates.raw_input);
                    raw_hiragana = Arc::clone(&candidates.hiragana);

                    require_ipc_service!()
                        .set_candidate_state(&candidates.texts, selection_index)?;
                }
                ClientAction::RemoveText => {
                    candidates = Arc::new(require_ipc_service!().remove_text()?);
                    selection_index = 0;
                    let text = candidates.texts.first().map(String::as_str).unwrap_or("");
                    let sub_text = candidates
                        .sub_texts
                        .first()
                        .map(String::as_str)
                        .unwrap_or("");
                    corresponding_count = *candidates.corresponding_count.first().unwrap_or(&0);

                    self.set_text(text, sub_text)?;
                    progress.visible_or_committed_effect = true;
                    abort_if_reentered!();
                    preview = Arc::new(text.to_owned());
                    suffix = Arc::new(sub_text.to_owned());
                    raw_input = Arc::clone(&candidates.raw_input);
                    raw_hiragana = Arc::clone(&candidates.hiragana);

                    require_ipc_service!()
                        .set_candidate_state(&candidates.texts, selection_index)?;
                }
                ClientAction::MoveCursor(_offset) => {
                    // TODO: I'll use azookey-kkc's composingText
                    // self.set_cursor(offset)?;
                }
                ClientAction::SetIMEMode(requested_mode) => {
                    progress.must_eat_on_failure = true;
                    let requested_change = mode != *requested_mode;
                    let tip_exists = {
                        let text_service = self.borrow()?;
                        let tip_exists =
                            text_service.borrow_composition()?.tip_composition.is_some();
                        tip_exists
                    };
                    if requested_change && tip_exists {
                        self.end_composition()?;
                        progress.visible_or_committed_effect = true;
                        generation = self.composition_generation()?;
                    }

                    let previous_mode = mode;
                    let actual_mode = self.set_input_mode_compartments(*requested_mode)?;
                    mode = actual_mode;

                    // UI is optional. apply_input_mode already attempted this notification, but
                    // keep the action-local clone in sync too; IPCService deduplicates it.
                    if let Some(ipc_service) = ipc_service.as_mut() {
                        if let Err(error) = ipc_service.set_input_mode(actual_mode.indicator()) {
                            tracing::warn!("Failed to update candidate UI input mode: {error:?}");
                        }
                    }

                    if requested_change || previous_mode != actual_mode {
                        selection_index = 0;
                        corresponding_count = 0;
                        preview = Arc::default();
                        suffix = Arc::default();
                        raw_input = Arc::default();
                        raw_hiragana = Arc::default();
                        candidates = Arc::default();
                        if let Some(ipc_service) = ipc_service.as_mut() {
                            if let Err(error) = ipc_service.clear_text() {
                                tracing::warn!(
                                    "Failed to clear server after input-mode change: {error:?}"
                                );
                            }
                        }
                    }
                }
                ClientAction::SetSelection(selection) => {
                    selection_index = match selection {
                        SetSelectionType::Up => max(0, selection_index - 1),
                        SetSelectionType::Down => min(
                            candidates.texts.len().saturating_sub(1) as i32,
                            selection_index + 1,
                        ),
                        SetSelectionType::Number(number) => {
                            (*number).clamp(0, candidates.texts.len().saturating_sub(1) as i32)
                        }
                    };

                    let index = usize::try_from(selection_index).unwrap_or(0);
                    let text = candidates
                        .texts
                        .get(index)
                        .context("candidate selection is out of range")?;
                    let sub_text = candidates
                        .sub_texts
                        .get(index)
                        .context("candidate subtext selection is out of range")?;
                    corresponding_count = *candidates
                        .corresponding_count
                        .get(index)
                        .context("candidate corresponding_count selection is out of range")?;

                    require_ipc_service!().set_selection(selection_index)?;
                    self.set_text(text, sub_text)?;
                    progress.visible_or_committed_effect = true;
                    abort_if_reentered!();
                    preview = Arc::new(text.clone());
                    suffix = Arc::new(sub_text.clone());
                    raw_hiragana = Arc::clone(&candidates.hiragana);
                }
                ClientAction::CommitPrefixAndAppend(text) => {
                    self.update_context(displayed_utf16_count(&preview, &suffix), &preview)?;
                    abort_if_reentered!();

                    let text = match mode {
                        InputMode::Kana => to_fullwidth(text, false),
                        InputMode::Latin => text.to_string(),
                    };
                    candidates = Arc::new(
                        require_ipc_service!()
                            .commit_prefix_and_append(corresponding_count, text)?,
                    );
                    selection_index = 0;

                    let text = candidates
                        .texts
                        .first()
                        .context("candidate text is empty")?;
                    let sub_text = candidates
                        .sub_texts
                        .first()
                        .context("candidate subtext is empty")?;
                    let replacement_text = candidate_display_text(text, sub_text);
                    self.shift_start(&preview, &replacement_text)?;
                    progress.visible_or_committed_effect = true;
                    abort_if_reentered!();

                    corresponding_count = *candidates
                        .corresponding_count
                        .first()
                        .context("candidate corresponding_count is empty")?;
                    preview = Arc::new(text.clone());
                    suffix = Arc::new(sub_text.clone());
                    raw_input = Arc::clone(&candidates.raw_input);
                    raw_hiragana = Arc::clone(&candidates.hiragana);

                    require_ipc_service!()
                        .set_candidate_state(&candidates.texts, selection_index)?;
                    self.update_pos()?;
                    abort_if_reentered!();

                    transition = CompositionState::Composing;
                }
                ClientAction::SetTextWithType(set_type) => {
                    let text = text_with_type(set_type, &raw_input, &raw_hiragana);
                    let entire_surface_count =
                        i32::try_from(raw_hiragana.chars().count()).unwrap_or(i32::MAX);

                    self.set_text(&text, "")?;
                    progress.visible_or_committed_effect = true;
                    abort_if_reentered!();
                    preview = Arc::new(text);
                    suffix = Arc::default();
                    corresponding_count = entire_surface_count;
                }
            }
        }

        let text_service = self.borrow()?;
        let mut composition = text_service.borrow_mut_composition()?;

        if !snapshot_is_current(generation, composition.generation) {
            drop(composition);
            drop(text_service);
            tracing::warn!("Discard stale composition writeback after TSF re-entry");
            if let Some(ipc_service) = ipc_service.as_mut() {
                if let Err(error) = ipc_service.hide_window() {
                    tracing::warn!("Failed to hide window after stale writeback: {error:?}");
                }
                if let Err(error) = ipc_service.clear_text() {
                    tracing::warn!("Failed to clear server after stale writeback: {error:?}");
                }
            }
            return Ok(());
        }

        composition.preview = preview;
        composition.state = transition;
        composition.selection_index = selection_index;
        composition.raw_input = raw_input;
        composition.raw_hiragana = raw_hiragana;
        composition.candidates = candidates;
        composition.suffix = suffix;
        composition.corresponding_count = corresponding_count;
        composition.generation = composition.generation.wrapping_add(1);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        candidate_display_text, displayed_utf16_count, failed_key_disposition,
        input_mode_transition, is_last_composing_character, snapshot_is_current, text_with_type,
        ActionProgress, ClientAction, Composition, CompositionState, FailedKeyDisposition,
        InputMode, SetTextType,
    };
    use std::sync::Arc;

    #[test]
    fn last_input_check_handles_multibyte_characters() {
        assert!(is_last_composing_character(""));
        assert!(is_last_composing_character("あ"));
        assert!(!is_last_composing_character("あい"));
        assert!(!is_last_composing_character("ab"));
    }

    #[test]
    fn function_key_text_uses_server_canonical_raw_input() {
        let raw_input = "kyou";
        let hiragana = "きょう";

        assert_eq!(
            text_with_type(&SetTextType::Hiragana, raw_input, hiragana),
            "きょう"
        );
        assert_eq!(
            text_with_type(&SetTextType::Katakana, raw_input, hiragana),
            "キョウ"
        );
        assert_eq!(
            text_with_type(&SetTextType::HalfKatakana, raw_input, hiragana),
            "ｷｮｳ"
        );
        assert_eq!(
            text_with_type(&SetTextType::FullLatin, raw_input, hiragana),
            "ｋｙｏｕ"
        );
        assert_eq!(
            text_with_type(&SetTextType::HalfLatin, raw_input, hiragana),
            "kyou"
        );
    }

    #[test]
    fn committed_prefix_replacement_keeps_the_new_suffix_visible() {
        assert_eq!(candidate_display_text("変換", "のこり"), "変換のこり");
    }

    #[test]
    fn displayed_composition_count_uses_utf16_code_units() {
        assert_eq!(displayed_utf16_count("変換😀", "後🚀"), 7);
    }

    #[test]
    fn nested_termination_invalidates_outer_snapshot() {
        assert!(snapshot_is_current(7, 7));
        assert!(!snapshot_is_current(7, 8));
        assert!(!snapshot_is_current(u64::MAX, 0));
    }

    #[test]
    fn local_reset_clears_stale_preedit_and_advances_generation() {
        let mut composition = Composition {
            preview: Arc::new("変換".to_owned()),
            suffix: Arc::new("中".to_owned()),
            raw_input: Arc::new("henkann".to_owned()),
            raw_hiragana: Arc::new("へんかん".to_owned()),
            corresponding_count: 4,
            selection_index: 2,
            state: CompositionState::Composing,
            generation: u64::MAX,
            ..Composition::default()
        };

        composition.reset();

        assert!(composition.preview.is_empty());
        assert!(composition.suffix.is_empty());
        assert!(composition.raw_input.is_empty());
        assert!(composition.raw_hiragana.is_empty());
        assert_eq!(composition.corresponding_count, 0);
        assert_eq!(composition.selection_index, 0);
        assert_eq!(composition.state, CompositionState::None);
        assert!(composition.tip_composition.is_none());
        assert_eq!(composition.generation, 0);
    }

    #[test]
    fn changing_input_mode_ends_the_composition_state() {
        let (state, actions) = input_mode_transition(
            CompositionState::Composing,
            InputMode::Kana,
            InputMode::Latin,
        );

        assert_eq!(state, CompositionState::None);
        assert_eq!(actions, [ClientAction::SetIMEMode(InputMode::Latin)]);
    }

    #[test]
    fn requesting_the_current_input_mode_preserves_composition_state() {
        let (state, actions) = input_mode_transition(
            CompositionState::Composing,
            InputMode::Kana,
            InputMode::Kana,
        );

        assert_eq!(state, CompositionState::Composing);
        assert_eq!(actions, [ClientAction::SetIMEMode(InputMode::Kana)]);
    }

    #[test]
    fn input_mode_toggle_moves_both_directions() {
        for (current, expected) in [
            (InputMode::Latin, InputMode::Kana),
            (InputMode::Kana, InputMode::Latin),
        ] {
            let (state, actions) =
                input_mode_transition(CompositionState::None, current, current.toggled());
            assert_eq!(state, CompositionState::None);
            assert_eq!(actions, [ClientAction::SetIMEMode(expected)]);
        }
    }

    #[test]
    fn failed_action_passes_through_only_after_no_effect_cleanup() {
        assert_eq!(
            failed_key_disposition(ActionProgress::default(), true),
            FailedKeyDisposition::PassThrough
        );

        assert_eq!(
            failed_key_disposition(
                ActionProgress {
                    started_composition: true,
                    ..ActionProgress::default()
                },
                true,
            ),
            FailedKeyDisposition::PassThrough
        );
    }

    #[test]
    fn failed_action_is_eaten_after_effect_or_failed_cleanup() {
        assert_eq!(
            failed_key_disposition(
                ActionProgress {
                    visible_or_committed_effect: true,
                    ..ActionProgress::default()
                },
                true,
            ),
            FailedKeyDisposition::Eat
        );
        assert_eq!(
            failed_key_disposition(
                ActionProgress {
                    must_eat_on_failure: true,
                    ..ActionProgress::default()
                },
                true,
            ),
            FailedKeyDisposition::Eat
        );
        assert_eq!(
            failed_key_disposition(ActionProgress::default(), false),
            FailedKeyDisposition::Eat
        );
    }
}
