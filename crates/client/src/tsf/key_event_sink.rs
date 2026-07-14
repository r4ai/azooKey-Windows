use windows::{
    core::{Interface as _, GUID},
    Win32::{
        Foundation::{BOOL, LPARAM, WPARAM},
        System::Ole::CONNECT_E_NOCONNECTION,
        UI::{
            Input::KeyboardAndMouse::{VK_BACK, VK_CONTROL, VK_KANJI, VK_MENU, VK_OEM_3, VK_SHIFT},
            TextServices::{
                ITfContext, ITfKeyEventSink_Impl, ITfKeystrokeMgr, TF_MOD_ALT,
                TF_MOD_IGNORE_ALL_MODIFIER, TF_PRESERVEDKEY,
            },
        },
    },
};

use anyhow::Result;
use std::time::Instant;

use crate::{diagnostics, extension::VKeyExt as _, globals::GUID_PRESERVED_KEY_INPUT_MODE};

use super::factory::{TextServiceFactory, TextServiceFactory_Impl};

const PREVIOUS_KEY_STATE_BIT: usize = 1 << 30;
const PRESERVED_ALT_GRAVE: u8 = 1 << 0;
const PRESERVED_KANJI: u8 = 1 << 1;
const INPUT_MODE_PRESERVED_KEYS: [(u8, TF_PRESERVEDKEY); 2] = [
    (
        PRESERVED_ALT_GRAVE,
        TF_PRESERVEDKEY {
            uVKey: VK_OEM_3.0 as u32,
            uModifiers: TF_MOD_ALT,
        },
    ),
    (
        PRESERVED_KANJI,
        TF_PRESERVEDKEY {
            uVKey: VK_KANJI.0 as u32,
            uModifiers: TF_MOD_IGNORE_ALL_MODIFIER,
        },
    ),
];

fn is_key_repeat(lparam: LPARAM) -> bool {
    lparam.0 as usize & PREVIOUS_KEY_STATE_BIT != 0
}

fn is_backspace(wparam: WPARAM) -> bool {
    wparam.0 == VK_BACK.0 as usize
}

fn should_claim_pending_cleanup(has_context: bool, has_pending_cleanup: bool) -> bool {
    has_context && has_pending_cleanup
}

fn is_input_mode_preserved_key(guid: &GUID) -> bool {
    *guid == GUID_PRESERVED_KEY_INPUT_MODE
}

fn with_preserved_key(mask: u8, marker: u8, owned: bool) -> u8 {
    if owned {
        mask | marker
    } else {
        mask & !marker
    }
}

fn should_use_direct_alt_grave(
    wparam: WPARAM,
    alt_pressed: bool,
    control_pressed: bool,
    preserved_keys: u8,
) -> bool {
    wparam.0 == VK_OEM_3.0 as usize
        && alt_pressed
        && !control_pressed
        // Some TSF hosts surface both the ordinary key event and OnPreservedKey. Use the
        // direct path only when this instance does not own the preserved shortcut, otherwise
        // one press could toggle twice.
        && preserved_keys & PRESERVED_ALT_GRAVE == 0
}

fn should_log_input_mode_key(wparam: WPARAM, alt_pressed: bool) -> bool {
    matches!(
        wparam.0,
        0x15 | 0x16 | 0x19 | 0x1A | 0xF0 | 0xF2 | 0xF3 | 0xF4
    ) || (wparam.0 == VK_OEM_3.0 as usize && alt_pressed)
}

fn log_mode_key_event(
    stage: &'static str,
    route: &'static str,
    wparam: WPARAM,
    preserved_keys: u8,
    eaten: bool,
) {
    let alt_pressed = VK_MENU.is_pressed();
    if !should_log_input_mode_key(wparam, alt_pressed) {
        return;
    }
    diagnostics::event(
        "mode_key",
        format_args!(
            "stage={} route={} vk={} alt={} ctrl={} shift={} preserved_mask={} eaten={}",
            stage,
            route,
            wparam.0,
            alt_pressed,
            VK_CONTROL.is_pressed(),
            VK_SHIFT.is_pressed(),
            preserved_keys,
            eaten
        ),
    );
}

