use anyhow::{Context, Result};
use windows::{
    core::{Interface, GUID, VARIANT},
    Win32::UI::TextServices::{
        ITfCompartmentEventSink, ITfCompartmentEventSink_Impl, ITfCompartmentMgr, ITfSource,
        GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
        TF_CONVERSIONMODE_FULLSHAPE, TF_CONVERSIONMODE_NATIVE, TF_CONVERSIONMODE_ROMAN,
    },
};

use crate::{
    diagnostics,
    engine::{composition::CompositionState, input_mode::InputMode, state::IMEState},
};

use super::{
    factory::{TextServiceFactory, TextServiceFactory_Impl},
    text_service::next_pending_input_mode_transition,
};

const HIRAGANA_CONVERSION_MODE: i32 =
    (TF_CONVERSIONMODE_NATIVE | TF_CONVERSIONMODE_FULLSHAPE | TF_CONVERSIONMODE_ROMAN) as i32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompartmentUpdate {
    Conversion(i32),
    OpenClose(i32),
}

const LATIN_UPDATES: [CompartmentUpdate; 1] = [CompartmentUpdate::OpenClose(0)];
const KANA_UPDATES: [CompartmentUpdate; 2] = [
    CompartmentUpdate::Conversion(HIRAGANA_CONVERSION_MODE),
    CompartmentUpdate::OpenClose(1),
];

fn compartment_updates(mode: InputMode) -> &'static [CompartmentUpdate] {
    match mode {
        InputMode::Latin => &LATIN_UPDATES,
        InputMode::Kana => &KANA_UPDATES,
    }
}

fn input_mode_from_compartments(
    open_close: Option<i32>,
    conversion_mode: Option<i32>,
) -> InputMode {
    let is_open = open_close.unwrap_or(0) != 0;
    let conversion_mode = conversion_mode.unwrap_or(HIRAGANA_CONVERSION_MODE) as u32;

    // Open/close and native/alphanumeric are independent TSF state. Activation normalizes a
    // conversion value inherited from another TIP before this reduction, while later external
    // conversion changes remain observable through the advised compartment sink.
    if is_open && conversion_mode & TF_CONVERSIONMODE_NATIVE != 0 {
        InputMode::Kana
    } else {
        InputMode::Latin
    }
}

fn is_input_mode_compartment(guid: &GUID) -> bool {
    *guid == GUID_COMPARTMENT_KEYBOARD_OPENCLOSE
        || *guid == GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION
}

fn read_i4(compartment_mgr: &ITfCompartmentMgr, guid: &GUID) -> Result<Option<i32>> {
    let value = unsafe { compartment_mgr.GetCompartment(guid)?.GetValue()? };
    if value.is_empty() {
        return Ok(None);
    }

    Ok(Some(i32::try_from(&value)?))
}

fn set_i4_if_changed(
    compartment_mgr: &ITfCompartmentMgr,
    tid: u32,
    guid: &GUID,
    value: i32,
) -> Result<bool> {
    let compartment = unsafe { compartment_mgr.GetCompartment(guid)? };
    let current = unsafe { compartment.GetValue()? };
    let current = if current.is_empty() {
        None
    } else {
        Some(i32::try_from(&current)?)
    };

    if current == Some(value) {
        return Ok(false);
    }

    let value = VARIANT::from(value);
    unsafe { compartment.SetValue(tid, &value)? };
    Ok(true)
}

fn advise_compartment(
    compartment_mgr: &ITfCompartmentMgr,
    guid: &GUID,
    sink: &ITfCompartmentEventSink,
) -> Result<u32> {
    let compartment = unsafe { compartment_mgr.GetCompartment(guid)? };
    let source = compartment.cast::<ITfSource>()?;
    Ok(unsafe { source.AdviseSink(&ITfCompartmentEventSink::IID, sink)? })
}

fn unadvise_compartment(
    compartment_mgr: &ITfCompartmentMgr,
    guid: &GUID,
    cookie: u32,
) -> Result<()> {
    let compartment = unsafe { compartment_mgr.GetCompartment(guid)? };
    let source = compartment.cast::<ITfSource>()?;
    unsafe { source.UnadviseSink(cookie)? };
    Ok(())
}

impl TextServiceFactory {
    fn input_mode_compartment_mgr(&self) -> Result<(ITfCompartmentMgr, u32)> {
        let (thread_mgr, tid) = {
            let text_service = self.borrow()?;
            (text_service.thread_mgr()?, text_service.tid)
        };
        Ok((thread_mgr.cast()?, tid))
    }

