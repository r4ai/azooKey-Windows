use windows::{
    core::Interface as _,
    Win32::UI::TextServices::{
        ITfContext, ITfContextView, ITfDocumentMgr, ITfSource, ITfTextLayoutSink,
        ITfTextLayoutSink_Impl, TfLayoutCode,
    },
};

use anyhow::Result;

use super::factory::{TextServiceFactory, TextServiceFactory_Impl};

impl ITfTextLayoutSink_Impl for TextServiceFactory_Impl {
    // This function is called when the text display position changes when the IME is enabled.
    // However, this function **will not be called** in Microsoft Store applications such as Notepad, so be careful.
    #[macros::anyhow]
    fn OnLayoutChange(
        &self,
        _pic: Option<&ITfContext>,
        _lcode: TfLayoutCode,
        _pview: Option<&ITfContextView>,
    ) -> Result<()> {
        let should_skip = match self.borrow_mut() {
            Ok(mut text_service) => text_service
                .update_pos_state
                .should_skip_layout_change(std::time::Instant::now()),
            Err(error) => {
                tracing::warn!("Skip OnLayoutChange due to borrow conflict: {error:?}");
                true
            }
        };

        if should_skip {
            tracing::debug!("Skip layout-triggered update_pos to avoid feedback loop");
            return Ok(());
        }

        if let Err(error) = self.update_pos() {
            tracing::warn!("Failed to update position from OnLayoutChange: {error:?}");
        }

        Ok(())
    }
}

impl TextServiceFactory {
    pub fn advise_text_layout_sink(&self, doc_mgr: ITfDocumentMgr) -> Result<()> {
        let has_existing_sink = {
            let text_service = self.borrow()?;
            text_service.text_layout_context.is_some() || text_service.text_layout_cookie.is_some()
        };
        if has_existing_sink {
            self.unadvise_text_layout_sink()?;
        }

        let context = unsafe { doc_mgr.GetTop()? };
        let sink = self.borrow()?.this::<ITfTextLayoutSink>()?;
        let source = context.cast::<ITfSource>()?;
        let cookie = unsafe { source.AdviseSink(&ITfTextLayoutSink::IID, &sink)? };

        let store_result = (|| -> Result<()> {
            let mut text_service = self.borrow_mut()?;
            text_service.text_layout_context = Some(context.clone());
            text_service.text_layout_cookie = Some(cookie);
            Ok(())
        })();

        if let Err(error) = store_result {
            if let Err(rollback_error) = unsafe { source.UnadviseSink(cookie) } {
                tracing::warn!(
                    "Failed to roll back text-layout sink registration: {rollback_error:?}"
                );
                if let Ok(mut text_service) = self.borrow_mut() {
                    text_service.text_layout_context = Some(context);
                    text_service.text_layout_cookie = Some(cookie);
                }
            }
            return Err(error);
        }

        Ok(())
    }

    pub fn unadvise_text_layout_sink(&self) -> Result<()> {
        let (context, cookie) = {
            let text_service = self.borrow()?;
            (
                text_service.text_layout_context.clone(),
                text_service.text_layout_cookie,
            )
        };

        let (context, cookie) = match (context, cookie) {
            (None, None) => return Ok(()),
            (Some(_), None) => {
                self.borrow_mut()?.text_layout_context = None;
                return Ok(());
            }
            (None, Some(_)) => anyhow::bail!("Text-layout sink cookie has no context"),
            (Some(context), Some(cookie)) => (context, cookie),
        };
        unsafe { context.cast::<ITfSource>()?.UnadviseSink(cookie)? };

        // Preserve the context/cookie until UnadviseSink succeeds so Deactivate can retry a
        // failed teardown with the original source.
        {
            let mut text_service = self.borrow_mut()?;
            if text_service.text_layout_cookie == Some(cookie) {
                text_service.text_layout_cookie = None;
                text_service.text_layout_context = None;
            }
        }

        Ok(())
    }
}
