use macros::anyhow;
use windows::{
    core::{implement, AsImpl, HRESULT, VARIANT},
    Win32::{
        Foundation::RECT,
        UI::TextServices::{
            ITfComposition, ITfCompositionSink, ITfContext, ITfContextComposition, ITfEditSession,
            ITfEditSession_Impl, ITfInsertAtSelection, ITfRange, ITfTextInputProcessor,
            GUID_PROP_ATTRIBUTE, TF_AE_NONE, TF_AE_START, TF_ANCHOR_END, TF_ANCHOR_START,
            TF_DEFAULT_SELECTION, TF_ES_ASYNC, TF_ES_READ, TF_ES_READWRITE, TF_ES_SYNC,
            TF_IAS_QUERYONLY, TF_SELECTION, TF_SELECTIONSTYLE, TF_ST_CORRECTION,
        },
    },
};

use std::{cell::Cell, mem::ManuallyDrop, rc::Rc, time::Instant};

use anyhow::{Context, Result};

use crate::{
    engine::state::IMEState,
    extension::StringExt as _,
    globals::{DllModule, GUID_DISPLAY_ATTRIBUTE},
};

use super::factory::TextServiceFactory;

#[implement(ITfEditSession)]
struct EditSession<'a, T> {
    callback: Rc<dyn Fn(u32) -> anyhow::Result<T>>,
    pub result: Cell<Option<T>>,
    phantom: std::marker::PhantomData<&'a T>,
}

fn take_sync_result<T>(session_result: HRESULT, callback_result: Option<T>) -> Result<T> {
    session_result.ok()?;
    callback_result.context("synchronous TSF edit session completed without running its callback")
}

fn sync_edit_session<T>(
    client_id: u32,
    context: ITfContext,
    read_write: bool,
    callback: Rc<dyn Fn(u32) -> anyhow::Result<T>>,
) -> Result<T> {
    let session: ITfEditSession = EditSession {
        callback,
        result: Cell::new(None),
        phantom: std::marker::PhantomData,
    }
    .into();

    let access = if read_write {
        TF_ES_READWRITE
    } else {
        TF_ES_READ
    };
    let session_result =
        unsafe { context.RequestEditSession(client_id, &session, access | TF_ES_SYNC)? };
    let callback_result = unsafe { session.as_impl() }.result.take();

    take_sync_result(session_result, callback_result)
}

pub(super) fn sync_read_edit_session<T>(
    client_id: u32,
    context: ITfContext,
    callback: Rc<dyn Fn(u32) -> anyhow::Result<T>>,
) -> Result<T> {
    sync_edit_session(client_id, context, false, callback)
}

fn sync_read_write_edit_session<T>(
    client_id: u32,
    context: ITfContext,
    callback: Rc<dyn Fn(u32) -> anyhow::Result<T>>,
) -> Result<T> {
    sync_edit_session(client_id, context, true, callback)
}

fn async_read_edit_session(
    client_id: u32,
    context: ITfContext,
    callback: Rc<dyn Fn(u32) -> anyhow::Result<()>>,
) -> Result<()> {
    let session: ITfEditSession = EditSession {
        callback,
        result: Cell::new(None),
        phantom: std::marker::PhantomData,
    }
    .into();

    // update_pos is also called from ITfTextLayoutSink::OnLayoutChange, where a synchronous
    // read/write request is explicitly forbidden. It does not return data to its caller, so a
    // detached read-only session is the correct contract.
    let session_result =
        unsafe { context.RequestEditSession(client_id, &session, TF_ES_READ | TF_ES_ASYNC)? };
    session_result.ok()?;

    Ok(())
}

fn current_text_ext(
    context: &ITfContext,
    tip_composition: Option<&ITfComposition>,
    cookie: u32,
) -> Result<Option<RECT>> {
    unsafe {
        let range = if let Some(tip_composition) = tip_composition {
            tip_composition.GetRange()?
        } else {
            let mut selections = [TF_SELECTION::default()];
            let mut fetched = 0;
            context.GetSelection(cookie, TF_DEFAULT_SELECTION, &mut selections, &mut fetched)?;
            if fetched == 0 {
                return Ok(None);
            }

            let Some(range) = selections[0].range.as_ref() else {
                return Ok(None);
            };
            let range = range.Clone()?;
            let anchor = if selections[0].style.ase == TF_AE_START {
                TF_ANCHOR_START
            } else {
                TF_ANCHOR_END
            };
            range.Collapse(cookie, anchor)?;
            range
        };

        let view = context.GetActiveView()?;
        let mut rect = RECT::default();
        let mut clipped = false.into();
        view.GetTextExt(cookie, &range, &mut rect, &mut clipped)?;
        Ok(Some(rect))
    }
}

