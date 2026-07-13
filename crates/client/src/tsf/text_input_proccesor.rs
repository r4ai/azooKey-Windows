use std::collections::HashMap;

use crate::{
    engine::{ipc_service, state::IMEState},
    globals::{DllModule, GUID_DISPLAY_ATTRIBUTE},
};

use super::factory::{TextServiceFactory, TextServiceFactory_Impl};
use windows::{
    core::Interface as _,
    Win32::{
        Foundation::BOOL,
        System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
        UI::TextServices::{
            CLSID_TF_CategoryMgr, ITfCategoryMgr, ITfKeyEventSink, ITfKeystrokeMgr,
            ITfLangBarItemButton, ITfLangBarItemMgr, ITfSource, ITfTextInputProcessorEx_Impl,
            ITfTextInputProcessor_Impl, ITfThreadMgr, ITfThreadMgrEventSink,
        },
    },
};

use anyhow::{Context, Result};

fn keep_first_error(first_error: &mut Option<anyhow::Error>, operation: &str, result: Result<()>) {
    if let Err(error) = result {
        let error = error.context(operation.to_owned());
        tracing::warn!("{operation} failed: {error:?}");
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

impl TextServiceFactory {
    fn rollback_pre_key_activation(&self, release_dll_ref: bool) {
        let clear_state = || -> Result<()> {
            let mut text_service = self.borrow_mut()?;
            text_service.tid = 0;
            text_service.thread_mgr = None;
            text_service.dll_ref_held = false;
            text_service.key_event_sink_advised = false;
            Ok(())
        };

        if !release_dll_ref {
            if let Err(error) = clear_state() {
                tracing::error!("Failed to clear TextService after Activate error: {error:?}");
            }
            return;
        }

        // Obtain the DLL state first, clear the marker, then release while the guard is still
        // held. If clearing fails we intentionally retain both the reference and marker, so a
        // later cleanup can retry without double-release.
        match DllModule::get() {
            Ok(mut dll_instance) => match clear_state() {
                Ok(()) => {
                    dll_instance.release();
                }
                Err(error) => {
                    tracing::error!("Failed to clear TextService before DLL rollback: {error:?}")
                }
            },
            Err(error) => tracing::error!(
                "Failed to access DLL state after Activate error; retaining marker: {error:?}"
            ),
        }
    }

    fn advise_thread_mgr_event_sink(&self) -> Result<()> {
        let (thread_mgr, sink, already_advised) = {
            let text_service = self.borrow()?;
            (
                text_service.thread_mgr()?,
                text_service.this::<ITfThreadMgrEventSink>()?,
                text_service.thread_mgr_event_cookie.is_some(),
            )
        };
        if already_advised {
            return Ok(());
        }

        let source = thread_mgr.cast::<ITfSource>()?;
        let cookie = unsafe { source.AdviseSink(&ITfThreadMgrEventSink::IID, &sink)? };
        if let Err(error) = (|| -> Result<()> {
            self.borrow_mut()?.thread_mgr_event_cookie = Some(cookie);
            Ok(())
        })() {
            if let Err(rollback_error) = unsafe { source.UnadviseSink(cookie) } {
                tracing::warn!("Failed to roll back thread-manager event sink: {rollback_error:?}");
                if let Ok(mut text_service) = self.borrow_mut() {
                    text_service.thread_mgr_event_cookie = Some(cookie);
                }
            }
            return Err(error);
        }

        Ok(())
    }

    fn unadvise_thread_mgr_event_sink(&self) -> Result<()> {
        let cookie = self.borrow()?.thread_mgr_event_cookie;
        let Some(cookie) = cookie else {
            return Ok(());
        };
        let thread_mgr = {
            let text_service = self.borrow()?;
            text_service.thread_mgr()?
        };

        unsafe {
            thread_mgr.cast::<ITfSource>()?.UnadviseSink(cookie)?;
        }
        let mut text_service = self.borrow_mut()?;
        if text_service.thread_mgr_event_cookie == Some(cookie) {
            text_service.thread_mgr_event_cookie = None;
        }
        Ok(())
    }

    fn unadvise_key_event_sink(&self) -> Result<()> {
        if !self.borrow()?.key_event_sink_advised {
            return Ok(());
        }
        let (thread_mgr, tid) = {
            let text_service = self.borrow()?;
            (text_service.thread_mgr()?, text_service.tid)
        };

        unsafe {
            thread_mgr
                .cast::<ITfKeystrokeMgr>()?
                .UnadviseKeyEventSink(tid)?;
        }
        self.borrow_mut()?.key_event_sink_advised = false;
        Ok(())
    }

    fn add_lang_bar_item(&self) -> Result<()> {
        let (thread_mgr, lang_bar_item, already_added) = {
            let text_service = self.borrow()?;
            (
                text_service.thread_mgr()?,
                text_service.this::<ITfLangBarItemButton>()?,
                text_service.lang_bar_added,
            )
        };
        if already_added {
            return Ok(());
        }

        let lang_bar_mgr = thread_mgr.cast::<ITfLangBarItemMgr>()?;
        unsafe { lang_bar_mgr.AddItem(&lang_bar_item)? };
        if let Err(error) = (|| -> Result<()> {
            self.borrow_mut()?.lang_bar_added = true;
            Ok(())
        })() {
            if let Err(rollback_error) = unsafe { lang_bar_mgr.RemoveItem(&lang_bar_item) } {
                tracing::warn!("Failed to roll back language-bar item: {rollback_error:?}");
                if let Ok(mut text_service) = self.borrow_mut() {
                    text_service.lang_bar_added = true;
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn remove_lang_bar_item(&self) -> Result<()> {
        if !self.borrow()?.lang_bar_added {
            return Ok(());
        }
        let (thread_mgr, lang_bar_item) = {
            let text_service = self.borrow()?;
            (
                text_service.thread_mgr()?,
                text_service.this::<ITfLangBarItemButton>()?,
            )
        };

        unsafe {
            thread_mgr
                .cast::<ITfLangBarItemMgr>()?
                .RemoveItem(&lang_bar_item)?;
        }
        self.borrow_mut()?.lang_bar_added = false;
        Ok(())
    }

    fn has_live_tsf_resources(&self) -> Result<bool> {
        let text_service = self.borrow()?;
        let has_active_composition = text_service.borrow_composition()?.tip_composition.is_some();
        Ok(has_active_composition
            || text_service.key_event_sink_advised
            || text_service.thread_mgr_event_cookie.is_some()
            || text_service.text_layout_cookie.is_some()
            || text_service.lang_bar_added
            || text_service.open_close_cookie.is_some()
            || text_service.conversion_mode_cookie.is_some())
    }

    fn release_dll_ref_if_unused(&self) -> Result<()> {
        if self.has_live_tsf_resources()? {
            return Ok(());
        }

        let dll_ref_held = self.borrow()?.dll_ref_held;
        if dll_ref_held {
            let mut dll_instance = DllModule::get()?;
            self.borrow_mut()?.dll_ref_held = false;
            dll_instance.release();
        }
        Ok(())
    }

    fn clear_deactivated_state_if_unused(&self) -> Result<()> {
        if self.has_live_tsf_resources()? || self.borrow()?.dll_ref_held {
            return Ok(());
        }

        if let Some(mut ipc_service) = IMEState::ipc_snapshot() {
            // Some hosts do not send OnCompositionTerminated after a successful EndComposition.
            // Do not perform conversion IPC during Deactivate; make the next focused stateful
            // request lazily reset the server session, and hide the optional UI best-effort.
            ipc_service.mark_server_session_dirty();
            if let Err(error) = ipc_service.hide_window() {
                tracing::warn!("Failed to queue candidate-window hide during teardown: {error:?}");
            }
        }

        let mut text_service = self.borrow_mut()?;
        // EndComposition is allowed to succeed without a synchronous termination callback.
        // Once all TSF registrations and the DLL reference are gone, no live range remains;
        // clear every local preedit field so a later Activate cannot inherit stale state. A late
        // callback performs the same reset again and merely advances the generation once more.
        text_service.borrow_mut_composition()?.reset();
        text_service.display_attribute_atom.clear();
        text_service.context = None;
        text_service.thread_mgr_event_cookie = None;
        text_service.text_layout_context = None;
        text_service.text_layout_cookie = None;
        text_service.key_event_sink_advised = false;
        text_service.lang_bar_added = false;
        text_service.open_close_cookie = None;
        text_service.conversion_mode_cookie = None;
        text_service.pending_input_mode_transition.set(None);
        text_service.pending_composition_cleanup.set(false);
        text_service.compartment_write_in_progress = false;
        text_service.tid = 0;
        text_service.thread_mgr = None;
        Ok(())
    }
}

impl ITfTextInputProcessor_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    #[tracing::instrument]
    fn Activate(&self, ptim: Option<&ITfThreadMgr>, tid: u32) -> Result<()> {
        tracing::debug!("Activated with tid: {tid}");

        // IPC startup is best-effort. The launcher, server, and host process can race during
        // login/install, but that must not leave a selected TIP without its key/TSF sinks.
        let ipc_is_ready = IMEState::ipc_snapshot().is_some();
        if !ipc_is_ready {
            // A successful transport connection is enough to publish the client. Do not probe
            // with AppendText/ClearText here: the converter state is process-global and belongs
            // to whichever focused TIP issues the next stateful request.
            let ipc_result = ipc_service::IPCService::new();

            match ipc_result {
                Ok(ipc_service) => match IMEState::install_ipc_if_absent(ipc_service) {
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!("Failed to store initialized IPC service: {error:?}");
                        if let Err(error) = IMEState::start_ipc_reconnect() {
                            tracing::warn!("Failed to schedule IPC reconnect: {error:?}");
                        }
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        "IPC services are not ready during Activate; continuing TSF setup: {error:?}"
                    );
                    if let Err(error) = IMEState::start_ipc_reconnect() {
                        tracing::warn!("Failed to schedule IPC reconnect: {error:?}");
                    }
                }
            }
        }

        let thread_mgr = ptim.context("Thread manager is null")?.clone();
        let keystroke_mgr = thread_mgr.cast::<ITfKeystrokeMgr>()?;
        let key_event_sink = self.borrow()?.this::<ITfKeyEventSink>()?;
        {
            let mut text_service = self.borrow_mut()?;
            if text_service.dll_ref_held
                || text_service.key_event_sink_advised
                || text_service.thread_mgr_event_cookie.is_some()
                || text_service.text_layout_cookie.is_some()
                || text_service.lang_bar_added
                || text_service.open_close_cookie.is_some()
                || text_service.conversion_mode_cookie.is_some()
            {
                anyhow::bail!("TextService is already active or still has registered sinks");
            }
            text_service.tid = tid;
            text_service.thread_mgr = Some(thread_mgr.clone());
            // Mark these optimistically before their external calls. No RefCell borrow is held
            // across COM, and the pre-key rollback clears both markers on failure.
            text_service.dll_ref_held = true;
            text_service.key_event_sink_advised = true;
        }

        let dll_ref_result = (|| -> Result<()> {
            DllModule::get()?.add_ref();
            Ok(())
        })();
        if let Err(error) = dll_ref_result {
            self.rollback_pre_key_activation(false);
            return Err(error);
        }

        // KeyEventSink is the activation boundary: any failure through this point is rolled back
        // and returned; every later optional subsystem is initialized independently.
        tracing::debug!("AdviseKeyEventSink");
        if let Err(error) =
            unsafe { keystroke_mgr.AdviseKeyEventSink(tid, &key_event_sink, BOOL::from(true)) }
        {
            self.rollback_pre_key_activation(true);
            return Err(error.into());
        }

        // initialize thread manager event sink
        tracing::debug!("AdviseThreadMgrEventSink");
        if let Err(error) = self.advise_thread_mgr_event_sink() {
            tracing::warn!("Failed to initialize thread-manager event sink: {error:?}");
        }

        // initialize text layout sink
        tracing::debug!("AdviseTextLayoutSink");
        match unsafe { thread_mgr.GetFocus() } {
            Ok(doc_mgr) => {
                if let Err(error) = self.advise_text_layout_sink(doc_mgr) {
                    tracing::warn!("Failed to initialize text-layout sink: {error:?}");
                }
            }
            Err(error) => {
                tracing::debug!("No focused document for initial text-layout sink: {error:?}");
            }
        }

        // initialize display attribute
        tracing::debug!("Initialize display attribute");
        let display_attribute_result: Result<HashMap<_, _>> = (|| unsafe {
            let mut map = HashMap::new();
            let category_mgr: ITfCategoryMgr =
                CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?;

            let atom = category_mgr.RegisterGUID(&GUID_DISPLAY_ATTRIBUTE)?;
            map.insert(GUID_DISPLAY_ATTRIBUTE, atom);
            Ok(map)
        })();
        match display_attribute_result {
            Ok(atom_map) => match self.borrow_mut() {
                Ok(mut text_service) => text_service.display_attribute_atom = atom_map,
                Err(error) => {
                    tracing::warn!("Failed to store display attribute: {error:?}");
                }
            },
            Err(error) => tracing::warn!("Failed to initialize display attribute: {error:?}"),
        }

        // initialize langbar
        tracing::debug!("Initialize langbar");
        if let Err(error) = self.add_lang_bar_item() {
            tracing::warn!("Failed to initialize language-bar item: {error:?}");
        }

        // Initial synchronization may refresh the language bar, so it must run only after the
        // first AddItem above. The compartment sink remains the authority for this TSF instance.
        tracing::debug!("Initialize input-mode compartments");
        if let Err(error) = self.activate_input_mode_compartments() {
            // Do not fail Activate after key/language-bar sinks have already been installed.
            // The internal mode and explicit keys remain usable, and a later activation can
            // retry compartment setup without leaving TSF in a half-activated state.
            tracing::warn!("Failed to initialize input-mode compartments: {error:?}");
        }

        tracing::debug!("Activate success");

        Ok(())
    }

    #[macros::anyhow]
    #[tracing::instrument]
    fn Deactivate(&self) -> Result<()> {
        tracing::debug!("Deactivated");
        let mut first_error = None;

        // Every teardown is attempted even after an earlier failure. Each helper clears its
        // registration marker only after the corresponding external operation succeeds.
        keep_first_error(&mut first_error, "EndComposition", self.end_composition());
        keep_first_error(
            &mut first_error,
            "Unadvise input-mode compartments",
            self.unadvise_input_mode_compartments(),
        );
        keep_first_error(
            &mut first_error,
            "Unadvise text-layout sink",
            self.unadvise_text_layout_sink(),
        );
        keep_first_error(
            &mut first_error,
            "Unadvise thread-manager event sink",
            self.unadvise_thread_mgr_event_sink(),
        );
        keep_first_error(
            &mut first_error,
            "Unadvise key-event sink",
            self.unadvise_key_event_sink(),
        );
        keep_first_error(
            &mut first_error,
            "Remove language-bar item",
            self.remove_lang_bar_item(),
        );
        keep_first_error(
            &mut first_error,
            "Clear display attributes",
            (|| -> Result<()> {
                self.borrow_mut()?.display_attribute_atom.clear();
                Ok(())
            })(),
        );
        keep_first_error(
            &mut first_error,
            "Release DLL reference",
            self.release_dll_ref_if_unused(),
        );
        keep_first_error(
            &mut first_error,
            "Clear deactivated TextService state",
            self.clear_deactivated_state_if_unused(),
        );

        if let Some(error) = first_error {
            tracing::warn!(
                "Deactivate completed with retryable TSF registrations retained: {error:?}"
            );
            Err(error)
        } else {
            tracing::debug!("Deactivate success");
            Ok(())
        }
    }
}

impl ITfTextInputProcessorEx_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn ActivateEx(&self, ptim: Option<&ITfThreadMgr>, tid: u32, _dwflags: u32) -> Result<()> {
        // called when the text service is activated
        // if this function is implemented, the Activate() function won't be called
        // so we need to call the Activate function manually
        tracing::debug!("Activated(Ex) with tid: {tid}");
        self.Activate(ptim, tid)?;
        Ok(())
    }
}
