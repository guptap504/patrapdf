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

impl Editor {
    /// The text being typed into, if any.
    pub(super) fn input_text(&self) -> Option<String> {
        if self.password_prompt.is_some() {
            return None;
        }
        if self.searching_recents() {
            return Some(self.recents_query.clone());
        }
        if let Some(index) = self.field_editing {
            return self.field_input_text(index);
        }
        if let Some(id) = self.editing {
            return self.items.iter().find(|i| i.id == id).and_then(|item| match &item.kind {
                ItemKind::Text { text, .. } => Some(text.clone()),
                _ => None,
            });
        }
        if self.watermark_editing {
            // While the whole text is selected, the next input replaces it.
            return Some(match (&self.watermark, self.watermark_replace) {
                (Some(watermark), false) => watermark.text.clone(),
                _ => String::new(),
            });
        }
        None
    }

    fn set_input_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.searching_recents() {
            self.recents_query = text.replace('\n', " ");
            cx.notify();
        } else if let Some(index) = self.field_editing {
            self.type_into_field(index, text, cx);
        } else if self.editing.is_some() {
            self.edit_text(cx, move |t| *t = text);
        } else if self.watermark_editing {
            let line = text.replace('\n', " ");
            self.edit_watermark_text(cx, move |t| *t = line);
        }
    }

    /// The text being composed by an input method (byte range), if still valid.
    pub(super) fn marked_range(&self, text: &str) -> Option<Range<usize>> {
        self.ime_marked
            .clone()
            .filter(|r| r.start <= r.end && r.end <= text.len() && text.is_char_boundary(r.start) && text.is_char_boundary(r.end))
    }

    /// Replace `range` (UTF-16 offsets), or else the text being composed, or
    /// else nothing at the end, with `new`. `mark` keeps it as composing text.
    fn replace_input(&mut self, range: Option<Range<usize>>, new: &str, mark: bool, cx: &mut Context<Self>) {
        let Some(mut text) = self.input_text() else { return };
        let range = range
            .map(|r| byte_offset(&text, r.start)..byte_offset(&text, r.end.max(r.start)))
            .or_else(|| self.marked_range(&text))
            .unwrap_or(text.len()..text.len());
        let new = new.replace("\r\n", "\n").replace('\r', "\n");
        text.replace_range(range.clone(), &new);
        self.ime_marked = (mark && !new.is_empty()).then(|| range.start..range.start + new.len());
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
