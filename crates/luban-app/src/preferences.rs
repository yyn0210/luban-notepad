use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Window};

use crate::app::LubanApp;
use crate::i18n::tr;

pub struct PreferencesState {
    pub open: bool,
    pub font_input: Entity<InputState>,
}

impl PreferencesState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let font_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("prefs.font_placeholder")));
        Self {
            open: false,
            font_input,
        }
    }
}
