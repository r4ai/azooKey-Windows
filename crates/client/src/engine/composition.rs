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
        Input::KeyboardAndMouse::VK_CONTROL,
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
            composition.preview = Arc::default();
            composition.suffix = Arc::default();
            composition.raw_input = Arc::default();
            composition.raw_hiragana = Arc::default();
            composition.corresponding_count = 0;
            composition.selection_index = 0;
            composition.candidates = Arc::default();
            composition.state = CompositionState::None;
            composition.tip_composition = None;
            composition.generation = composition.generation.wrapping_add(1);
        }

        let ipc_service = match IMEState::get() {
            Ok(state) => state.ipc_service.clone(),
            Err(error) => {
                tracing::warn!(
                    "Failed to get IPC service after composition termination: {error:?}"
                );
                None
            }
        };
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

    #[tracing::instrument]
    pub fn process_key(
        &self,
        context: Option<&ITfContext>,
        wparam: WPARAM,
    ) -> Result<Option<(Vec<ClientAction>, CompositionState)>> {
        if context.is_none() {
            return Ok(None);
        };

        // check shortcut keys
        if VK_CONTROL.is_pressed() {
            return Ok(None);
        }

        let (composition_state, is_last_input, suffix_is_empty, mode) = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            let mode = IMEState::get()?.input_mode.clone();
            (
                composition.state,
                is_last_composing_character(&composition.raw_hiragana),
                composition.suffix.is_empty(),
                mode,
            )
        };

        let action = UserAction::try_from(wparam.0)?;

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
                UserAction::ToggleInputMode => (
                    CompositionState::None,
                    vec![match mode {
                        InputMode::Kana => ClientAction::SetIMEMode(InputMode::Latin),
                        InputMode::Latin => ClientAction::SetIMEMode(InputMode::Kana),
                    }],
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
                UserAction::ToggleInputMode => (
                    CompositionState::None,
                    vec![
                        ClientAction::EndComposition,
                        ClientAction::SetIMEMode(InputMode::Latin),
                    ],
                ),
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
                UserAction::ToggleInputMode => (
                    CompositionState::None,
                    vec![
                        ClientAction::EndComposition,
                        ClientAction::SetIMEMode(InputMode::Latin),
                    ],
                ),
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

        if let Some((actions, transition)) = self.process_key(context, wparam)? {
            self.handle_action(&actions, transition)?;
        } else {
            return Ok(false);
        }

        Ok(true)
    }

    #[tracing::instrument]
    pub fn handle_action(
        &self,
        actions: &[ClientAction],
        transition: CompositionState,
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
            mode,
        ) = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            let mode = IMEState::get()?.input_mode.clone();
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
        let mut ipc_service = IMEState::get()?
            .ipc_service
            .clone()
            .context("ipc_service is None")?;
        let mut transition = transition;
        macro_rules! abort_if_reentered {
            () => {
                if !snapshot_is_current(generation, self.composition_generation()?) {
                    tracing::warn!("Discard stale composition action after TSF re-entry");
                    if let Err(error) = ipc_service.hide_window() {
                        tracing::warn!("Failed to hide window after TSF re-entry: {error:?}");
                    }
                    if let Err(error) = ipc_service.clear_text() {
                        tracing::warn!("Failed to clear server after TSF re-entry: {error:?}");
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
                    abort_if_reentered!();
                    self.update_pos()?;
                    abort_if_reentered!();
                }
                ClientAction::EndComposition => {
                    self.end_composition()?;
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
                    ipc_service.hide_window()?;
                    ipc_service.clear_text()?;
                }
                ClientAction::DiscardComposition => {
                    // Clearing the TSF range before ending commits an empty string. In
                    // particular, do not call RemoveText here: its response runs an expensive
                    // conversion whose result is immediately thrown away.
                    self.set_text("", "")?;
                    abort_if_reentered!();
                    self.end_composition()?;
                    generation = self.composition_generation()?;
                    selection_index = 0;
                    corresponding_count = 0;
                    preview = Arc::default();
                    suffix = Arc::default();
                    raw_input = Arc::default();
                    raw_hiragana = Arc::default();
                    candidates = Arc::default();
                    ipc_service.hide_window()?;
                    ipc_service.clear_text()?;
                }
                ClientAction::AppendText(text) => {
                    let text = match mode {
                        InputMode::Kana => to_fullwidth(text, false),
                        InputMode::Latin => text.to_string(),
                    };

                    candidates = Arc::new(ipc_service.append_text(text)?);
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
                    abort_if_reentered!();
                    preview = Arc::new(text.clone());
                    suffix = Arc::new(sub_text.clone());
                    raw_input = Arc::clone(&candidates.raw_input);
                    raw_hiragana = Arc::clone(&candidates.hiragana);

                    ipc_service.set_candidate_state(&candidates.texts, selection_index)?;
                }
                ClientAction::RemoveText => {
                    candidates = Arc::new(ipc_service.remove_text()?);
                    selection_index = 0;
                    let text = candidates.texts.first().map(String::as_str).unwrap_or("");
                    let sub_text = candidates
                        .sub_texts
                        .first()
                        .map(String::as_str)
                        .unwrap_or("");
                    corresponding_count = *candidates.corresponding_count.first().unwrap_or(&0);

                    self.set_text(text, sub_text)?;
                    abort_if_reentered!();
                    preview = Arc::new(text.to_owned());
                    suffix = Arc::new(sub_text.to_owned());
                    raw_input = Arc::clone(&candidates.raw_input);
                    raw_hiragana = Arc::clone(&candidates.hiragana);

                    ipc_service.set_candidate_state(&candidates.texts, selection_index)?;
                }
                ClientAction::MoveCursor(_offset) => {
                    // TODO: I'll use azookey-kkc's composingText
                    // self.set_cursor(offset)?;
                }
                ClientAction::SetIMEMode(mode) => {
                    let tip_exists = {
                        let text_service = self.borrow()?;
                        let tip_exists =
                            text_service.borrow_composition()?.tip_composition.is_some();
                        tip_exists
                    };
                    if tip_exists {
                        self.end_composition()?;
                        generation = self.composition_generation()?;
                    }

                    let mut ime_state = IMEState::get()?;
                    ime_state.input_mode = mode.clone();

                    // update the language bar
                    self.update_lang_bar()?;

                    let mode = match mode {
                        InputMode::Latin => "A",
                        InputMode::Kana => "あ",
                    };

                    ipc_service.set_input_mode(mode)?;

                    selection_index = 0;
                    corresponding_count = 0;
                    preview = Arc::default();
                    suffix = Arc::default();
                    raw_input = Arc::default();
                    raw_hiragana = Arc::default();
                    candidates = Arc::default();
                    ipc_service.clear_text()?;
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

                    ipc_service.set_selection(selection_index)?;
                    self.set_text(text, sub_text)?;
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
                    candidates =
                        Arc::new(ipc_service.commit_prefix_and_append(corresponding_count, text)?);
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
                    abort_if_reentered!();

                    corresponding_count = *candidates
                        .corresponding_count
                        .first()
                        .context("candidate corresponding_count is empty")?;
                    preview = Arc::new(text.clone());
                    suffix = Arc::new(sub_text.clone());
                    raw_input = Arc::clone(&candidates.raw_input);
                    raw_hiragana = Arc::clone(&candidates.hiragana);

                    ipc_service.set_candidate_state(&candidates.texts, selection_index)?;
                    self.update_pos()?;
                    abort_if_reentered!();

                    transition = CompositionState::Composing;
                }
                ClientAction::SetTextWithType(set_type) => {
                    let text = text_with_type(set_type, &raw_input, &raw_hiragana);
                    let entire_surface_count =
                        i32::try_from(raw_hiragana.chars().count()).unwrap_or(i32::MAX);

                    self.set_text(&text, "")?;
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
            if let Err(error) = ipc_service.hide_window() {
                tracing::warn!("Failed to hide window after stale writeback: {error:?}");
            }
            if let Err(error) = ipc_service.clear_text() {
                tracing::warn!("Failed to clear server after stale writeback: {error:?}");
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
        candidate_display_text, displayed_utf16_count, is_last_composing_character,
        snapshot_is_current, text_with_type, SetTextType,
    };

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
}
