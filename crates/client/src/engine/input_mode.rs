use crate::tsf::factory::TextServiceFactory;

use windows::{
    core::Interface,
    Win32::UI::TextServices::{ITfLangBarItemButton, ITfLangBarItemMgr},
};

use anyhow::Result;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputMode {
    #[default]
    Latin,
    Kana,
}

impl InputMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::Latin => Self::Kana,
            Self::Kana => Self::Latin,
        }
    }

    pub fn indicator(self) -> &'static str {
        match self {
            Self::Latin => "A",
            Self::Kana => "あ",
        }
    }
}

impl TextServiceFactory {
    pub fn update_lang_bar(&self) -> Result<()> {
        // change the icon of the language bar item
        // Clone the COM interfaces before calling back into TSF. RemoveItem/AddItem can
        // synchronously call GetIcon/AdviseSink on this object, so keeping a RefCell borrow
        // alive across those calls would make a re-entrant callback fail.
        let (thread_mgr, lang_bar_item, lang_bar_added) = {
            let text_service = self.borrow()?;
            (
                text_service.thread_mgr()?,
                text_service.this::<ITfLangBarItemButton>()?,
                text_service.lang_bar_added,
            )
        };
        if !lang_bar_added {
            return Ok(());
        }
        let lang_bar_item_mgr = thread_mgr.cast::<ITfLangBarItemMgr>()?;

        unsafe {
            lang_bar_item_mgr.RemoveItem(&lang_bar_item)?;
        };
        self.borrow_mut()?.lang_bar_added = false;

        unsafe { lang_bar_item_mgr.AddItem(&lang_bar_item)? };
        self.borrow_mut()?.lang_bar_added = true;

        Ok(())
    }
}
