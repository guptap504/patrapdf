//! Typing through the platform's text input: input methods (composing
//! Chinese, Japanese or Korean, accents with dead keys), the emoji picker and
//! dictation all insert text this way. Text is always typed at the end.

use std::ops::Range;

use gpui::{
    Bounds, Context, ElementInputHandler, EntityInputHandler, IntoElement, Pixels, Point, Styled, UTF16Selection,
    Window, canvas,
};

use super::Editor;
use crate::model::ItemKind;

/// What typed text goes into.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum InputTarget {
    /// The Recents search box.
    Recents,
    /// A form field of the document (its index).
    Field(usize),
    /// A text item (its id).
    Item(u64),
    /// The find bar.
    Find,
    Watermark,
}

impl Editor {
    /// What is being typed into, if anything. The password prompt takes its
    /// keys directly instead (no input method, as in system password fields),
    /// and so do the prompts for the pages to add and the page to go to.
    pub(super) fn input_target(&self) -> Option<InputTarget> {
        if self.password_prompt.is_some() || self.import_prompt.is_some() || self.goto.is_some() {
            None
        } else if self.searching_recents() {
            Some(InputTarget::Recents)
        } else if let Some(index) = self.field_editing {
            Some(InputTarget::Field(index))
        } else if let Some(id) = self.editing {
            Some(InputTarget::Item(id))
        } else if self.find_focused() {
            Some(InputTarget::Find)
        } else {
            self.watermark_editing.then_some(InputTarget::Watermark)
        }
    }

    /// The text being typed into, if any.
    pub(super) fn input_text(&self) -> Option<String> {
        match self.input_target()? {
            InputTarget::Recents => Some(self.recents_query.clone()),
            InputTarget::Field(index) => self.field_input_text(index),
            InputTarget::Find => self.find.as_ref().map(|find| find.query.clone()),
            InputTarget::Item(id) => self.items.iter().find(|i| i.id == id).and_then(|item| match &item.kind {
                ItemKind::Text { text, .. } => Some(text.clone()),
                _ => None,
            }),
            // While the whole text is selected, the next input replaces it.
            InputTarget::Watermark => Some(match (&self.watermark, self.watermark_replace) {
                (Some(watermark), false) => watermark.text.clone(),
                _ => String::new(),
            }),
        }
    }

    fn set_input_text(&mut self, text: String, cx: &mut Context<Self>) {
        match self.input_target() {
            Some(InputTarget::Recents) => {
                self.recents_query = text.replace('\n', " ");
                cx.notify();
            }
            Some(InputTarget::Field(index)) => self.type_into_field(index, text, cx),
            Some(InputTarget::Find) => self.set_find_query(text.replace('\n', " "), cx),
            Some(InputTarget::Item(_)) => self.edit_text(cx, move |t| *t = text),
            Some(InputTarget::Watermark) => {
                let line = text.replace('\n', " ");
                self.edit_watermark_text(cx, move |t| *t = line);
            }
            None => {}
        }
    }

    /// The part of `text` (what is being typed into) that an input method is
    /// still composing, as a byte range, if there is one and it's still valid.
    pub(super) fn marked_range(&self, text: &str) -> Option<Range<usize>> {
        let (target, r) = self.ime_marked.clone()?;
        let valid = Some(target) == self.input_target()
            && r.start <= r.end
            && r.end <= text.len()
            && text.is_char_boundary(r.start)
            && text.is_char_boundary(r.end);
        valid.then_some(r)
    }

    /// Backspace: delete the last character of what is being typed into.
    pub(super) fn delete_backward(&mut self, cx: &mut Context<Self>) {
        let Some(mut text) = self.input_text() else { return };
        text.pop();
        self.ime_marked = None;
        self.set_input_text(text, cx);
    }

    /// Replace `range` (UTF-16 offsets), or else the text being composed, or
    /// else nothing at the end, with `new`. `mark` keeps it as composing text.
    pub(super) fn replace_input(&mut self, range: Option<Range<usize>>, new: &str, mark: bool, cx: &mut Context<Self>) {
        let Some(mut text) = self.input_text() else { return };
        let range = range
            .map(|r| byte_offset(&text, r.start)..byte_offset(&text, r.end.max(r.start)))
            .or_else(|| self.marked_range(&text))
            .unwrap_or(text.len()..text.len());
        let new = new.replace("\r\n", "\n").replace('\r', "\n");
        text.replace_range(range.clone(), &new);
        let marked = range.start..range.start + new.len();
        self.ime_marked = self.input_target().filter(|_| mark && !new.is_empty()).map(|target| (target, marked));
        self.set_input_text(text, cx);
    }

    /// An invisible element that receives typed text while it's painted. Put it
    /// where the caret is, so input method windows open next to it.
    pub(super) fn input_anchor(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let focus = self.focus.clone();
        let editor = cx.entity();
        canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, editor.clone()), cx);
            },
        )
        .absolute()
        .size_full()
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.input_text()?;
        let start = byte_offset(&text, range.start);
        let end = byte_offset(&text, range.end).max(start);
        *adjusted = Some(utf16_offset(&text, start)..utf16_offset(&text, end));
        Some(text[start..end].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let text = self.input_text()?;
        let end = utf16_offset(&text, text.len());
        Some(UTF16Selection { range: end..end, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let text = self.input_text()?;
        let marked = self.marked_range(&text)?;
        Some(utf16_offset(&text, marked.start)..utf16_offset(&text, marked.end))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.ime_marked = None;
        cx.notify();
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        self.replace_input(range, text, false, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_input(range, text, true, cx);
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // The anchor is the caret, and text is only ever typed at the caret.
        Some(element_bounds)
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

/// The byte offset of a UTF-16 offset (clamped to the text).
fn byte_offset(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// The UTF-16 offset of a byte offset (a char boundary).
fn utf16_offset(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_convert_between_utf16_and_bytes() {
        let text = "aé😀中";
        // a(1) é(1) 😀(2) 中(1) in UTF-16; 1 + 2 + 4 + 3 bytes.
        assert_eq!(byte_offset(text, 0), 0);
        assert_eq!(byte_offset(text, 2), 3);
        assert_eq!(byte_offset(text, 4), 7);
        assert_eq!(byte_offset(text, 5), 10);
        assert_eq!(byte_offset(text, 99), 10);
        assert_eq!(utf16_offset(text, 7), 4);
        assert_eq!(utf16_offset(text, 10), 5);
    }
}
