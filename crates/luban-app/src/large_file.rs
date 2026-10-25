//! The large-file controller panel.
//!
//! The panel owns the range fields and slider state. Text input and slider
//! events are recorded as pending changes and applied at the start of the next
//! render, where a `Window` is available to write values back into the inputs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderState, SliderValue};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::*;
use luban_core::large_file::{
    format_bytes, format_number, parse_byte_count, validate_range_inputs, RangeInputError,
    RangeMode, RangeSelection, SizeUnit,
};

use crate::app::LubanApp;
use crate::i18n::tr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeField {
    From,
    To,
    Size,
}

/// The sliders use a normalized `0..=1` track, so the default step of 1.0
/// would collapse every drag to the two endpoints.
const SLIDER_STEP: f32 = 0.001;

fn new_slider_state() -> SliderState {
    SliderState::new()
        .min(0.0)
        .max(1.0)
        .step(SLIDER_STEP)
        .default_value(0.0)
}

/// Map a slider position to the byte value of `field`.
fn slider_value(field: RangeField, ratio: f32, selection: &RangeSelection) -> u64 {
    let total = selection.total();
    let from = selection.from();
    let ratio = f64::from(ratio.clamp(0.0, 1.0));
    match field {
        RangeField::From => {
            let span = total.saturating_sub(1);
            (ratio * span as f64).round() as u64
        }
        RangeField::To => {
            let start = (from + 1).min(total);
            let span = total.saturating_sub(start);
            start + (ratio * span as f64).round() as u64
        }
        RangeField::Size => {
            let span = total.saturating_sub(from);
            1 + (ratio * span.saturating_sub(1) as f64).round() as u64
        }
    }
}

/// The slider position that represents `field` in `selection`.
fn slider_ratio(field: RangeField, selection: &RangeSelection) -> f32 {
    let total = selection.total();
    let from = selection.from();
    let to = selection.to();
    let size = selection.size();
    let ratio = match field {
        RangeField::From => {
            if total > 1 {
                from as f32 / (total - 1) as f32
            } else {
                0.0
            }
        }
        RangeField::To => {
            let start = (from + 1).min(total);
            let span = total.saturating_sub(start);
            if span > 0 {
                to.saturating_sub(start) as f32 / span as f32
            } else {
                0.0
            }
        }
        RangeField::Size => {
            let span = total.saturating_sub(from);
            if span > 1 {
                size.saturating_sub(1) as f32 / (span - 1) as f32
            } else {
                0.0
            }
        }
    };
    ratio.clamp(0.0, 1.0)
}

/// A file the controller can load.
#[derive(Clone, Debug)]
pub struct LargeFileTarget {
    pub path: PathBuf,
    pub size: u64,
    pub tab_id: Option<u64>,
}

/// Parameters for pointing the panel at a file.
pub struct PanelBinding {
    pub path: PathBuf,
    pub size: u64,
    pub tab_id: Option<u64>,
    pub default_size: u64,
    pub unit: SizeUnit,
    pub loaded: u64,
    pub whole: bool,
}

pub struct LargeFilePanel {
    pub open: bool,
    pub target: Option<LargeFileTarget>,
    pub selection: RangeSelection,
    pub unit: SizeUnit,
    pub loaded: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub status: Option<String>,
    pub progress: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
    pub available_space: Option<u64>,
    pub bound_tab: Option<u64>,
    /// Keep the stopped target even when no tab exists for it yet.
    keep_target: bool,
    pinned_active_tab: Option<u64>,
    pub from_input: Entity<InputState>,
    pub to_input: Entity<InputState>,
    pub size_input: Entity<InputState>,
    pub from_slider: Entity<SliderState>,
    pub to_slider: Entity<SliderState>,
    pub size_slider: Entity<SliderState>,
    pending_input: Option<(RangeField, bool)>,
    pending_slider: Option<RangeField>,
}

