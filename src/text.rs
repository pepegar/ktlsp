//! Byte-offset <-> (line, UTF-16 column) conversion. Pure; no LSP types.
//!
//! LSP `Position.character` is a UTF-16 code-unit index, while tree-sitter (and our core)
//! speak UTF-8 byte offsets. This module is the single owner of that conversion. It is the
//! only place in the core that knows about UTF-16, and the LSP layer uses it to translate
//! the byte ranges our resolver returns into editor positions.

/// Maps byte offsets to/from `(line, utf16_col)` for one text buffer.
pub struct LineIndex {
    /// Byte offset of the start of each line. `line_starts[0] == 0`.
    line_starts: Vec<usize>,
    len: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex {
            line_starts,
            len: text.len(),
        }
    }

    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(line) => line - 1,
        }
    }

    /// Byte offset -> (line, UTF-16 column). Clamps out-of-range offsets to EOF, and floors a
    /// non-char-boundary offset (e.g. a stale index range against changed text) to the previous
    /// boundary so we never panic slicing mid-character.
    pub fn position(&self, text: &str, offset: usize) -> (u32, u32) {
        let mut offset = offset.min(self.len);
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.line_of(offset);
        let line_start = self.line_starts[line];
        let col = text[line_start..offset].encode_utf16().count();
        (line as u32, col as u32)
    }

    /// (line, UTF-16 column) -> byte offset. CRLF-aware: a trailing `\r\n` is not counted
    /// as line content, so a column past the last visible char clamps to the line end.
    pub fn offset(&self, text: &str, line: u32, col: u32) -> usize {
        let line = line as usize;
        if line >= self.line_starts.len() {
            return self.len;
        }
        let line_start = self.line_starts[line];
        let line_end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.len);
        let mut content = &text[line_start..line_end];
        if let Some(s) = content.strip_suffix('\n') {
            content = s;
        }
        if let Some(s) = content.strip_suffix('\r') {
            content = s;
        }
        let mut u16_col = 0u32;
        let mut offset = line_start;
        for ch in content.chars() {
            if u16_col >= col {
                break;
            }
            u16_col += ch.len_utf16() as u32;
            offset += ch.len_utf8();
        }
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_roundtrip() {
        let text = "fun main() {\n    val x = 1\n}\n";
        let li = LineIndex::new(text);
        // start of `val` is line 1, col 4
        let off = text.find("val").unwrap();
        assert_eq!(li.position(text, off), (1, 4));
        assert_eq!(li.offset(text, 1, 4), off);
    }

    #[test]
    fn unicode_utf16_columns() {
        // "é" is 2 UTF-8 bytes, 1 UTF-16 unit. "𝟙" (U+1D7D9) is 4 UTF-8 bytes, 2 UTF-16 units.
        let text = "val é = 𝟙\nval y = 2\n";
        let li = LineIndex::new(text);
        let y_off = text.find('y').unwrap();
        // line 1, col 4 regardless of multibyte chars on line 0
        assert_eq!(li.position(text, y_off), (1, 4));
        // column after "𝟙" on line 0: "val 𝟙"? compute position of the newline-ish end.
        let star = text.find('𝟙').unwrap();
        let (l, c) = li.position(text, star);
        assert_eq!(l, 0);
        // "val é = " -> v a l space é space = space = 8 UTF-16 units
        assert_eq!(c, 8);
        // round-trip back to the byte offset of "𝟙"
        assert_eq!(li.offset(text, 0, 8), star);
    }

    #[test]
    fn crlf_lines() {
        let text = "val a = 1\r\nval b = 2\r\n";
        let li = LineIndex::new(text);
        let b_off = text.find('b').unwrap();
        assert_eq!(li.position(text, b_off), (1, 4));
        assert_eq!(li.offset(text, 1, 4), b_off);
        // a column past the content clamps before the \r
        let eol = li.offset(text, 0, 999);
        assert_eq!(&text[eol..eol + 2], "\r\n");
    }
}
