use unicode_segmentation::UnicodeSegmentation;

#[derive(Default, Debug, Clone)]
pub struct Editor {
    pub text: String,
    /// Cursor offset in UTF-8 bytes.
    pub cursor: usize,
}

impl Editor {
    pub fn insert(&mut self, text: &str) {
        let clean: String = text
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .map(|c| if matches!(c, '\n' | '\t') { ' ' } else { c })
            .collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    pub fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i);
    }

    pub fn right(&mut self) {
        self.cursor += self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
    }

    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn delete(&mut self) {
        let start = self.cursor;
        self.right();
        let end = self.cursor;
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    pub fn clear_left(&mut self) {
        self.text.replace_range(..self.cursor, "");
        self.cursor = 0;
    }

    pub fn clear_right(&mut self) {
        self.text.truncate(self.cursor);
    }

    pub fn word_left(&mut self) {
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            self.left();
        }
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.left();
        }
    }

    pub fn word_right(&mut self) {
        while self.cursor < self.text.len()
            && self.text[self.cursor..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            self.right();
        }
        while self.cursor < self.text.len()
            && self.text[self.cursor..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.right();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_graphemes_without_splitting_korean_or_combining_text() {
        let mut editor = Editor::default();
        // Wide syllables, a combining mark, and emoji must remain whole graphemes.
        editor.insert("\u{d55c}\u{ae00} e\u{301}🙂");
        editor.left();
        editor.backspace();
        assert_eq!(editor.text, "\u{d55c}\u{ae00} 🙂");

        editor.word_left();
        assert_eq!(editor.cursor, 0);
        editor.right();
        editor.delete();
        assert_eq!(editor.text, "\u{d55c} 🙂");

        editor.cursor = editor.text.len();
        editor.clear_left();
        assert!(editor.text.is_empty());
    }
}