impl LargeFilePanel {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>) -> Self {
        let from_input = cx.new(|cx| InputState::new(window, cx).placeholder("0"));
        let to_input = cx.new(|cx| InputState::new(window, cx).placeholder("0"));
        let size_input = cx.new(|cx| InputState::new(window, cx).placeholder("0"));
        let slider = |cx: &mut Context<LubanApp>| cx.new(|_| new_slider_state());
        Self {
            open: false,
            target: None,
            selection: RangeSelection::new(0, 1),
            unit: SizeUnit::Bytes,
            loaded: 0,
            loading: false,
            error: None,
            status: None,
            progress: Arc::new(AtomicU64::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            available_space: None,
            bound_tab: None,
            keep_target: false,
            pinned_active_tab: None,
            from_input,
            to_input,
            size_input,
            from_slider: slider(cx),
            to_slider: slider(cx),
            size_slider: slider(cx),
            pending_input: None,
            pending_slider: None,
        }
    }

    /// Show the panel for a file that is about to be loaded.
    pub fn open_for(
        &mut self,
        binding: PanelBinding,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) {
        self.open = true;
        self.keep_target = false;
        self.pinned_active_tab = None;
        self.target = Some(LargeFileTarget {
            path: binding.path,
            size: binding.size,
            tab_id: binding.tab_id,
        });
        self.selection = RangeSelection::new(binding.size, binding.default_size);
        self.selection.set_mode(if binding.whole {
            RangeMode::Whole
        } else {
            RangeMode::Beginning
        });
        self.unit = binding.unit;
        self.loaded = 0;
        self.loading = false;
        self.error = None;
        self.status = None;
        self.bound_tab = binding.tab_id;
        self.available_space = available_space(&std::env::temp_dir());
        self.sync_all(window, cx);
    }

    /// Bind the panel to an already open document.
    pub fn bind(&mut self, binding: PanelBinding, window: &mut Window, cx: &mut Context<LubanApp>) {
        let same = self
            .target
            .as_ref()
            .is_some_and(|target| target.path == binding.path && target.size == binding.size);
        if same {
            self.bound_tab = binding.tab_id;
            self.loaded = binding.loaded;
            return;
        }
        self.target = Some(LargeFileTarget {
            path: binding.path,
            size: binding.size,
            tab_id: binding.tab_id,
        });
        self.selection = RangeSelection::new(binding.size, binding.default_size);
        self.selection.set_mode(if binding.whole {
            RangeMode::Whole
        } else {
            RangeMode::Beginning
        });
        self.unit = binding.unit;
        self.loaded = binding.loaded;
        self.loading = false;
        self.error = None;
        self.status = None;
        self.bound_tab = binding.tab_id;
        self.available_space = available_space(&std::env::temp_dir());
        self.sync_all(window, cx);
    }

    pub fn clear_target(
        &mut self,
        tab_id: Option<u64>,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) {
        self.target = None;
        self.keep_target = false;
        self.pinned_active_tab = None;
        self.selection = RangeSelection::new(0, 1);
        self.loaded = 0;
        self.loading = false;
        self.error = None;
        self.status = None;
        self.bound_tab = tab_id;
        self.sync_all(window, cx);
    }

    pub fn set_mode(&mut self, mode: RangeMode, window: &mut Window, cx: &mut Context<LubanApp>) {
        self.selection.set_mode(mode);
        self.error = None;
        self.status = None;
        self.sync_all(window, cx);
    }

    /// Replace the selection after the source file's size changed.
    pub fn set_selection(
        &mut self,
        selection: RangeSelection,
        size: u64,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) {
        self.selection = selection;
        if let Some(target) = &mut self.target {
            target.size = size;
        }
        self.error = None;
        self.sync_all(window, cx);
    }

    pub fn begin_loading(
        &mut self,
        progress: Arc<AtomicU64>,
        cancel: Arc<AtomicBool>,
        cx: &mut Context<LubanApp>,
    ) {
        self.progress = progress;
        self.cancel = cancel;
        self.loading = true;
        self.error = None;
        self.status = None;
        self.progress.store(0, Ordering::Relaxed);
        cx.notify();
    }

    pub fn finish_loading(&mut self, loaded: u64) {
        self.loading = false;
        self.loaded = loaded;
        self.status = None;
    }

