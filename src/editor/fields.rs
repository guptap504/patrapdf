//! The document's own form fields: drawn over the page with their current
//! values (the page image is rendered without them), and filled in by
//! clicking and typing. Values are saved into the fields themselves.

use std::sync::Arc;

use gpui::{
    AnyElement, Bounds, Context, CursorStyle, Hsla, MouseButton, MouseDownEvent, PathBuilder, SharedString, Window,
    anchored, canvas, deferred, div, fill, point, prelude::*, px, quad,
};

use super::{Editor, Tool};
use crate::forms::{self, Field, FieldKind, FieldValue, Mark, Widget};
use crate::model::{LINE_HEIGHT, TEXT_FONT};
use crate::{pdf, ui};

fn color([r, g, b]: [f32; 3]) -> Hsla {
    gpui::Rgba { r, g, b, a: 1. }.into()
}

impl Editor {
    /// The open document's form fields.
    pub(super) fn form(&self) -> Arc<Vec<Field>> {
        self.doc.as_ref().map(|d| d.loaded.form.clone()).unwrap_or_default()
    }

    /// The text of the field being typed into.
    pub(super) fn field_input_text(&self, index: usize) -> Option<String> {
        match self.fields.get(index)? {
            FieldValue::Text(text) => Some(text.clone()),
            FieldValue::On(_) => None,
        }
    }

    /// Typing into a text field (or an editable combo box). Undo takes back a
    /// whole run of typing at once.
    pub(super) fn type_into_field(&mut self, index: usize, text: String, cx: &mut Context<Self>) {
        let form = self.form();
        let Some(field) = form.get(index) else { return };
        let value = FieldValue::Text(field.accept(&text));
        if self.fields.get(index) == Some(&value) {
            cx.notify();
            return;
        }
        if !self.typing_checkpointed {
            self.checkpoint();
            self.typing_checkpointed = true;
        }
        self.mark_dirty();
        self.fields[index] = value;
        cx.notify();
    }

    /// Set a field's value as one undoable step.
    pub(super) fn set_field_value(&mut self, index: usize, value: FieldValue, cx: &mut Context<Self>) {
        if self.fields.get(index).is_none_or(|v| *v == value) {
            return;
        }
        self.checkpoint();
        self.fields[index] = value;
        cx.notify();
    }

    pub(super) fn start_field_editing(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.selected = None;
        self.field_editing = Some(index);
        self.typing_checkpointed = false;
        self.ime_marked = None;
        cx.notify();
    }

