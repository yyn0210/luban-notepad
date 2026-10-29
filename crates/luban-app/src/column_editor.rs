//! Column editor dialog state.

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, Window};
use luban_core::line_ops::ColumnPayload;

use crate::app::LubanApp;
use crate::i18n::tr;

pub struct ColumnEditorState {
    pub open: bool,
    pub numbers: bool,
    pub text: Entity<InputState>,
    pub start: Entity<InputState>,
    pub step: Entity<InputState>,
    pub width: Entity<InputState>,
    pub column: Entity<InputState>,
    pub error: Option<String>,
}

impl ColumnEditorState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let text =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("column.text_placeholder")));
        let start = cx.new(|cx| InputState::new(window, cx).default_value("1"));
        let step = cx.new(|cx| InputState::new(window, cx).default_value("1"));
        let width = cx.new(|cx| InputState::new(window, cx).default_value("0"));
        let column = cx.new(|cx| InputState::new(window, cx).default_value("1"));
        Self {
            open: false,
            numbers: false,
            text,
            start,
            step,
            width,
            column,
            error: None,
        }
    }

    pub fn open_at(&mut self, column: usize, window: &mut Window, cx: &mut Context<LubanApp>) {
        self.open = true;
        self.error = None;
        let value = column.max(1).to_string();
        self.column
            .update(cx, |input, cx| input.set_value(value, window, cx));
    }

    pub fn payload(&self, cx: &gpui_kit::App) -> Result<(usize, ColumnPayload), String> {
        let column = self
            .column
            .read(cx)
            .value()
            .trim()
            .parse::<usize>()
            .map_err(|_| tr("column.invalid_column"))?;
        if column == 0 {
            return Err(tr("column.invalid_column"));
        }
        if self.numbers {
            let start = self
                .start
                .read(cx)
                .value()
                .trim()
                .parse::<i64>()
                .map_err(|_| tr("column.invalid_start"))?;
            let step = self
                .step
                .read(cx)
                .value()
                .trim()
                .parse::<i64>()
                .map_err(|_| tr("column.invalid_step"))?;
            let width = self
                .width
                .read(cx)
                .value()
                .trim()
                .parse::<usize>()
                .map_err(|_| tr("column.invalid_width"))?;
            if width > 32 {
                return Err(tr("column.invalid_width"));
            }
            Ok((column, ColumnPayload::Numbers { start, step, width }))
        } else {
            let text = self.text.read(cx).value().to_string();
            if text.is_empty() {
                return Err(tr("column.empty_text"));
            }
            Ok((column, ColumnPayload::Text(text)))
        }
    }
}