    pub fn stop(&mut self) -> bool {
        if !self.loading {
            return false;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.status = Some(tr("lfc.stopped"));
        true
    }

    pub fn displayed_loaded(&self) -> u64 {
        if self.loading {
            self.progress.load(Ordering::Relaxed)
        } else {
            self.loaded
        }
    }

    pub fn queue_input(&mut self, field: RangeField, normalize: bool) {
        self.pending_input = Some((field, normalize));
    }

    pub fn queue_slider(&mut self, field: RangeField) {
        self.pending_slider = Some(field);
    }

    /// Apply queued input/slider edits. Must run with a window available.
    pub fn apply_pending(&mut self, window: &mut Window, cx: &mut Context<LubanApp>) {
        if let Some(field) = self.pending_slider.take() {
            self.apply_slider(field, window, cx);
        }
        if let Some((field, normalize)) = self.pending_input.take() {
            self.apply_input(field, normalize, window, cx);
        }
    }

    fn apply_slider(&mut self, field: RangeField, window: &mut Window, cx: &mut Context<LubanApp>) {
        let total = self.selection.total();
        if total == 0 {
            return;
        }
        let ratio = match field {
            RangeField::From => self.from_slider.read(cx).value().end(),
            RangeField::To => self.to_slider.read(cx).value().end(),
            RangeField::Size => self.size_slider.read(cx).value().end(),
        };
        let value = slider_value(field, ratio, &self.selection);
        let _ = match field {
            RangeField::From => self.selection.set_from(value),
            RangeField::To => self.selection.set_to(value),
            RangeField::Size => self.selection.set_size(value),
        };
        self.error = None;
        self.status = None;
        self.sync_inputs_except(None, window, cx);
        self.sync_sliders_except(Some(field), window, cx);
    }

    fn apply_input(
        &mut self,
        field: RangeField,
        normalize: bool,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) {
        let text = match field {
            RangeField::From => self.from_input.read(cx).value().to_string(),
            RangeField::To => self.to_input.read(cx).value().to_string(),
            RangeField::Size => self.size_input.read(cx).value().to_string(),
        };
        let mut invalid = |panel: &mut Self, message: String, normalize: bool| {
            panel.error = Some(message);
            if normalize {
                panel.sync_inputs_except(None, window, cx);
            }
        };
        let Some(value) = parse_byte_count(&text) else {
            invalid(self, tr("lfc.error.invalid_number"), normalize);
            return;
        };
        let check = match field {
            RangeField::From => {
                validate_range_inputs(value, self.selection.to(), self.selection.size())
            }
            RangeField::To => {
                validate_range_inputs(self.selection.from(), value, self.selection.size())
            }
            RangeField::Size => {
                validate_range_inputs(self.selection.from(), self.selection.to(), value)
            }
        };
        if let Err(error) = check {
            invalid(self, input_error_message(error), normalize);
            return;
        }
        let result = match field {
            RangeField::From => self.selection.set_from(value),
            RangeField::To => self.selection.set_to(value),
            RangeField::Size => self.selection.set_size(value),
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.status = None;
                self.sync_inputs_except(Some(field), window, cx);
                self.sync_sliders_except(None, window, cx);
            }
            Err(err) => {
                invalid(self, err.to_string(), normalize);
            }
        }
    }

    /// Read and validate the range currently shown in the fields. `None`
    /// means the whole file. This catches a valid edit that has not been
    /// committed with Enter or blur, so Apply never uses a stale selection.
    pub fn current_range(
        &self,
        cx: &gpui_kit::App,
    ) -> std::result::Result<Option<(u64, u64)>, String> {
        if self.selection.mode() == RangeMode::Whole {
            return Ok(None);
        }
        let parse =
            |text: String| parse_byte_count(&text).ok_or_else(|| tr("lfc.error.invalid_number"));
        let from = parse(self.from_input.read(cx).value().to_string())?;
        let to = parse(self.to_input.read(cx).value().to_string())?;
        let size = parse(self.size_input.read(cx).value().to_string())?;
        validate_range_inputs(from, to, size).map_err(input_error_message)?;
        Ok(Some((from, to)))
    }

    fn sync_all(&mut self, window: &mut Window, cx: &mut Context<LubanApp>) {
        self.sync_inputs_except(None, window, cx);
        self.sync_sliders_except(None, window, cx);
    }

