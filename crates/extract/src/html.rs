//! HTML → text with a single-pass tokenizer (Amendment 3.14).
//!
//! M0-C found html2text (DOM build + tree walk) needs > 10 s CPU on 2 MiB of deeply nested
//! `<div>`s. This extractor builds no tree, so nesting depth has no cost at all: every byte is
//! visited a bounded number of times and every forward search either succeeds and advances past
//! its match, or fails and ends tokenizing (the rest of the input is inside an unterminated
//! construct). Worst case is therefore O(n) for any input.
//!
//! What it does: drops tags, comments, doctype/processing instructions and the content of
//! `script`, `style`, `template`, `noscript`, `svg`, `math` and `head`-only `object`s; turns
//! block-level tags into line breaks and table cells into tabs; decodes character references;
//! collapses whitespace outside `<pre>`.

use memchr::{memchr, memchr2, memchr3, memmem};

use crate::sink::TextSink;

/// Elements whose content is not document text.
const SKIP_CONTENT: &[&[u8]] = &[
    b"script",
    b"style",
    b"template",
    b"noscript",
    b"svg",
    b"math",
    b"object",
    b"iframe",
];

/// Elements that start or end a line.
const BLOCK: &[&[u8]] = &[
    b"p",
    b"div",
    b"br",
    b"li",
    b"ul",
    b"ol",
    b"dl",
    b"dt",
    b"dd",
    b"tr",
    b"table",
    b"thead",
    b"tbody",
    b"tfoot",
    b"caption",
    b"h1",
    b"h2",
    b"h3",
    b"h4",
    b"h5",
    b"h6",
    b"pre",
    b"blockquote",
    b"section",
    b"article",
    b"aside",
    b"header",
    b"footer",
    b"nav",
    b"main",
    b"figure",
    b"figcaption",
    b"hr",
    b"title",
    b"address",
    b"details",
    b"summary",
    b"form",
    b"fieldset",
    b"legend",
];

/// Longest tag name we look at (longer names are still skipped as tags, just not classified).
const MAX_NAME: usize = 16;
/// Longest character reference considered (`&CounterClockwiseContourIntegral;` is 33).
const MAX_ENTITY: usize = 40;

struct Out<'a> {
    sink: &'a mut TextSink,
    pending_space: bool,
    pending_lines: u8,
    pre_depth: u32,
}

impl Out<'_> {
    fn line(&mut self) {
        self.pending_lines = self.pending_lines.max(1);
        self.pending_space = false;
    }

    fn flush_pending(&mut self) {
        if self.sink.is_empty() {
            self.pending_lines = 0;
            self.pending_space = false;
            return;
        }
        if self.pending_lines > 0 {
            self.sink.end_line();
            self.pending_lines = 0;
            self.pending_space = false;
        } else if self.pending_space {
            if !matches!(self.sink.last_char(), Some(' ' | '\n' | '\t')) {
                self.sink.push(' ');
            }
            self.pending_space = false;
        }
    }

    /// Text run without markup.
    fn text(&mut self, run: &str) {
        if self.pre_depth > 0 {
            self.flush_pending();
            self.sink.push_str(run);
            return;
        }
        for (i, word) in run.split(|c: char| c.is_ascii_whitespace()).enumerate() {
            if i > 0 {
                self.pending_space = true;
            }
            if !word.is_empty() {
                self.flush_pending();
                self.sink.push_str(word);
            }
        }
    }

    fn ch(&mut self, c: char) {
        let mut tmp = [0u8; 4];
        let s: &str = c.encode_utf8(&mut tmp);
        if c == '\u{a0}' {
            // Non-breaking space: a real space that never collapses into a line break.
            self.flush_pending();
            self.sink.push(' ');
        } else {
            self.text(s);
        }
    }
}