    fn advise_input_mode_compartments(&self) -> Result<()> {
        let (compartment_mgr, _) = self.input_mode_compartment_mgr()?;
        let sink = {
            let text_service = self.borrow()?;
            text_service.this::<ITfCompartmentEventSink>()?
        };

        let open_close_cookie = advise_compartment(
            &compartment_mgr,
            &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
            &sink,
        )?;

        let conversion_mode_cookie = match advise_compartment(
            &compartment_mgr,
            &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
            &sink,
        ) {
            Ok(cookie) => cookie,
            Err(error) => {
                if let Err(rollback_error) = unadvise_compartment(
                    &compartment_mgr,
                    &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
                    open_close_cookie,
                ) {
                    tracing::warn!(
                        "Failed to roll back open/close compartment sink: {rollback_error:?}"
                    );
                    // The sink is still live. Retain its cookie so Deactivate can retry instead
                    // of losing an untracked callback registration.
                    match self.borrow_mut() {
                        Ok(mut text_service) => {
                            text_service.open_close_cookie = Some(open_close_cookie)
                        }
                        Err(store_error) => tracing::error!(
                            "Failed to retain live open/close cookie: {store_error:?}"
                        ),
                    }
                }
                return Err(error);
            }
        };

        // AdviseSink is allowed to call back synchronously. Store the cookies only after both
        // COM calls have completed, with no RefCell borrow spanning either call.
        let store_result = (|| -> Result<()> {
            let mut text_service = self.borrow_mut()?;
            text_service.open_close_cookie = Some(open_close_cookie);
            text_service.conversion_mode_cookie = Some(conversion_mode_cookie);
            Ok(())
        })();

        if let Err(error) = store_result {
            let mut live_open_close_cookie = None;
            let mut live_conversion_mode_cookie = None;
            for (guid, cookie, is_open_close) in [
                (
                    &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
                    open_close_cookie,
                    true,
                ),
                (
                    &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
                    conversion_mode_cookie,
                    false,
                ),
            ] {
                if let Err(rollback_error) = unadvise_compartment(&compartment_mgr, guid, cookie) {
                    tracing::warn!(
                        "Failed to roll back input-mode compartment sink: {rollback_error:?}"
                    );
                    if is_open_close {
                        live_open_close_cookie = Some(cookie);
                    } else {
                        live_conversion_mode_cookie = Some(cookie);
                    }
                }
            }
            if live_open_close_cookie.is_some() || live_conversion_mode_cookie.is_some() {
                match self.borrow_mut() {
                    Ok(mut text_service) => {
                        text_service.open_close_cookie = live_open_close_cookie;
                        text_service.conversion_mode_cookie = live_conversion_mode_cookie;
                    }
                    Err(store_error) => tracing::error!(
                        "Failed to retain live compartment cookies: {store_error:?}"
                    ),
                }
            }
            return Err(error);
        }

        Ok(())
    }

    pub fn activate_input_mode_compartments(&self) -> Result<()> {
        self.advise_input_mode_compartments()?;

        let initialize_result = (|| -> Result<()> {
            let (compartment_mgr, tid) = self.input_mode_compartment_mgr()?;

            // Conversion compartments are shared across the thread and can retain another
            // TIP's non-native mode. AzooKey supports Hiragana for its open state, so normalize
            // that metadata on every activation while leaving open/close under Windows control.
            let conversion_changed = set_i4_if_changed(
                &compartment_mgr,
                tid,
                &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
                HIRAGANA_CONVERSION_MODE,
            )?;
            diagnostics::event(
                "compartment_normalize",
                format_args!(
                    "conversion={} changed={}",
                    HIRAGANA_CONVERSION_MODE, conversion_changed
                ),
            );

            self.sync_input_mode_from_compartments(true)?;
            Ok(())
        })();

        if let Err(error) = initialize_result {
            if let Err(cleanup_error) = self.unadvise_input_mode_compartments() {
                tracing::warn!(
                    "Failed to clean up input-mode compartments after activation error: {cleanup_error:?}"
                );
            }
            return Err(error);
        }

        Ok(())
    }