    fn sync_inputs_except(
        &mut self,
        except: Option<RangeField>,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) {
        let total = self.selection.total();
        let values = [
            (RangeField::From, format_number(self.selection.from())),
            (RangeField::To, format_number(self.selection.to())),
            (RangeField::Size, format_number(self.selection.size())),
        ];
        for (field, value) in values {
            if Some(field) == except {
                continue;
            }
            let input = match field {
                RangeField::From => self.from_input.clone(),
                RangeField::To => self.to_input.clone(),
                RangeField::Size => self.size_input.clone(),
            };
            if total == 0 && field != RangeField::From {
                input.update(cx, |state, cx| state.set_value("0", window, cx));
            } else {
                input.update(cx, |state, cx| state.set_value(value, window, cx));
            }
        }
    }

    fn sync_sliders_except(
        &mut self,
        except: Option<RangeField>,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) {
        let ratios = [
            (
                RangeField::From,
                slider_ratio(RangeField::From, &self.selection),
            ),
            (
                RangeField::To,
                slider_ratio(RangeField::To, &self.selection),
            ),
            (
                RangeField::Size,
                slider_ratio(RangeField::Size, &self.selection),
            ),
        ];
        for (field, ratio) in ratios {
            if Some(field) == except {
                continue;
            }
            let slider = match field {
                RangeField::From => self.from_slider.clone(),
                RangeField::To => self.to_slider.clone(),
                RangeField::Size => self.size_slider.clone(),
            };
            slider.update(cx, |state, cx| {
                state.set_value(SliderValue::Single(ratio), window, cx)
            });
        }
    }

    /// Keep the panel target after a stopped load that has no tab yet.
    pub fn pin_to_tab(&mut self, tab_id: Option<u64>) {
        self.keep_target = true;
        self.pinned_active_tab = tab_id;
    }

    pub fn should_hold_target(&self, active_tab: Option<u64>) -> bool {
        self.keep_target && self.pinned_active_tab == active_tab
    }

    pub fn clear_pin(&mut self) {
        self.keep_target = false;
        self.pinned_active_tab = None;
    }

