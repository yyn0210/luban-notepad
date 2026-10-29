//! Macro recording and macro-dialog state.

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Window};

use crate::app::LubanApp;
use crate::i18n::tr;

pub struct MacroUiState {
    pub open: bool,
    pub name_input: Entity<InputState>,
    pub shortcut_input: Entity<InputState>,
    pub selected: Option<String>,
    pub error: Option<String>,
    /// Macro name waiting for a second Save click to overwrite.
    pub overwrite_pending: Option<String>,
}

impl MacroUiState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("macro.name_placeholder")));
        let shortcut_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("macro.shortcut_placeholder")));
        Self {
            open: false,
            name_input,
            shortcut_input,
            selected: None,
            error: None,
            overwrite_pending: None,
        }
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<LubanApp>) {
        self.open = true;
        self.selected = None;
        self.error = None;
        self.overwrite_pending = None;
        self.name_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.shortcut_input
            .update(cx, |input, cx| input.set_value("", window, cx));
    }
}