struct DetachedPositionSession;

impl DetachedPositionSession {
    fn new() -> Result<Self> {
        DllModule::get()?.add_ref();
        Ok(Self)
    }
}

impl Drop for DetachedPositionSession {
    fn drop(&mut self) {
        match DllModule::get() {
            Ok(mut dll_instance) => {
                dll_instance.release();
            }
            Err(error) => {
                tracing::warn!("Failed to release DLL reference for indicator position: {error:?}");
            }
        }
    }
}

struct UpdatePosCompletion {
    owner: ITfTextInputProcessor,
    pending: Cell<bool>,
    dll_ref_held: bool,
}

impl UpdatePosCompletion {
    fn new(owner: ITfTextInputProcessor) -> Result<Self> {
        // A queued edit session can outlive TextService::Deactivate. Keep the in-process COM
        // server loaded until TSF either runs or releases this callback object.
        DllModule::get()?.add_ref();
        Ok(Self {
            owner,
            pending: Cell::new(true),
            dll_ref_held: true,
        })
    }

    fn finish(&self) {
        if !self.pending.get() {
            return;
        }

        let factory: &TextServiceFactory = unsafe { self.owner.as_impl() };
        match factory.borrow_mut() {
            Ok(mut text_service) => {
                text_service.update_pos_state.finish_update(Instant::now());
                self.pending.set(false);
            }
            Err(error) => {
                tracing::warn!("Failed to reset asynchronous update_pos guard: {error:?}");
            }
        }
    }
}

impl Drop for UpdatePosCompletion {
    fn drop(&mut self) {
        // TSF normally invokes the callback after accepting an asynchronous request. If the
        // context is instead torn down and releases the session, do not leave position updates
        // permanently suppressed.
        self.finish();

        if self.dll_ref_held {
            match DllModule::get() {
                Ok(mut dll_instance) => {
                    dll_instance.release();
                    self.dll_ref_held = false;
                }
                Err(error) => {
                    tracing::warn!(
                        "Failed to release DLL reference for update_pos session: {error:?}"
                    );
                }
            }
        }
    }
}

impl<'a, T> ITfEditSession_Impl for EditSession_Impl<'a, T> {
    #[anyhow]
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        match (self.callback)(cookie) {
            Ok(result) => {
                self.result.set(Some(result));
                Ok(())
            }
            Err(error) => {
                tracing::warn!("TSF edit-session callback failed: {error:?}");
                Err(error)
            }
        }
    }
}

impl TextServiceFactory {
    #[tracing::instrument]
    pub fn update_indicator_pos(&self) -> Result<()> {
        let (tid, context, tip_composition) = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            (
                text_service.tid,
                text_service.context::<ITfContext>()?,
                composition.tip_composition.clone(),
            )
        };
        let lifetime = Rc::new(DetachedPositionSession::new()?);

