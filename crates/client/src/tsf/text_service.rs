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
    last_batch_started_at: Option<Instant>,
    pending_count: u32,
}

impl BackspaceRepeatState {
    /// Returns whether this repeat should be retained for the next batch instead of running a
    /// conversion immediately. The window is measured from the start of the previous batch, so
    /// synchronous conversion latency does not add another artificial pause.
    pub fn should_defer(&self, is_repeat: bool, now: Instant) -> bool {
        is_repeat
            && self.last_batch_started_at.is_some_and(|last| {
                now.saturating_duration_since(last) < BACKSPACE_REPEAT_COALESCE_WINDOW
            })
    }

    pub fn defer(&mut self, count: u32) {
        self.pending_count = self.pending_count.saturating_add(count.max(1));
    }

    pub fn batch_count(&self, current_count: u32) -> u32 {
        self.pending_count.saturating_add(current_count.max(1))
    }

    pub fn take_pending(&mut self) -> u32 {
        std::mem::take(&mut self.pending_count)
    }

    pub fn clear_pending(&mut self) {
        self.pending_count = 0;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn pending_count(&self) -> u32 {
        self.pending_count
    }

    pub fn mark_batch_started(&mut self, now: Instant) {
        self.last_batch_started_at = Some(now);
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
    fn backspace_repeat_state_preserves_deferred_deletions() {
        let start = Instant::now();
        let mut state = BackspaceRepeatState::default();
        state.mark_batch_started(start);

        assert!(!state.should_defer(false, start + Duration::from_millis(1)));
        assert!(state.should_defer(true, start + Duration::from_millis(79)));
        state.defer(2);
        state.defer(3);
        assert_eq!(state.pending_count(), 5);
        assert_eq!(state.batch_count(1), 6);
        state.clear_pending();
        assert_eq!(state.pending_count(), 0);
        assert!(!state.should_defer(true, start + Duration::from_millis(80)));
    }

    #[test]
    fn backspace_repeat_count_saturates_instead_of_wrapping() {
        let mut state = BackspaceRepeatState::default();
        state.defer(u32::MAX);
        state.defer(1);

        assert_eq!(state.take_pending(), u32::MAX);
    }

    #[test]
    fn terminated_composition_does_not_leak_deletions_into_the_next_one() {
        let start = Instant::now();
        let mut state = BackspaceRepeatState::default();
        state.mark_batch_started(start);
        state.defer(4);

        state.clear_pending();

        assert_eq!(state.batch_count(1), 1);
        // Keep the short safety window so queued repeats cannot reach committed host text.
        assert!(state.should_defer(true, start + Duration::from_millis(1)));

        state.reset();
        assert!(!state.should_defer(true, start + Duration::from_millis(1)));
    }

    #[test]
    fn thirty_hertz_repeat_sequence_keeps_every_deletion_while_batching_conversions() {
        let start = Instant::now();
        let mut state = BackspaceRepeatState::default();
        let mut processed_count = 0;
        let mut conversion_count = 0;

        for index in 0..30 {
            let now = start + Duration::from_millis(index * 33);
            let is_repeat = index != 0;
            if state.should_defer(is_repeat, now) {
                state.defer(1);
            } else {
                processed_count += state.batch_count(1);
                state.clear_pending();
                conversion_count += 1;
                state.mark_batch_started(now);
            }
        }
        processed_count += state.take_pending();

        assert_eq!(processed_count, 30);
        assert!(conversion_count < 15);
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