    /// Move to the next (or previous) text field, in reading order: by page,
    /// then top to bottom, then left to right.
    pub(super) fn next_field(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
        let form = doc.loaded.form.clone();
        let mut stops: Vec<(usize, i64, i64, usize)> = form
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(f.kind, FieldKind::Text { .. } | FieldKind::Choice { editable: true, .. }))
            .filter_map(|(i, f)| {
                f.widgets.iter().find_map(|w| {
                    let page = doc.order.iter().position(|&p| p == w.page)?;
                    Some((page, w.rect[1].round() as i64, w.rect[0].round() as i64, i))
                })
            })
            .collect();
        if stops.is_empty() {
            return;
        }
        stops.sort();
        let current = self.field_editing.and_then(|i| stops.iter().position(|s| s.3 == i));
        let n = stops.len();
        let next = match (current, back) {
            (Some(c), false) => (c + 1) % n,
            (Some(c), true) => (c + n - 1) % n,
            (None, false) => 0,
            (None, true) => n - 1,
        };
        let (page, _, _, index) = stops[next];
        self.finish_editing(cx);
        self.close_menus();
        if self.current_page() != Some(page) {
            self.scroll_to_page(page, cx);
        }
        self.start_field_editing(index, window, cx);
    }

    fn widget_mouse_down(
        &mut self,
        index: usize,
        widget_index: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // With a placing tool, the click places a new item instead.
        if self.tool != Tool::Select {
            return;
        }
        cx.stop_propagation();
        window.focus(&self.focus);
        let form = self.form();
        let Some(field) = form.get(index) else { return };
        let Some(widget) = field.widgets.get(widget_index) else { return };
        // A click that just closed this field's own list shouldn't reopen it.
        let dismissed = std::mem::take(&mut self.menu_dismissed);
        let was_open = self.choice_open == Some(index);
        self.finish_editing(cx);
        self.close_menus();
        self.selected = None;
        let value = self.fields.get(index).cloned();
        match &field.kind {
            FieldKind::Text { .. } => self.start_field_editing(index, window, cx),
            FieldKind::CheckBox | FieldKind::Radio { .. } => {
                let on = widget.on_state.clone();
                let selected = matches!(&value, Some(FieldValue::On(Some(s))) if Some(s) == on.as_ref());
                let no_toggle_off = matches!(field.kind, FieldKind::Radio { no_toggle_off: true });
                if selected && no_toggle_off {
                    return;
                }
                let new = if selected { None } else { on };
                self.set_field_value(index, FieldValue::On(new), cx);
            }
            FieldKind::Choice { combo: true, editable, .. } => {
                if !(was_open || dismissed) {
                    self.choice_open = Some(index);
                }
                if *editable {
                    self.start_field_editing(index, window, cx);
                }
            }
            FieldKind::Choice { combo: false, options, .. } => {
                let Some((_, _, y)) = self.page_at(event.position) else { return };
                let Some(value) = value else { return };
                if let Some(option) = forms::list_option_at(field, widget, &value, y - widget.rect[1]) {
                    self.set_field_value(index, FieldValue::Text(options[option].value.clone()), cx);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn choose_option(&mut self, index: usize, value: String, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        self.choice_open = None;
        self.set_field_value(index, FieldValue::Text(value), cx);
        cx.notify();
    }

    /// The fields on the page with original index `source`.
    pub(super) fn render_widgets(&self, source: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let form = self.form();
        let mut out = Vec::new();
        for (index, field) in form.iter().enumerate() {
            let Some(value) = self.fields.get(index) else { continue };
            for (widget_index, widget) in field.widgets.iter().enumerate() {
                if widget.page == source {
                    out.push(self.render_widget(index, field, widget_index, widget, value, cx));
                }
            }
        }
        out
    }

    fn render_widget(
        &self,
        index: usize,
        field: &Field,
        widget_index: usize,
        widget: &Widget,
        value: &FieldValue,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let z = self.zoom;
        let [x, y, w, h] = widget.rect;
        let editing = self.field_editing == Some(index);
        // Turned fields are previewed upright, filling their box on screen.
        let shown = if widget.upright { widget.clone() } else { Widget { size: (w, h), ..widget.clone() } };
        let ratio = self.metrics.baseline_ratio;
        let layout = match field.kind {
            FieldKind::Text { .. } | FieldKind::Choice { .. } => {
                Some(forms::layout(field, &shown, value, ratio, &mut pdf::measure_text))
            }
            _ => None,
        };
        let mark = match (&field.kind, value) {
            (FieldKind::CheckBox | FieldKind::Radio { .. }, FieldValue::On(Some(on))) if widget.on_state.as_ref() == Some(on) => {
                Some(widget.symbol.mark())
            }
            _ => None,
        };

        // Background, border, list highlight and check mark.
        let (background, border, border_width, text_color) =
            (widget.background, widget.border, widget.border_width, widget.text_color);
        let highlight = layout.as_ref().and_then(|l| l.highlight);
        let decoration = canvas(
            |_, _, _| {},
            move |bounds: Bounds<gpui::Pixels>, _, window, _| {
                if let Some(bg) = background {
                    window.paint_quad(fill(bounds, color(bg)));
                }
                // The usual light tint that marks fillable fields.
                window.paint_quad(fill(bounds, ui::accent().opacity(0.1)));
                if let Some((top, height)) = highlight {
                    let band = Bounds::new(bounds.origin + point(px(0.), px(top * z)), gpui::size(bounds.size.width, px(height * z)));
                    window.paint_quad(fill(band, color(forms::LIST_HIGHLIGHT)));
                }
                if let (Some(bc), true) = (border, border_width > 0.) {
                    window.paint_quad(quad(bounds, px(0.), gpui::transparent_black(), px(border_width * z), color(bc), Default::default()));
                }
                if let Some(mark) = &mark {
                    let d = ((w.min(h)) - 2. * (border_width + 1.)).max(1.) * z;
                    let origin = bounds.origin + point(px((w * z - d) / 2.), px((h * z - d) / 2.));
                    let p = |(mx, my): (f32, f32)| origin + point(px(mx * d), px((1. - my) * d));
                    let path = match mark {
                        Mark::Lines(lines) => {
                            let mut b = PathBuilder::stroke(px(d * 0.12));
                            for line in lines {
                                for (i, &point) in line.iter().enumerate() {
                                    if i == 0 { b.move_to(p(point)) } else { b.line_to(p(point)) }
                                }
                            }
                            b.build()
                        }
                        Mark::Polygon(points) => {
                            let mut b = PathBuilder::fill();
                            for (i, &point) in points.iter().enumerate() {
                                if i == 0 { b.move_to(p(point)) } else { b.line_to(p(point)) }
                            }
                            b.close();
                            b.build()
                        }
                        Mark::Disc(r) => {
                            let mut b = PathBuilder::fill();
                            for i in 0..=32 {
                                let a = i as f32 / 32. * std::f32::consts::TAU;
                                let point = (0.5 + r * a.cos(), 0.5 + r * a.sin());
                                if i == 0 { b.move_to(p(point)) } else { b.line_to(p(point)) }
                            }
                            b.close();
                            b.build()
                        }
                    };
                    if let Ok(path) = path {
                        window.paint_path(path, color(text_color));
                    }
                }
            },
        )
        .absolute()
        .size_full();

        let text_runs = layout.as_ref().map(|layout| {
            let size = layout.size;
            let line = |x: f32, baseline: f32| (px(x * z), px((baseline - ratio * size) * z));
            let mut runs: Vec<AnyElement> = layout
                .runs
                .iter()
                .map(|run| {
                    let (left, top) = line(run.x, run.baseline);
                    div()
                        .absolute()
                        .left(left)
                        .top(top)
                        .whitespace_nowrap()
                        .child(run.text.clone())
                        .into_any_element()
                })
                .collect();
            if editing {
                let (left, top) = line(layout.caret.0, layout.caret.1);
                runs.push(
                    div()
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(px(1.5))
                        .h(px(size * LINE_HEIGHT * z))
                        .bg(ui::accent())
                        .child(self.input_anchor(cx))
                        .into_any_element(),
                );
            }
            div()
                .absolute()
                .size_full()
                .font_family(TEXT_FONT)
                .text_size(px(size * z))
                .line_height(px(size * LINE_HEIGHT * z))
                .text_color(color(text_color))
                .children(runs)
        });

        let combo = matches!(field.kind, FieldKind::Choice { combo: true, .. });
        let cursor = match field.kind {
            FieldKind::Text { .. } | FieldKind::Choice { editable: true, .. } => CursorStyle::IBeam,
            _ => CursorStyle::PointingHand,
        };
        let name: SharedString = field.name.clone().into();
        let mut element = div()
            .id(SharedString::from(format!("form-field-{index}-{widget_index}")))
            .absolute()
            .left(px(x * z))
            .top(px(y * z))
            .w(px(w * z))
            .h(px(h * z))
            .cursor(cursor)
            .child(decoration)
            .child(div().absolute().size_full().overflow_hidden().children(text_runs))
            .when(combo, |d| {
                d.child(
                    div()
                        .absolute()
                        .right(px(1.))
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .child(ui::icon("chevron-down", color(text_color)).size(px((h * z * 0.6).clamp(8., 16.)))),
                )
            })
            .when(editing, |d| d.shadow(ui::ring()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| this.widget_mouse_down(index, widget_index, e, window, cx)),
            );
        if !editing && !field.name.is_empty() {
            element = element.tooltip(ui::tooltip(name, None));
        }
        if self.choice_open == Some(index) && widget_index == 0 {
            element = element.child(self.render_choice_menu(index, field, value, cx));
        }
        element.into_any_element()
    }

    /// The list of a combo box's options.
    fn render_choice_menu(&self, index: usize, field: &Field, value: &FieldValue, cx: &mut Context<Self>) -> AnyElement {
        let FieldKind::Choice { options, .. } = &field.kind else { return div().into_any_element() };
        let selected = match value {
            FieldValue::Text(v) => Some(v.as_str()),
            FieldValue::On(_) => None,
        };
        let items = options.iter().enumerate().map(|(i, option)| {
            let chosen = option.value.clone();
            ui::menu_item(("form-option", i), option.label.clone(), selected == Some(option.value.as_str()), None)
                .on_click(cx.listener(move |this, _, _, cx| this.choose_option(index, chosen.clone(), cx)))
        });
        let panel = ui::menu_panel()
            .child(
                div()
                    .id("form-options")
                    .flex()
                    .flex_col()
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .children(items)
                    .when(options.is_empty(), |d| d.child(ui::menu_caption("No options"))),
            )
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                if this.choice_open.is_some() {
                    this.choice_open = None;
                    this.menu_dismissed = true;
                    cx.notify();
                }
            }));
        div()
            .absolute()
            .top_full()
            .left_0()
            .child(deferred(anchored().snap_to_window_with_margin(px(8.)).child(panel)).with_priority(1))
            .into_any_element()
    }
}