    pub fn unadvise_input_mode_compartments(&self) -> Result<()> {
        let (open_close_cookie, conversion_mode_cookie) = {
            let text_service = self.borrow()?;
            (
                text_service.open_close_cookie,
                text_service.conversion_mode_cookie,
            )
        };
        if open_close_cookie.is_none() && conversion_mode_cookie.is_none() {
            return Ok(());
        }

        let (thread_mgr, open_close_cookie, conversion_mode_cookie) = {
            let text_service = self.borrow()?;
            (
                text_service.thread_mgr()?,
                text_service.open_close_cookie,
                text_service.conversion_mode_cookie,
            )
        };
        let compartment_mgr = thread_mgr.cast::<ITfCompartmentMgr>()?;

        // Always attempt both unadvises. Keep a cookie until its own UnadviseSink succeeds so a
        // failed Deactivate retains enough state for a later retry.
        let mut first_error = None;
        for (guid, cookie, is_open_close) in [
            (
                &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
                open_close_cookie,
                true,
            ),
            (
                &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
                conversion_mode_cookie,
                false,
            ),
        ] {
            let Some(cookie) = cookie else {
                continue;
            };
            match unadvise_compartment(&compartment_mgr, guid, cookie) {
                Ok(()) => {
                    let clear_result = (|| -> Result<()> {
                        let mut text_service = self.borrow_mut()?;
                        let stored_cookie = if is_open_close {
                            &mut text_service.open_close_cookie
                        } else {
                            &mut text_service.conversion_mode_cookie
                        };
                        if *stored_cookie == Some(cookie) {
                            *stored_cookie = None;
                        }
                        Ok(())
                    })();
                    if let Err(error) = clear_result {
                        tracing::warn!(
                            "Unadvised input-mode compartment but failed to clear its cookie: {error:?}"
                        );
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!("Failed to unadvise input-mode compartment: {error:?}");
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    fn apply_input_mode(&self, mode: InputMode, force_notification: bool) -> Result<()> {
        let changed = {
            let text_service = self.borrow()?;
            let changed = text_service.mode.get() != mode;
            text_service.mode.set(mode);
            changed
        };

        if !changed && !force_notification {
            return Ok(());
        }

        // Both calls can re-enter this COM object. The TextService borrow above is deliberately
        // dropped first, and UI IPC is best-effort so input mode stays usable without the UI.
        if let Err(error) = self.update_lang_bar() {
            tracing::warn!("Failed to update language bar input mode: {error:?}");
        }

        let ipc_service = IMEState::ipc_snapshot();
        if let Some(mut ipc_service) = ipc_service {
            if let Err(error) = ipc_service.set_input_mode(mode.indicator()) {
                tracing::warn!("Failed to update candidate UI input mode: {error:?}");
            }
        }

        Ok(())
    }

    fn sync_input_mode_from_compartments(&self, force_notification: bool) -> Result<InputMode> {
        let (compartment_mgr, _) = self.input_mode_compartment_mgr()?;
        let open_close = read_i4(&compartment_mgr, &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)?;
        let conversion_mode = read_i4(
            &compartment_mgr,
            &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
        )?;
        let mode = input_mode_from_compartments(open_close, conversion_mode);
        diagnostics::event(
            "compartment_state",
            format_args!(
                "open_close={:?} conversion={:?} resolved={:?}",
                open_close, conversion_mode, mode
            ),
        );
        self.apply_input_mode(mode, force_notification)?;
        Ok(mode)
    }

    pub fn set_input_mode_compartments(&self, requested_mode: InputMode) -> Result<InputMode> {
        diagnostics::event(
            "mode_request",
            format_args!("requested={:?}", requested_mode),
        );
        // Update the per-TextService authority first, and release its RefCell borrow before any
        // SetValue. SetValue can synchronously invoke OnChange on this same object.
        if let Err(error) = self.apply_input_mode(requested_mode, false) {
            diagnostics::event(
                "mode_result",
                format_args!("requested={:?} status=apply_error", requested_mode),
            );
            return Err(error);
        }

        let (compartment_mgr, tid) = self.input_mode_compartment_mgr()?;
        {
            self.borrow_mut()?.compartment_write_in_progress = true;
        }

        let write_result = (|| -> Result<()> {
            for update in compartment_updates(requested_mode) {
                match *update {
                    CompartmentUpdate::Conversion(value) => {
                        set_i4_if_changed(
                            &compartment_mgr,
                            tid,
                            &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
                            value,
                        )?;
                    }
                    CompartmentUpdate::OpenClose(value) => {
                        set_i4_if_changed(
                            &compartment_mgr,
                            tid,
                            &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
                            value,
                        )?;
                    }
                }
            }
            Ok(())
        })();

        // Synchronous OnChange calls were intentionally ignored while the multi-compartment
        // transaction was incomplete. Clear the guard, then reconcile against the final TSF
        // values. This also rolls the optimistic mode back if a SetValue failed.
        {
            self.borrow_mut()?.compartment_write_in_progress = false;
        }
        let sync_result = self.sync_input_mode_from_compartments(false);

        match (write_result, sync_result) {
            (Ok(()), Ok(mode)) => {
                diagnostics::event(
                    "mode_result",
                    format_args!("requested={:?} actual={:?} status=ok", requested_mode, mode),
                );
                Ok(mode)
            }
            (Err(error), Ok(mode)) => {
                diagnostics::event(
                    "mode_result",
                    format_args!(
                        "requested={:?} actual={:?} status=write_error",
                        requested_mode, mode
                    ),
                );
                Err(error)
            }
            (Ok(()), Err(error)) => {
                diagnostics::event(
                    "mode_result",
                    format_args!("requested={:?} status=sync_error", requested_mode),
                );
                Err(error)
            }
            (Err(error), Err(sync_error)) => {
                diagnostics::event(
                    "mode_result",
                    format_args!("requested={:?} status=write_and_sync_error", requested_mode),
                );
                tracing::warn!(
                    "Failed to resync input mode after compartment write error: {sync_error:?}"
                );
                Err(error)
            }
        }
    }
}

impl ITfCompartmentEventSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnChange(&self, rguid: *const GUID) -> Result<()> {
        let guid = unsafe { rguid.as_ref() }.context("Compartment GUID is null")?;
        if !is_input_mode_compartment(guid) {
            return Ok(());
        }
        // SetValue is synchronous and can re-enter here between the conversion and open/close
        // writes. The outbound path performs one final authoritative read after both writes.
        if self.borrow()?.compartment_write_in_progress {
            return Ok(());
        }
        diagnostics::event(
            "compartment_change",
            format_args!(
                "kind={}",
                if *guid == GUID_COMPARTMENT_KEYBOARD_OPENCLOSE {
                    "open_close"
                } else {
                    "conversion"
                }
            ),
        );

        // Never write compartments or request a synchronous edit session from OnChange.
        // External TSF changes only update per-service state and best-effort UI here.
        let previous_mode = self.borrow()?.mode.get();
        let mode = self.sync_input_mode_from_compartments(false)?;
        if previous_mode != mode {
            let (pending, has_active_composition) = {
                let text_service = self.borrow()?;
                let composition = text_service.borrow_composition()?;
                (
                    text_service.pending_input_mode_transition.get(),
                    composition.tip_composition.is_some()
                        || composition.state != CompositionState::None,
                )
            };
            let pending = next_pending_input_mode_transition(
                pending,
                previous_mode,
                mode,
                has_active_composition,
            );
            self.borrow()?.pending_input_mode_transition.set(pending);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compartment_values_reduce_to_supported_input_modes() {
        assert_eq!(
            input_mode_from_compartments(Some(0), Some(HIRAGANA_CONVERSION_MODE)),
            InputMode::Latin
        );
        assert_eq!(
            input_mode_from_compartments(Some(1), Some(HIRAGANA_CONVERSION_MODE)),
            InputMode::Kana
        );
        assert_eq!(
            input_mode_from_compartments(Some(1), Some(0)),
            InputMode::Latin
        );
        assert_eq!(
            input_mode_from_compartments(Some(0), Some(0)),
            InputMode::Latin
        );
        assert_eq!(input_mode_from_compartments(None, None), InputMode::Latin);
    }

    #[test]
    fn outbound_updates_preserve_conversion_when_closing() {
        assert_eq!(
            compartment_updates(InputMode::Latin),
            [CompartmentUpdate::OpenClose(0)]
        );
    }

    #[test]
    fn outbound_updates_configure_hiragana_before_opening() {
        assert_eq!(
            compartment_updates(InputMode::Kana),
            [
                CompartmentUpdate::Conversion(HIRAGANA_CONVERSION_MODE),
                CompartmentUpdate::OpenClose(1),
            ]
        );
    }

    #[test]
    fn event_routing_accepts_only_the_two_input_mode_compartments() {
        assert!(is_input_mode_compartment(
            &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE
        ));
        assert!(is_input_mode_compartment(
            &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION
        ));
        assert!(!is_input_mode_compartment(&GUID::zeroed()));
    }
}