        async_read_edit_session(
            tid,
            context.clone(),
            Rc::new(move |cookie| {
                let _keep_dll_loaded = &lifetime;
                let Some(rect) = current_text_ext(&context, tip_composition.as_ref(), cookie)?
                else {
                    return Ok(());
                };
                let Some(mut ipc_service) = IMEState::ipc_snapshot() else {
                    return Ok(());
                };
                ipc_service.set_window_position(rect.top, rect.left, rect.bottom, rect.right)
            }),
        )
    }

    #[tracing::instrument]
    pub fn start_composition(&self) -> Result<()> {
        tracing::debug!("start_composition");

        let (tid, context, context_composition, sink, insert, tip_exists) = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            (
                text_service.tid,
                text_service.context()?,
                text_service.context::<ITfContextComposition>()?,
                text_service.this::<ITfCompositionSink>()?,
                text_service.context::<ITfInsertAtSelection>()?,
                composition.tip_composition.is_some(),
            )
        };

        if tip_exists {
            self.end_composition()?;
            return Ok(());
        }

        let composition = sync_read_write_edit_session::<ITfComposition>(
            tid,
            context,
            Rc::new({
                move |cookie| unsafe {
                    let range = insert.InsertTextAtSelection(cookie, TF_IAS_QUERYONLY, &[])?;
                    let composition =
                        context_composition.StartComposition(cookie, &range, &sink)?;

                    Ok(composition)
                }
            }),
        )?;

        tracing::debug!("Composition started {composition:?}");
        self.borrow()?.borrow_mut_composition()?.tip_composition = Some(composition);

        Ok(())
    }

    #[tracing::instrument]
    pub fn end_composition(&self) -> Result<()> {
        tracing::debug!("end_composition");
        let (tid, context, tip_composition) = {
            let text_service = self.borrow()?;
            let tip_composition = text_service.borrow_composition()?.tip_composition.clone();
            let snapshot = (
                text_service.tid,
                text_service.context::<ITfContext>()?,
                tip_composition,
            );
            snapshot
        };
        if let Some(composition) = tip_composition {
            sync_read_write_edit_session(
                tid,
                context.clone(),
                Rc::new({
                    move |cookie| unsafe {
                        // clear display attribute first
                        let range: ITfRange = composition.GetRange()?;

                        let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                        prop.Clear(cookie, &range)?;

                        // shift the start of the composition
                        range.Collapse(cookie, TF_ANCHOR_END)?;
                        let selection = TF_SELECTION {
                            range: ManuallyDrop::new(Some(range.clone())),
                            style: TF_SELECTIONSTYLE {
                                ase: TF_AE_NONE,
                                fInterimChar: false.into(),
                            },
                        };

                        context.SetSelection(cookie, &[selection])?;

                        composition.EndComposition(cookie)?;
                        Ok(())
                    }
                }),
            )?;

            // Only forget the TSF composition after the synchronous edit session actually
            // succeeded. OnCompositionTerminated commonly cleared this already; this assignment
            // is the idempotent fallback for hosts that do not call the sink.
            self.borrow()?.borrow_mut_composition()?.tip_composition = None;
        } else {
            tracing::warn!("Composition is not started");
        }

        Ok(())
    }

    #[tracing::instrument]
    pub fn set_text(&self, text: &str, subtext: &str) -> Result<()> {
        let (tid, context, display_attribute_atom, tip_composition) = {
            let text_service = self.borrow()?;
            let tip_composition = text_service.borrow_composition()?.tip_composition.clone();
            let snapshot = (
                text_service.tid,
                text_service.context::<ITfContext>()?,
                text_service.display_attribute_atom.clone(),
                tip_composition,
            );
            snapshot
        };
        if let Some(composition) = tip_composition {
            sync_read_write_edit_session(
                tid,
                context.clone(),
                Rc::new({
                    let text_len = i32::try_from(text.encode_utf16().count()).unwrap_or(i32::MAX);

                    // unpadded is all you need!
                    let text = format!("{text}{subtext}").as_str().to_wide_16_unpadded();

                    move |cookie| unsafe {
                        let range = composition.GetRange()?;
                        range.SetText(cookie, TF_ST_CORRECTION, &text)?;

                        // first, set the display attribute to the "text" part
                        let text_range = range.Clone()?;
                        text_range.Collapse(cookie, TF_ANCHOR_START)?;
                        let mut shifted: i32 = 0;
                        text_range.ShiftEnd(cookie, text_len, &mut shifted, std::ptr::null())?;
                        let display_attribute = display_attribute_atom.get(&GUID_DISPLAY_ATTRIBUTE);
                        if let Some(display_attribute) = display_attribute {
                            let pvar = VARIANT::from(*display_attribute as i32);
                            let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                            prop.SetValue(cookie, &text_range, &pvar)?;
                        }

                        range.Collapse(cookie, TF_ANCHOR_END)?;
                        let selection = TF_SELECTION {
                            range: ManuallyDrop::new(Some(range.clone())),
                            style: TF_SELECTIONSTYLE {
                                ase: TF_AE_NONE,
                                fInterimChar: false.into(),
                            },
                        };

                        context.SetSelection(cookie, &[selection])?;

                        Ok(())
                    }
                }),
            )?;
        } else {
            tracing::warn!("Composition is not started");
        }

        Ok(())
    }

    #[tracing::instrument]
    pub fn shift_start(&self, text: &str, subtext: &str) -> Result<()> {
        let (tid, context, display_attribute_atom, tip_composition) = {
            let text_service = self.borrow()?;
            let tip_composition = text_service.borrow_composition()?.tip_composition.clone();
            let snapshot = (
                text_service.tid,
                text_service.context::<ITfContext>()?,
                text_service.display_attribute_atom.clone(),
                tip_composition,
            );
            snapshot
        };
        if let Some(composition) = tip_composition {
            sync_read_write_edit_session(
                tid,
                context.clone(),
                Rc::new({
                    let text_len = i32::try_from(text.encode_utf16().count()).unwrap_or(i32::MAX);
                    let subtext = subtext.to_wide_16_unpadded();

                    move |cookie| unsafe {
                        // first, shift the start of the composition
                        let range = composition.GetRange()?;
                        let mut shifted: i32 = 0;

                        // and clear the display attribute
                        let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                        prop.Clear(cookie, &range)?;

                        range.Collapse(cookie, TF_ANCHOR_START)?;
                        range.ShiftStart(cookie, text_len, &mut shifted, std::ptr::null())?;

                        composition.ShiftStart(cookie, &range)?;

                        // then, set the display attribute
                        let range = composition.GetRange()?;

                        range.SetText(cookie, TF_ST_CORRECTION, &subtext)?;

                        let display_attribute = display_attribute_atom.get(&GUID_DISPLAY_ATTRIBUTE);
                        if let Some(display_attribute) = display_attribute {
                            let pvar = VARIANT::from(*display_attribute as i32);
                            let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                            prop.SetValue(cookie, &range, &pvar)?;
                        }

                        range.Collapse(cookie, TF_ANCHOR_END)?;
                        let selection = TF_SELECTION {
                            range: ManuallyDrop::new(Some(range)),
                            style: TF_SELECTIONSTYLE {
                                ase: TF_AE_NONE,
                                fInterimChar: false.into(),
                            },
                        };

                        context.SetSelection(cookie, &[selection])?;

                        Ok(())
                    }
                }),
            )?;
        } else {
            tracing::warn!("Composition is not started");
        }

        Ok(())
    }

    #[tracing::instrument]
    pub fn update_pos(&self) -> Result<()> {
        {
            let mut text_service = match self.borrow_mut() {
                Ok(text_service) => text_service,
                Err(error) => {
                    tracing::warn!("Skip update_pos due to borrow conflict: {error:?}");
                    return Ok(());
                }
            };

            if !text_service
                .update_pos_state
                .try_begin_update(Instant::now())
            {
                tracing::debug!("Skip re-entrant update_pos call");
                return Ok(());
            }
        }

        let result: Result<()> = (|| {
            let (tid, context, tip_composition, owner) = {
                let text_service = self.borrow()?;
                let composition = text_service.borrow_composition()?;
                (
                    text_service.tid,
                    text_service.context::<ITfContext>()?,
                    composition.tip_composition.clone(),
                    text_service.this::<ITfTextInputProcessor>()?,
                )
            };

            let completion = Rc::new(UpdatePosCompletion::new(owner)?);

            if let Some(tip_composition) = tip_composition {
                async_read_edit_session(
                    tid,
                    context.clone(),
                    Rc::new({
                        let context = context.clone();
                        let completion = Rc::clone(&completion);

                        move |cookie| {
                            let result: Result<()> = (|| {
                                let Some(mut ipc_service) = IMEState::ipc_snapshot() else {
                                    return Ok(());
                                };
                                let Some(rect) =
                                    current_text_ext(&context, Some(&tip_composition), cookie)?
                                else {
                                    return Ok(());
                                };

                                ipc_service.set_window_position(
                                    rect.top,
                                    rect.left,
                                    rect.bottom,
                                    rect.right,
                                )?;

                                Ok(())
                            })();

                            completion.finish();
                            result
                        }
                    }),
                )?;
            }

            Ok(())
        })();

        if let Err(error) = result {
            tracing::warn!("Failed to update composition window position: {error:?}");
            // Errors before the detached session owns UpdatePosCompletion still need to release
            // the guard. finish_update is idempotent if the completion already did so.
            match self.borrow_mut() {
                Ok(mut text_service) => {
                    text_service.update_pos_state.finish_update(Instant::now());
                }
                Err(error) => {
                    tracing::warn!("Failed to reset rejected update_pos guard: {error:?}");
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::take_sync_result;
    use windows::{
        core::HRESULT,
        Win32::{
            Foundation::{E_FAIL, S_OK},
            UI::TextServices::TF_S_ASYNC,
        },
    };

    #[test]
    fn synchronous_session_requires_its_callback_result() {
        assert_eq!(take_sync_result(S_OK, Some(7)).unwrap(), 7);
        assert!(take_sync_result::<()>(S_OK, None).is_err());
        assert!(take_sync_result::<()>(TF_S_ASYNC, None).is_err());
    }

    #[test]
    fn synchronous_session_propagates_inner_hresult_failure() {
        let error = take_sync_result(E_FAIL, Some(())).unwrap_err();
        let hresult = error
            .downcast_ref::<windows::core::Error>()
            .map(windows::core::Error::code);
        assert_eq!(hresult, Some(HRESULT(E_FAIL.0)));
    }
}
