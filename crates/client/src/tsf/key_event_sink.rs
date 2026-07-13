use windows::{
    core::GUID,
    Win32::{
        Foundation::{BOOL, LPARAM, WPARAM},
        UI::{
            Input::KeyboardAndMouse::VK_BACK,
            TextServices::{ITfContext, ITfKeyEventSink_Impl},
        },
    },
};

use anyhow::Result;
use std::time::Instant;

use super::factory::TextServiceFactory_Impl;

const PREVIOUS_KEY_STATE_BIT: usize = 1 << 30;

fn is_key_repeat(lparam: LPARAM) -> bool {
    lparam.0 as usize & PREVIOUS_KEY_STATE_BIT != 0
}

fn is_backspace(wparam: WPARAM) -> bool {
    wparam.0 == VK_BACK.0 as usize
}

fn should_claim_pending_cleanup(has_context: bool, has_pending_cleanup: bool) -> bool {
    has_context && has_pending_cleanup
}

// sink (aka event listener) for key events
impl ITfKeyEventSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    #[tracing::instrument]
    fn OnTestKeyDown(
        &self,
        pic: Option<&ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<BOOL> {
        if is_backspace(wparam)
            && self
                .borrow()?
                .backspace_repeat_state
                .should_suppress(is_key_repeat(lparam), Instant::now())
        {
            // Claim queued auto-repeat events even if the previous event ended the
            // composition. Otherwise the host application can receive the backlog and
            // unexpectedly delete committed text.
            return Ok(true.into());
        }

        // Composition recovery and external compartment changes are finalized only from the
        // real OnKeyDown safe point. Claim one test event so handle_key can safely retry them.
        if should_claim_pending_cleanup(pic.is_some(), self.has_pending_key_cleanup()?) {
            return Ok(true.into());
        }

        // this function checks if the key event will be handled by "OnKeyUp" function
        // so we need to return TRUE if we want to handle the key event
        let result = self.process_key(pic, wparam)?.is_some();

        Ok(result.into())
    }

    #[macros::anyhow]
    #[tracing::instrument]
    fn OnKeyDown(&self, pic: Option<&ITfContext>, wparam: WPARAM, lparam: LPARAM) -> Result<BOOL> {
        // this function is called when a key is pressed
        // we can handle key events here
        if is_backspace(wparam)
            && self
                .borrow()?
                .backspace_repeat_state
                .should_suppress(is_key_repeat(lparam), Instant::now())
        {
            return Ok(true.into());
        }

        let result = self.handle_key(pic, wparam)?;
        if result && is_backspace(wparam) {
            self.borrow_mut()?
                .backspace_repeat_state
                .mark_handled(Instant::now());
        }

        Ok(result.into())
    }

    #[macros::anyhow]
    fn OnTestKeyUp(
        &self,
        _pic: Option<&ITfContext>,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        // same as OnTestKeyDown
        Ok(false.into())
    }

    #[macros::anyhow]
    fn OnKeyUp(&self, _pic: Option<&ITfContext>, _wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
        // this function is called when a key is released
        // but we handle key events in OnKeyDown function
        // so just return S_OK
        Ok(false.into())
    }

    #[macros::anyhow]
    fn OnPreservedKey(&self, _pic: Option<&ITfContext>, _rguid: *const GUID) -> Result<BOOL> {
        // this function is actually not used
        Ok(true.into())
    }

    #[macros::anyhow]
    fn OnSetFocus(&self, _fforeground: BOOL) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_only_the_previous_key_state_bit() {
        assert!(!is_key_repeat(LPARAM(0)));
        assert!(!is_key_repeat(LPARAM(1)));
        assert!(is_key_repeat(LPARAM(PREVIOUS_KEY_STATE_BIT as isize)));
    }

    #[test]
    fn pending_cleanup_is_claimed_before_ipc_key_classification() {
        assert!(should_claim_pending_cleanup(true, true));
        assert!(!should_claim_pending_cleanup(true, false));
        assert!(!should_claim_pending_cleanup(false, true));
    }
}