impl TextServiceFactory {
    pub(super) fn preserve_input_mode_keys(&self) -> Result<()> {
        let (thread_mgr, tid, registered) = {
            let text_service = self.borrow()?;
            (
                text_service.thread_mgr()?,
                text_service.tid,
                text_service.preserved_input_mode_keys,
            )
        };
        let keystroke_mgr = thread_mgr.cast::<ITfKeystrokeMgr>()?;
        let mut first_error = None;

        for (marker, key) in INPUT_MODE_PRESERVED_KEYS {
            if registered & marker != 0 {
                continue;
            }

            // Track ownership before calling out to COM. PreserveKey can re-enter this object;
            // an optimistic marker prevents an untracked successful registration if storing
            // state after the call were to fail.
            {
                let mut text_service = self.borrow_mut()?;
                text_service.preserved_input_mode_keys =
                    with_preserved_key(text_service.preserved_input_mode_keys, marker, true);
            }
            match unsafe {
                keystroke_mgr.PreserveKey(tid, &GUID_PRESERVED_KEY_INPUT_MODE, &key, &[])
            } {
                Ok(()) => diagnostics::event(
                    "preserve_key",
                    format_args!("vk={} modifiers={} status=ok", key.uVKey, key.uModifiers),
                ),
                Err(error) => {
                    // We do not own a rejected registration and must not unpreserve another
                    // service's key during Deactivate. If clearing fails, retain the marker
                    // conservatively; UnpreserveKey's NOCONNECTION result is normalized below.
                    match self.borrow_mut() {
                        Ok(mut text_service) => {
                            text_service.preserved_input_mode_keys = with_preserved_key(
                                text_service.preserved_input_mode_keys,
                                marker,
                                false,
                            );
                        }
                        Err(clear_error) => tracing::error!(
                            "Failed to clear rejected preserved-key marker: {clear_error:?}"
                        ),
                    }
                    tracing::warn!(
                        virtual_key = key.uVKey,
                        modifiers = key.uModifiers,
                        "Failed to preserve input-mode key: {error:?}"
                    );
                    diagnostics::event(
                        "preserve_key",
                        format_args!(
                            "vk={} modifiers={} status=error hr=0x{:08X}",
                            key.uVKey,
                            key.uModifiers,
                            error.code().0 as u32
                        ),
                    );
                    if first_error.is_none() {
                        first_error = Some(error.into());
                    }
                }
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    pub(super) fn unpreserve_input_mode_keys(&self) -> Result<()> {
        let (thread_mgr, registered) = {
            let text_service = self.borrow()?;
            (
                text_service.thread_mgr()?,
                text_service.preserved_input_mode_keys,
            )
        };
        if registered == 0 {
            return Ok(());
        }

        let keystroke_mgr = thread_mgr.cast::<ITfKeystrokeMgr>()?;
        let mut first_error = None;
        for (marker, key) in INPUT_MODE_PRESERVED_KEYS {
            if registered & marker == 0 {
                continue;
            }

            match unsafe { keystroke_mgr.UnpreserveKey(&GUID_PRESERVED_KEY_INPUT_MODE, &key) } {
                Ok(()) => {
                    let mut text_service = self.borrow_mut()?;
                    text_service.preserved_input_mode_keys =
                        with_preserved_key(text_service.preserved_input_mode_keys, marker, false);
                    diagnostics::event(
                        "unpreserve_key",
                        format_args!("vk={} modifiers={} status=ok", key.uVKey, key.uModifiers),
                    );
                }
                Err(error) if error.code() == CONNECT_E_NOCONNECTION => {
                    // The registration is already absent (for example, after TSF performed
                    // external cleanup). Treat that as successful teardown.
                    let mut text_service = self.borrow_mut()?;
                    text_service.preserved_input_mode_keys =
                        with_preserved_key(text_service.preserved_input_mode_keys, marker, false);
                    diagnostics::event(
                        "unpreserve_key",
                        format_args!(
                            "vk={} modifiers={} status=already_absent",
                            key.uVKey, key.uModifiers
                        ),
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        virtual_key = key.uVKey,
                        modifiers = key.uModifiers,
                        "Failed to unpreserve input-mode key: {error:?}"
                    );
                    diagnostics::event(
                        "unpreserve_key",
                        format_args!(
                            "vk={} modifiers={} status=error hr=0x{:08X}",
                            key.uVKey,
                            key.uModifiers,
                            error.code().0 as u32
                        ),
                    );
                    if first_error.is_none() {
                        first_error = Some(error.into());
                    }
                }
            }
        }

        first_error.map_or(Ok(()), Err)
    }
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

        let preserved_keys = self.borrow()?.preserved_input_mode_keys;
        if pic.is_some()
            && should_use_direct_alt_grave(
                wparam,
                VK_MENU.is_pressed(),
                VK_CONTROL.is_pressed(),
                preserved_keys,
            )
        {
            log_mode_key_event(
                "test_down",
                "direct_alt_grave",
                wparam,
                preserved_keys,
                true,
            );
            return Ok(true.into());
        }

        // this function checks if the key event will be handled by "OnKeyUp" function
        // so we need to return TRUE if we want to handle the key event
        let result = self.process_key(pic, wparam)?.is_some();
        log_mode_key_event("test_down", "key_sink", wparam, preserved_keys, result);

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

        let preserved_keys = self.borrow()?.preserved_input_mode_keys;
        let direct_alt_grave = should_use_direct_alt_grave(
            wparam,
            VK_MENU.is_pressed(),
            VK_CONTROL.is_pressed(),
            preserved_keys,
        );
        let result = if direct_alt_grave {
            self.handle_input_mode_toggle(pic)?
        } else {
            self.handle_key(pic, wparam)?
        };
        log_mode_key_event(
            "key_down",
            if direct_alt_grave {
                "direct_alt_grave"
            } else {
                "key_sink"
            },
            wparam,
            preserved_keys,
            result,
        );
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
    fn OnPreservedKey(&self, pic: Option<&ITfContext>, rguid: *const GUID) -> Result<BOOL> {
        let Some(guid) = (unsafe { rguid.as_ref() }) else {
            diagnostics::event(
                "preserved_key",
                format_args!("matched=false reason=null_guid context={}", pic.is_some()),
            );
            return Ok(false.into());
        };
        if !is_input_mode_preserved_key(guid) {
            diagnostics::event(
                "preserved_key",
                format_args!(
                    "matched=false reason=unknown_guid context={}",
                    pic.is_some()
                ),
            );
            return Ok(false.into());
        }

        let eaten = self.handle_input_mode_toggle(pic)?;
        diagnostics::event(
            "preserved_key",
            format_args!("matched=true context={} eaten={}", pic.is_some(), eaten),
        );
        Ok(eaten.into())
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

    #[test]
    fn only_the_input_mode_command_guid_is_claimed() {
        assert!(is_input_mode_preserved_key(&GUID_PRESERVED_KEY_INPUT_MODE));
        assert!(!is_input_mode_preserved_key(&GUID::zeroed()));
    }

    #[test]
    fn preserved_key_definitions_cover_101_and_japanese_keyboards() {
        assert_eq!(INPUT_MODE_PRESERVED_KEYS[0].1.uVKey, VK_OEM_3.0 as u32);
        assert_eq!(INPUT_MODE_PRESERVED_KEYS[0].1.uModifiers, TF_MOD_ALT);
        assert_eq!(INPUT_MODE_PRESERVED_KEYS[1].1.uVKey, VK_KANJI.0 as u32);
        assert_eq!(
            INPUT_MODE_PRESERVED_KEYS[1].1.uModifiers,
            TF_MOD_IGNORE_ALL_MODIFIER
        );
    }

    #[test]
    fn direct_alt_grave_is_only_a_fallback_for_an_unowned_shortcut() {
        let grave = WPARAM(VK_OEM_3.0 as usize);
        assert!(should_use_direct_alt_grave(grave, true, false, 0));
        assert!(!should_use_direct_alt_grave(grave, false, false, 0));
        assert!(!should_use_direct_alt_grave(grave, true, true, 0));
        assert!(!should_use_direct_alt_grave(
            grave,
            true,
            false,
            PRESERVED_ALT_GRAVE
        ));
        assert!(!should_use_direct_alt_grave(WPARAM(0x41), true, false, 0));
    }

    #[test]
    fn preserved_key_ownership_is_tracked_per_shortcut() {
        let only_alt_grave = with_preserved_key(0, PRESERVED_ALT_GRAVE, true);
        assert_eq!(only_alt_grave, PRESERVED_ALT_GRAVE);

        let both = with_preserved_key(only_alt_grave, PRESERVED_KANJI, true);
        assert_eq!(both, PRESERVED_ALT_GRAVE | PRESERVED_KANJI);

        assert_eq!(
            with_preserved_key(both, PRESERVED_ALT_GRAVE, false),
            PRESERVED_KANJI
        );
    }

    #[test]
    fn diagnostic_filter_excludes_ordinary_typing_keys() {
        for key in [0x15, 0x16, 0x19, 0x1A, 0xF0, 0xF2, 0xF3, 0xF4] {
            assert!(should_log_input_mode_key(WPARAM(key), false));
        }
        assert!(should_log_input_mode_key(WPARAM(VK_OEM_3.0 as usize), true));
        assert!(!should_log_input_mode_key(
            WPARAM(VK_OEM_3.0 as usize),
            false
        ));
        for key in [0x08, 0x20, 0x41, 0x5A] {
            assert!(!should_log_input_mode_key(WPARAM(key), false));
        }
    }
}