/// Extracts the text of an HTML document into `sink`.
pub fn html_to_text(src: &str, sink: &mut TextSink) {
    let b = src.as_bytes();
    let n = b.len();
    let mut out = Out {
        sink,
        pending_space: false,
        pending_lines: 0,
        pre_depth: 0,
    };
    let mut i = 0usize;
    let mut text_start = 0usize;
    while i < n && !out.sink.is_full() {
        let Some(off) = memchr2(b'<', b'&', &b[i..]) else {
            break;
        };
        i += off;
        // Positions of '<' / '&' are ASCII, hence char boundaries.
        out.text(&src[text_start..i]);
        if b[i] == b'&' {
            let (c, used) = char_ref(&b[i..]);
            match c {
                Some(c) => out.ch(c),
                None => out.text("&"),
            }
            i += used;
            text_start = i;
            continue;
        }
        // '<'
        match tag(b, i, &mut out) {
            Some(next) => i = next,
            None => {
                // Unterminated comment/tag/raw element: the rest is not text.
                text_start = n;
                break;
            }
        }
        text_start = i;
    }
    if text_start < n && !out.sink.is_full() {
        out.text(&src[text_start..]);
    }
}

/// Handles markup at `b[i] == '<'`. Returns the index after it, or `None` when unterminated.
fn tag(b: &[u8], i: usize, out: &mut Out<'_>) -> Option<usize> {
    let n = b.len();
    let next = b.get(i + 1).copied();
    match next {
        Some(b'!') => {
            if b[i..].starts_with(b"<!--") {
                let end = memmem::find(&b[i + 4..], b"-->")?;
                Some(i + 4 + end + 3)
            } else {
                let end = memchr(b'>', &b[i + 2..])?;
                Some(i + 2 + end + 1)
            }
        }
        Some(b'?') => {
            let end = memchr(b'>', &b[i + 2..])?;
            Some(i + 2 + end + 1)
        }
        Some(c) if c == b'/' || c.is_ascii_alphabetic() => {
            let closing = c == b'/';
            let name_start = i + 1 + usize::from(closing);
            let mut j = name_start;
            while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'-' || b[j] == b':') {
                j += 1;
            }
            let mut name = [0u8; MAX_NAME];
            let name_len = (j - name_start).min(MAX_NAME);
            for (k, slot) in name.iter_mut().take(name_len).enumerate() {
                *slot = b[name_start + k].to_ascii_lowercase();
            }
            let name = &name[..name_len];
            let end = tag_end(b, j)?;
            let self_closing = end > 0 && b[end - 1] == b'/';
            let after = end + 1;

            if !closing && !self_closing && SKIP_CONTENT.contains(&name) {
                return skip_raw(b, after, name);
            }
            if name == b"pre" {
                if closing {
                    out.pre_depth = out.pre_depth.saturating_sub(1);
                } else if !self_closing {
                    out.pre_depth = out.pre_depth.saturating_add(1);
                }
            }
            if BLOCK.contains(&name) {
                out.line();
            } else if (name == b"td" || name == b"th") && !closing && !out.sink.is_empty() {
                out.flush_pending();
                if !matches!(out.sink.last_char(), Some('\n' | '\t')) {
                    out.sink.push('\t');
                }
            }
            Some(after)
        }
        // A lone '<' is text.
        _ => {
            out.text("<");
            Some(i + 1)
        }
    }
}

/// Index of the `>` that ends a tag whose attributes start at `from`. Quotes only count as
/// quoting when they open an attribute value (`= "`), like browsers do.
fn tag_end(b: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    loop {
        let off = memchr3(b'>', b'"', b'\'', &b[j..])?;
        j += off;
        if b[j] == b'>' {
            return Some(j);
        }
        let q = b[j];
        let mut k = j;
        while k > from && b[k - 1].is_ascii_whitespace() {
            k -= 1;
        }
        if k > from && b[k - 1] == b'=' {
            let close = memchr(q, &b[j + 1..])?;
            j = j + 1 + close + 1;
        } else {
            j += 1;
        }
    }
}