    pub fn render(
        &mut self,
        can_save_part: bool,
        window: &mut Window,
        cx: &mut Context<LubanApp>,
    ) -> AnyElement {
        self.apply_pending(window, cx);
        let weak = cx.weak_entity();
        let whole = self.selection.mode() == RangeMode::Whole;
        let has_target = self.target.is_some();
        let total = self.selection.total();
        let unit = self.unit;
        let size_text = self
            .target
            .as_ref()
            .map(|target| format_bytes(target.size, unit))
            .unwrap_or_else(|| "—".to_string());
        let loaded_text = if has_target {
            format_bytes(self.displayed_loaded(), unit)
        } else {
            "—".to_string()
        };
        let available = self
            .available_space
            .map(|value| format_bytes(value, unit))
            .unwrap_or_else(|| "—".to_string());
        let from_default = tr("lfc.from_default");
        let to_default = tr("lfc.to_default");
        let error = self.error.clone();
        let status = self.status.clone();

        let mut panel = div()
            .w(px(320.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(cx.theme().colors.border)
            .bg(cx.theme().colors.background)
            .occlude()
            .id("lfc-panel")
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());

        let weak_close = weak.clone();
        panel = panel.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(cx.theme().colors.border)
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(tr("lfc.title"))
                .child(
                    div()
                        .id("lfc-close")
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors.secondary))
                        .child("×")
                        .on_click(move |_, _, cx| {
                            let _ = weak_close.update(cx, |this, cx| {
                                this.large_file.open = false;
                                this.large_file.bound_tab = None;
                                cx.notify();
                            });
                        }),
                ),
        );

        let mut body = div()
            .id("lfc-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .text_sm();

        body = body
            .child(self.labeled_value(tr("lfc.file_size"), size_text, cx))
            .child(self.labeled_value(tr("lfc.opened"), loaded_text, cx));

        let weak_stop = weak.clone();
        body = body.child(
            Button::new("lfc-stop")
                .label(tr("lfc.stop"))
                .ghost()
                .compact()
                .w_full()
                .disabled(!self.loading)
                .on_click(move |_, _, cx| {
                    let _ = weak_stop.update(cx, |this, cx| {
                        if this.large_file.stop() {
                            this.set_status(tr("lfc.stopped"), cx);
                        }
                        cx.notify();
                    });
                }),
        );

        body = body.child(div().h(px(4.)));
        for mode in [
            RangeMode::Whole,
            RangeMode::Beginning,
            RangeMode::End,
            RangeMode::Middle,
        ] {
            let selected = self.selection.mode() == mode;
            let weak_mode = weak.clone();
            body = body.child(
                div()
                    .id(("lfc-mode", mode as usize))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors.secondary))
                    .child(div().child(if selected { "●" } else { "○" }))
                    .child(tr(mode.key()))
                    .on_click(move |_, window, cx| {
                        let _ = weak_mode.update(cx, |this, cx| {
                            this.large_file.set_mode(mode, window, cx);
                            cx.notify();
                        });
                    }),
            );
        }

        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .text_xs()
                .text_color(cx.theme().colors.muted_foreground)
                .child(tr("lfc.from"))
                .child(from_default),
        );
        body = body.child(self.field_row(
            "lfc-from",
            self.from_input.clone(),
            self.from_slider.clone(),
            whole || total == 0,
            cx,
        ));
        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .text_xs()
                .text_color(cx.theme().colors.muted_foreground)
                .child(tr("lfc.to"))
                .child(to_default),
        );
        body = body.child(self.field_row(
            "lfc-to",
            self.to_input.clone(),
            self.to_slider.clone(),
            whole || total == 0,
            cx,
        ));
        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .text_xs()
                .text_color(cx.theme().colors.muted_foreground)
                .child(tr("lfc.size")),
        );
        body = body.child(self.field_row(
            "lfc-size",
            self.size_input.clone(),
            self.size_slider.clone(),
            whole || total == 0,
            cx,
        ));

        body = body.child(div().h(px(4.)));
        body = body.child(self.labeled_value(tr("lfc.available_temp"), available, cx));
        let weak_unit = weak.clone();
        let unit_label = tr(unit.key());
        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(tr("lfc.unit"))
                .child(
                    Button::new("lfc-unit")
                        .label(unit_label)
                        .ghost()
                        .compact()
                        .dropdown_menu(move |menu, _window, _cx| {
                            let mut menu = menu;
                            for candidate in SizeUnit::ALL {
                                let weak_item = weak_unit.clone();
                                menu = menu.item(Self::unit_item(&weak_item, candidate, unit));
                            }
                            menu
                        }),
                ),
        );
        if self.loading {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("lfc.loading")),
            );
        }

        // Keep the action buttons outside the scroll area so a slider's hit
        // area can never cover them.
        let mut footer = div()
            .id("lfc-footer")
            .p_3()
            .border_t_1()
            .border_color(cx.theme().colors.border)
            .flex()
            .flex_col()
            .gap_2()
            .text_sm()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if let Some(error) = error {
            footer = footer.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.danger)
                    .child(error),
            );
        }
        if let Some(status) = status {
            footer = footer.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(status),
            );
        }
        if !has_target {
            footer = footer.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors.muted_foreground)
                    .child(tr("lfc.no_target")),
            );
        }

        let weak_apply = weak.clone();
        footer = footer.child(
            Button::new("lfc-apply")
                .label(tr("lfc.apply"))
                .primary()
                .compact()
                .w_full()
                .disabled(!has_target)
                .on_click(move |_, window, cx| {
                    let _ = weak_apply.update(cx, |this, cx| {
                        this.apply_large_file_range(window, cx);
                    });
                }),
        );

        let weak_save = weak.clone();
        footer = footer.child(
            Button::new("lfc-save-part")
                .label(tr("lfc.save_part"))
                .ghost()
                .compact()
                .w_full()
                .disabled(!can_save_part)
                .on_click(move |_, window, cx| {
                    let _ = weak_save.update(cx, |this, cx| {
                        this.save_active_as(window, cx);
                    });
                }),
        );

        panel.child(body).child(footer).into_any_element()
    }

    fn labeled_value(&self, label: String, value: String, cx: &Context<LubanApp>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(label)
            .child(div().text_color(cx.theme().colors.foreground).child(value))
            .into_any_element()
    }

    fn field_row(
        &self,
        _id: &'static str,
        input: Entity<InputState>,
        slider: Entity<SliderState>,
        disabled: bool,
        _cx: &Context<LubanApp>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(Input::new(&input).w_full().h(px(28.)).disabled(disabled)),
            )
            .child(
                div()
                    .h(px(28.))
                    .pb_1()
                    .flex()
                    .items_center()
                    .child(Slider::new(&slider).w_full().disabled(disabled)),
            )
            .into_any_element()
    }

    fn unit_item(
        weak: &WeakEntity<LubanApp>,
        candidate: SizeUnit,
        current: SizeUnit,
    ) -> PopupMenuItem {
        let weak = weak.clone();
        PopupMenuItem::new(tr(candidate.key()))
            .checked(candidate == current)
            .on_click(move |_, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.large_file.unit = candidate;
                    this.settings.large_file_display_unit = candidate;
                    let _ = this.settings.save();
                    cx.notify();
                });
            })
    }
}

