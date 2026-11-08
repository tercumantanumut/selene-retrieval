//! Bounded text output shared by every parser.

/// A `String` that never grows beyond `cap` bytes. Writes past the cap are cut on a char
/// boundary and set [`TextSink::is_full`]; parsers check it to stop early.
#[derive(Debug)]
pub struct TextSink {
    buf: String,
    cap: usize,
    full: bool,
}

impl TextSink {
    /// `size_hint` pre-reserves (clamped to `cap` and 1 MiB) to avoid regrowth on small files.
    pub fn new(cap: usize, size_hint: usize) -> Self {
        Self {
            buf: String::with_capacity(size_hint.min(cap).min(1 << 20)),
            cap,
            full: false,
        }
    }

    /// Appends `s`, cutting it at the cap. Returns `false` once the sink is full.
    pub fn push_str(&mut self, s: &str) -> bool {
        if self.full {
            return false;
        }
        let room = self.cap - self.buf.len();
        if s.len() <= room {
            self.buf.push_str(s);
            return true;
        }
        let mut cut = room;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.buf.push_str(&s[..cut]);
        self.full = true;
        false
    }

    pub fn push(&mut self, c: char) -> bool {
        let mut tmp = [0u8; 4];
        self.push_str(c.encode_utf8(&mut tmp))
    }

    pub fn is_full(&self) -> bool {
        self.full
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Last char written, if any.
    pub fn last_char(&self) -> Option<char> {
        self.buf.chars().next_back()
    }

    /// Ensures the text ends with a line break (no-op on empty output).
    pub fn end_line(&mut self) {
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.push('\n');
        }
    }

    /// `(text, truncated)`.
    pub fn finish(self) -> (String, bool) {
        (self.buf, self.full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_on_char_boundary_and_reports_full() {
        let mut s = TextSink::new(5, 0);
        assert!(s.push_str("ab"));
        assert!(!s.push_str("cé€")); // 'c' (1) + 'é' (2) fits = 5 bytes, '€' cut
        assert!(s.is_full());
        assert!(!s.push_str("x"));
        let (t, trunc) = s.finish();
        assert_eq!(t, "abcé");
        assert!(trunc);
    }

    #[test]
    fn exact_fit_is_not_truncated() {
        let mut s = TextSink::new(3, 0);
        assert!(s.push_str("abc"));
        assert!(!s.is_full());
        let (t, trunc) = s.finish();
        assert_eq!((t.as_str(), trunc), ("abc", false));
    }
}