/// Skips to after `</name ...>` (ASCII case-insensitive). `None` if never closed.
fn skip_raw(b: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut j = from;
    loop {
        let off = memmem::find(&b[j..], b"</")?;
        let k = j + off + 2;
        let end = k + name.len();
        if end <= b.len()
            && b[k..end].eq_ignore_ascii_case(name)
            && b.get(end)
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'-'))
        {
            let close = memchr(b'>', &b[end..])?;
            return Some(end + close + 1);
        }
        j = k;
    }
}

/// Decodes a character reference at `b[0] == '&'`. Returns the char (if any) and bytes consumed
/// (1 when it is not a reference, so the `&` is kept as text).
fn char_ref(b: &[u8]) -> (Option<char>, usize) {
    let lim = b.len().min(MAX_ENTITY);
    if b.get(1) == Some(&b'#') {
        let hex = matches!(b.get(2), Some(b'x' | b'X'));
        let start = if hex { 3 } else { 2 };
        let mut j = start;
        let mut v: u32 = 0;
        while j < lim {
            let d = match (hex, b[j]) {
                (_, c @ b'0'..=b'9') => u32::from(c - b'0'),
                (true, c @ b'a'..=b'f') => u32::from(c - b'a' + 10),
                (true, c @ b'A'..=b'F') => u32::from(c - b'A' + 10),
                _ => break,
            };
            v = v
                .saturating_mul(if hex { 16 } else { 10 })
                .saturating_add(d);
            j += 1;
        }
        if j == start {
            return (None, 1);
        }
        if b.get(j) == Some(&b';') {
            j += 1;
        }
        let c = match v {
            0 => '\u{FFFD}',
            _ => char::from_u32(v).unwrap_or('\u{FFFD}'),
        };
        return (Some(c), j);
    }
    let mut j = 1;
    while j < lim && b[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j == 1 {
        return (None, 1);
    }
    let name = &b[1..j];
    let has_semi = b.get(j) == Some(&b';');
    // Legacy references without ';' are honoured only for the classic five + nbsp.
    let legacy_ok = matches!(name, b"amp" | b"lt" | b"gt" | b"quot" | b"nbsp" | b"AMP");
    if !has_semi && !legacy_ok {
        return (None, 1);
    }
    match named(name) {
        Some(c) => (Some(c), j + usize::from(has_semi)),
        None => (None, 1),
    }
}

fn named(name: &[u8]) -> Option<char> {
    Some(match name {
        b"amp" | b"AMP" => '&',
        b"lt" | b"LT" => '<',
        b"gt" | b"GT" => '>',
        b"quot" | b"QUOT" => '"',
        b"apos" => '\'',
        b"nbsp" => '\u{a0}',
        b"copy" => '©',
        b"reg" => '®',
        b"trade" => '™',
        b"hellip" => '…',
        b"mdash" => '—',
        b"ndash" => '–',
        b"lsquo" => '‘',
        b"rsquo" => '’',
        b"ldquo" => '“',
        b"rdquo" => '”',
        b"sbquo" => '‚',
        b"bdquo" => '„',
        b"laquo" => '«',
        b"raquo" => '»',
        b"bull" => '•',
        b"middot" => '·',
        b"deg" => '°',
        b"times" => '×',
        b"divide" => '÷',
        b"plusmn" => '±',
        b"euro" => '€',
        b"pound" => '£',
        b"yen" => '¥',
        b"cent" => '¢',
        b"sect" => '§',
        b"para" => '¶',
        b"shy" => '\u{ad}',
        b"zwj" => '\u{200d}',
        b"zwnj" => '\u{200c}',
        b"ensp" | b"emsp" | b"thinsp" => ' ',
        b"larr" => '←',
        b"rarr" => '→',
        b"uarr" => '↑',
        b"darr" => '↓',
        b"hearts" => '♥',
        b"check" => '✓',
        b"auml" => 'ä',
        b"ouml" => 'ö',
        b"uuml" => 'ü',
        b"Auml" => 'Ä',
        b"Ouml" => 'Ö',
        b"Uuml" => 'Ü',
        b"szlig" => 'ß',
        b"eacute" => 'é',
        b"egrave" => 'è',
        b"ecirc" => 'ê',
        b"aacute" => 'á',
        b"agrave" => 'à',
        b"acirc" => 'â',
        b"iacute" => 'í',
        b"oacute" => 'ó',
        b"uacute" => 'ú',
        b"ntilde" => 'ñ',
        b"ccedil" => 'ç',
        b"Eacute" => 'É',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn h(s: &str) -> String {
        let mut sink = TextSink::new(1 << 20, s.len());
        html_to_text(s, &mut sink);
        sink.finish().0
    }

    #[test]
    fn basic_document() {
        let t = h(
            "<!doctype html><html><head><title>T</title><style>p{}</style>\
            <script>var x=1<2;</script></head><body><h1>Heading</h1><p>Hello <b>HTML</b> \
            world &amp; friends.</p><ul><li>one</li><li>two</li></ul><table><tr><td>a</td>\
            <td>b</td></tr></table></body></html>",
        );
        assert_eq!(t, "T\nHeading\nHello HTML world & friends.\none\ntwo\na\tb");
    }

    #[test]
    fn entities_and_quotes() {
        assert_eq!(
            h("a&lt;b&gt;c &#65;&#x42; &nbsp;x &bogus; &amp"),
            "a<b>c AB  x &bogus; &"
        );
        assert_eq!(h(r#"<a title="x > y" href='q'>link</a> it's"#), "link it's");
        assert_eq!(h("<a title=it's>t</a>"), "t");
        assert_eq!(
            h("&#0;&#x110000;&#99999999999;"),
            "\u{FFFD}\u{FFFD}\u{FFFD}"
        );
    }

    #[test]
    fn pre_keeps_whitespace() {
        assert_eq!(h("<p>a   b</p><pre>x\n  y</pre>z"), "a b\nx\n  y\nz");
    }

    #[test]
    fn unterminated_constructs_end_cleanly() {
        assert_eq!(h("text <!-- never closed"), "text");
        assert_eq!(h("text <script>evil()"), "text");
        assert_eq!(h("text <a href=\"x"), "text");
        assert_eq!(h("a < b"), "a < b");
        assert_eq!(h("<scriptx>kept</scriptx>"), "kept");
        assert_eq!(h("<script>a</scriptx></script>b"), "b");
    }

    #[test]
    fn deep_nesting_is_linear() {
        // M0-C fixture shape: 200k nested divs (2.2 MB) took > 10 s with html2text.
        let s = format!(
            "<html><body>{}deep{}</body></html>",
            "<div>".repeat(200_000),
            "</div>".repeat(200_000)
        );
        let t = Instant::now();
        assert_eq!(h(&s), "deep");
        assert!(t.elapsed().as_millis() < 2000, "{:?}", t.elapsed());
    }

    #[test]
    fn pathological_shapes_are_linear() {
        let t = Instant::now();
        for s in [
            "<a".repeat(500_000),
            "<!--".repeat(500_000),
            "&#".repeat(500_000),
            "<a x='".repeat(300_000),
            "<script></scrip".repeat(200_000),
            "<".repeat(1_000_000),
            "&".repeat(1_000_000),
        ] {
            let _ = h(&s);
        }
        assert!(t.elapsed().as_millis() < 5000, "{:?}", t.elapsed());
    }

    #[test]
    fn output_cap_is_honoured() {
        let s = "<p>word</p>".repeat(100_000);
        let mut sink = TextSink::new(1000, s.len());
        html_to_text(&s, &mut sink);
        let (t, trunc) = sink.finish();
        assert!(trunc && t.len() <= 1000);
    }
}
