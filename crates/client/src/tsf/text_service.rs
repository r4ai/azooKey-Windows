use std::{
    cell::{Cell, Ref, RefCell, RefMut},
    collections::HashMap,
    time::{Duration, Instant},
};

use windows::{
    core::{Interface, GUID},
    Win32::UI::TextServices::{ITfContext, ITfTextInputProcessor, ITfThreadMgr},
};

use anyhow::{Context, Result};

use crate::engine::{composition::Composition, input_mode::InputMode};

const BACKSPACE_REPEAT_COALESCE_WINDOW: Duration = Duration::from_millis(80);

#[derive(Debug, Default)]
pub struct BackspaceRepeatState {
    last_handled_at: Option<Instant>,
}

impl BackspaceRepeatState {
    pub fn should_suppress(&self, is_repeat: bool, now: Instant) -> bool {
        is_repeat
            && self.last_handled_at.is_some_and(|last| {
                now.saturating_duration_since(last) < BACKSPACE_REPEAT_COALESCE_WINDOW
            })
    }

    pub fn mark_handled(&mut self, now: Instant) {
        self.last_handled_at = Some(now);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdatePosState {
    #[default]
    Idle,
    Updating {
        suppress_layout_until: Instant,
    },
    SuppressingLayoutChange {
        until: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingInputModeTransition {
    pub from: InputMode,
    pub to: InputMode,
}

pub fn next_pending_input_mode_transition(
    pending: Option<PendingInputModeTransition>,
    previous_mode: InputMode,
    next_mode: InputMode,
    has_active_composition: bool,
) -> Option<PendingInputModeTransition> {
    if !has_active_composition || previous_mode == next_mode {
        return if has_active_composition {
            pending
        } else {
            None
        };
    }

    let from = pending.map_or(previous_mode, |pending| pending.from);
    if from == next_mode {
        None
    } else {
        Some(PendingInputModeTransition {
            from,
            to: next_mode,
        })
    }
}

impl UpdatePosState {
    const LAYOUT_CHANGE_SUPPRESSION: Duration = Duration::from_millis(200);

    pub fn try_begin_update(&mut self, now: Instant) -> bool {
        if matches!(self, Self::Updating { .. }) {
            return false;
        }

        *self = Self::Updating {
            suppress_layout_until: now + Self::LAYOUT_CHANGE_SUPPRESSION,
        };

        true
    }

    pub fn finish_update(&mut self, now: Instant) {
        *self = match *self {
            Self::Updating {
                suppress_layout_until,
            } if now <= suppress_layout_until => Self::SuppressingLayoutChange {
                until: suppress_layout_until,
            },
            Self::Updating { .. } => Self::Idle,
            state => state,
        };
    }

    pub fn should_skip_layout_change(&mut self, now: Instant) -> bool {
        match *self {
            Self::Idle => false,
            Self::Updating { .. } => true,
            Self::SuppressingLayoutChange { until } if now <= until => true,
            Self::SuppressingLayoutChange { .. } => {
                *self = Self::Idle;
                false
            }
        }
    }
}

#[derive(Default, Debug)]
pub struct TextService {
    pub tid: u32,
    pub thread_mgr: Option<ITfThreadMgr>,
    pub dll_ref_held: bool,
    pub key_event_sink_advised: bool,
    pub preserved_input_mode_keys: u8,
    pub thread_mgr_event_cookie: Option<u32>,
    pub text_layout_context: Option<ITfContext>,
    pub text_layout_cookie: Option<u32>,
    pub lang_bar_added: bool,
    pub context: Option<ITfContext>,
    pub composition: RefCell<Composition>,
    pub update_pos_state: UpdatePosState,
    pub backspace_repeat_state: BackspaceRepeatState,
    pub display_attribute_atom: HashMap<GUID, u32>,
    pub mode: Cell<InputMode>,
    pub open_close_cookie: Option<u32>,
    pub conversion_mode_cookie: Option<u32>,
    pub compartment_write_in_progress: bool,
    pub pending_input_mode_transition: Cell<Option<PendingInputModeTransition>>,
    pub pending_composition_cleanup: Cell<bool>,
    pub this: Option<ITfTextInputProcessor>,
}

impl TextService {
    pub fn this<I: Interface>(&self) -> Result<I> {
        if let Some(this) = self.this.as_ref() {
            Ok(this.cast()?)
        } else {
            anyhow::bail!("this is null");
        }
    }

    pub fn thread_mgr(&self) -> Result<ITfThreadMgr> {
        self.thread_mgr.clone().context("Thread manager is null")
    }

    pub fn context<I: Interface>(&self) -> Result<I> {
        let context = self.context.as_ref().context("Context is null")?;
        Ok(context.cast()?)
    }

    pub fn borrow_composition(&self) -> Result<Ref<Composition>> {
        Ok(self.composition.try_borrow()?)
    }

    pub fn borrow_mut_composition(&self) -> Result<RefMut<Composition>> {
        Ok(self.composition.try_borrow_mut()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backspace_repeat_state_only_coalesces_fast_repeats() {
        let start = Instant::now();
        let mut state = BackspaceRepeatState::default();
        state.mark_handled(start);

        assert!(!state.should_suppress(false, start + Duration::from_millis(1)));
        assert!(state.should_suppress(true, start + Duration::from_millis(79)));
        assert!(!state.should_suppress(true, start + Duration::from_millis(80)));
    }

    #[test]
    fn pending_mode_transition_coalesces_to_the_latest_external_mode() {
        let pending =
            next_pending_input_mode_transition(None, InputMode::Kana, InputMode::Latin, true);
        assert_eq!(
            pending,
            Some(PendingInputModeTransition {
                from: InputMode::Kana,
                to: InputMode::Latin,
            })
        );

        assert_eq!(
            next_pending_input_mode_transition(pending, InputMode::Latin, InputMode::Kana, true,),
            None
        );
    }

    #[test]
    fn mode_change_without_a_composition_does_not_stay_pending() {
        assert_eq!(
            next_pending_input_mode_transition(
                Some(PendingInputModeTransition {
                    from: InputMode::Kana,
                    to: InputMode::Latin,
                }),
                InputMode::Latin,
                InputMode::Kana,
                false,
            ),
            None
        );
    }
}