fn input_error_message(error: RangeInputError) -> String {
    match error {
        RangeInputError::StartNotBeforeEnd => tr("lfc.error.start_end"),
        RangeInputError::ZeroSize => tr("lfc.error.zero_size"),
    }
}

/// Free space on the filesystem that holds `path`.
pub fn available_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
        if result == 0 {
            return Some(stat.f_bavail as u64 * stat.f_frsize as u64);
        }
        None
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            return Some(free);
        }
        None
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{new_slider_state, slider_ratio, slider_value, RangeField, SLIDER_STEP};
    use luban_core::large_file::RangeSelection;

    fn selection_value(field: RangeField, selection: &RangeSelection) -> u64 {
        match field {
            RangeField::From => selection.from(),
            RangeField::To => selection.to(),
            RangeField::Size => selection.size(),
        }
    }

    #[test]
    fn sliders_use_a_fine_step() {
        let slider = new_slider_state();
        assert_eq!(slider.min_value(), 0.0);
        assert_eq!(slider.max_value(), 1.0);
        assert!(slider.step_value() > 0.0);
        assert!(
            slider.step_value() <= SLIDER_STEP,
            "a coarse step would squeeze every drag onto the track ends"
        );
    }

    #[test]
    fn slider_values_cover_the_whole_range() {
        let selection = RangeSelection::new(1000, 100);
        assert_eq!(slider_value(RangeField::From, 0.0, &selection), 0);
        assert_eq!(slider_value(RangeField::From, 1.0, &selection), 999);
        assert_eq!(slider_value(RangeField::To, 0.0, &selection), 1);
        assert_eq!(slider_value(RangeField::To, 1.0, &selection), 1000);
        assert_eq!(slider_value(RangeField::Size, 0.0, &selection), 1);
        assert_eq!(slider_value(RangeField::Size, 1.0, &selection), 1000);
    }

    #[test]
    fn slider_midpoints_produce_intermediate_values() {
        let selection = RangeSelection::new(1000, 100);
        assert_eq!(slider_value(RangeField::From, 0.5, &selection), 500);
        assert_eq!(slider_value(RangeField::To, 0.5, &selection), 501);
        assert_eq!(slider_value(RangeField::Size, 0.5, &selection), 501);
    }

    #[test]
    fn slider_mapping_considers_the_current_start() {
        let mut selection = RangeSelection::new(1000, 100);
        selection.set_from(400).unwrap();
        assert_eq!(slider_value(RangeField::To, 0.0, &selection), 401);
        assert_eq!(slider_value(RangeField::Size, 0.0, &selection), 1);
        assert_eq!(slider_value(RangeField::Size, 1.0, &selection), 600);
    }

    #[test]
    fn slider_positions_round_trip() {
        let mut selection = RangeSelection::new(1000, 100);
        selection.set_from(250).unwrap();
        selection.set_to(750).unwrap();
        for field in [RangeField::From, RangeField::To, RangeField::Size] {
            let ratio = slider_ratio(field, &selection);
            let value = slider_value(field, ratio, &selection);
            assert_eq!(value, selection_value(field, &selection));
        }
    }
}
