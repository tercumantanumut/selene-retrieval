//! Text decoding with encoding detection and binary sniffing.
//!
//! Order: UTF-32 BOM → UTF-8/UTF-16 BOM → BOM-less UTF-16 (zero-byte parity) → NUL window
//! (binary) → UTF-8 (tolerating ≤ 1 invalid sequence per 1000 bytes) → chardetng legacy guess.
//! Every decoded result then passes [`looks_binary`]: control and replacement characters in
//! samples from the start, middle and end of the text. Random bytes fail one of the two checks
//! (M0-C: the old 1 % NUL heuristic indexed 1 MiB of random bytes as text).
//!
//! The text is returned verbatim (line endings and all), so line numbers match the file.

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
use sr_common::protocol::extract::{MAX_PAYLOAD_BYTES, WorkerFailure};

use crate::Failure;

/// Any NUL byte in this many leading bytes (after the UTF-16 check) means binary (git's rule).
const NUL_WINDOW: usize = 8000;
/// Bytes inspected for BOM-less UTF-16.
const UTF16_SNIFF: usize = 64 * 1024;
/// Bytes fed to the legacy-encoding detector.
const DETECT_WINDOW: usize = 1 << 20;
/// Chars sampled per window by [`looks_binary`] (three windows: start, middle, end).
const SAMPLE_CHARS: usize = 16 * 1024;
/// Binary when more than 1 in this many sampled chars is a control or replacement char.
const SUSPICIOUS_ONE_IN: usize = 50;

/// Decoded text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub text: String,
    /// WHATWG encoding name (`UTF-8`, `UTF-16LE`, `windows-1252`, ...) or `UTF-32LE/BE`.
    pub encoding: &'static str,
    /// The input or the output was cut at the cap.
    pub truncated: bool,
}

/// Input window for HTML: markup shrinks, so read up to 4× the text cap.
pub fn html_input_window(max_text: usize) -> usize {
    let payload_cap = usize::try_from(MAX_PAYLOAD_BYTES).unwrap_or(usize::MAX);
    max_text.saturating_mul(4).min(payload_cap)
}

/// Decodes `b` to at most `cap` bytes of UTF-8. Fails with `Binary` for non-text content.
pub fn decode(b: &[u8], cap: usize) -> Result<Decoded, Failure> {
    if b.is_empty() {
        return Ok(Decoded {
            text: String::new(),
            encoding: UTF_8.name(),
            truncated: false,
        });
    }
    // UTF-32 BOMs first: FF FE 00 00 also starts like a UTF-16LE BOM.
    if b.starts_with(&[0xFF, 0xFE, 0, 0]) {
        return checked(utf32(&b[4..], true, cap), "UTF-32LE");
    }
    if b.starts_with(&[0, 0, 0xFE, 0xFF]) {
        return checked(utf32(&b[4..], false, cap), "UTF-32BE");
    }
    if let Some((enc, bom_len)) = Encoding::for_bom(b) {
        return checked(decode_with(enc, &b[bom_len..], cap), enc.name());
    }
    if let Some(enc) = sniff_utf16(&b[..b.len().min(UTF16_SNIFF)]) {
        return checked(decode_with(enc, b, cap), enc.name());
    }
    if b[..b.len().min(NUL_WINDOW)].contains(&0) {
        return Err(Failure::new(
            WorkerFailure::Binary,
            "NUL bytes near the start of the file",
        ));
    }
    let mut window = &b[..b.len().min(cap)];
    if window.len() < b.len() {
        // A multi-byte char cut by the window edge is not an encoding error.
        window = &window[..utf8_floor(window)];
    }
    let invalid = window
        .utf8_chunks()
        .filter(|c| !c.invalid().is_empty())
        .count();
    if invalid == 0 || invalid.saturating_mul(1000) <= window.len() {
        return checked(decode_with(UTF_8, b, cap), UTF_8.name());
    }
    let enc = guess_legacy(&b[..b.len().min(DETECT_WINDOW)]);
    checked(decode_with(enc, b, cap), enc.name())
}

fn checked((text, truncated): (String, bool), encoding: &'static str) -> Result<Decoded, Failure> {
    if looks_binary(&text) {
        return Err(Failure::new(
            WorkerFailure::Binary,
            format!("too many control characters after decoding as {encoding}"),
        ));
    }
    Ok(Decoded {
        text,
        encoding,
        truncated,
    })
}

/// Decodes with `enc`, reading at most what can produce `cap` bytes of output.
fn decode_with(enc: &'static Encoding, b: &[u8], cap: usize) -> (String, bool) {
    let limit = if enc == UTF_16LE || enc == UTF_16BE {
        cap.saturating_mul(2)
    } else {
        cap
    };
    let input_cut = b.len() > limit;
    let mut slice = &b[..b.len().min(limit)];
    if input_cut && enc == UTF_8 {
        slice = &slice[..utf8_floor(slice)];
    }
    let (cow, _had_errors) = enc.decode_without_bom_handling(slice);
    let mut s = cow.into_owned();
    let out_cut = truncate_at(&mut s, cap);
    (s, input_cut || out_cut)
}

/// Length of `b` without a trailing incomplete UTF-8 sequence.
fn utf8_floor(b: &[u8]) -> usize {
    let len = b.len();
    for back in 1..=len.min(4) {
        let i = len - back;
        let c = b[i];
        if c & 0xC0 != 0x80 {
            let need = match c {
                0xF0..=0xFF => 4,
                0xE0..=0xEF => 3,
                0xC0..=0xDF => 2,
                _ => 1,
            };
            return if back < need { i } else { len };
        }
    }
    len
}

/// Truncates on a char boundary. Returns whether anything was cut.
pub(crate) fn truncate_at(s: &mut String, cap: usize) -> bool {
    if s.len() <= cap {
        return false;
    }
    let mut cut = cap;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    true
}

fn utf32(b: &[u8], le: bool, cap: usize) -> (String, bool) {
    let mut s = String::with_capacity(b.len().min(cap).min(1 << 20));
    let mut cut = false;
    for ch in b.chunks(4) {
        let c = match <[u8; 4]>::try_from(ch) {
            Ok(a) => {
                let v = if le {
                    u32::from_le_bytes(a)
                } else {
                    u32::from_be_bytes(a)
                };
                char::from_u32(v).unwrap_or('\u{FFFD}')
            }
            Err(_) => '\u{FFFD}',
        };
        if s.len() + c.len_utf8() > cap {
            cut = true;
            break;
        }
        s.push(c);
    }
    (s, cut)
}

/// BOM-less UTF-16: ASCII-heavy text has zero high bytes in one parity and almost none in the other.
fn sniff_utf16(head: &[u8]) -> Option<&'static Encoding> {
    let pairs = head.len() / 2;
    if pairs < 2 {
        return None;
    }
    let (mut zero_even, mut zero_odd) = (0usize, 0usize);
    for p in head.as_chunks::<2>().0 {
        zero_even += usize::from(p[0] == 0);
        zero_odd += usize::from(p[1] == 0);
    }
    if zero_odd * 10 >= pairs * 3 && zero_even * 20 <= pairs {
        Some(UTF_16LE)
    } else if zero_even * 10 >= pairs * 3 && zero_odd * 20 <= pairs {
        Some(UTF_16BE)
    } else {
        None
    }
}

fn guess_legacy(window: &[u8]) -> &'static Encoding {
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(window, true);
    det.guess(None, chardetng::Utf8Detection::Deny)
}

fn is_suspicious(c: char) -> bool {
    match c {
        // Common in real text: tab, LF, CR, form feed (page breaks), ESC (ANSI logs), backspace.
        '\t' | '\n' | '\r' | '\u{c}' | '\u{1b}' | '\u{8}' => false,
        '\0'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{FFFD}' => true,
        _ => false,
    }
}

/// Whether decoded text is really binary: more than 2 % control/replacement chars in any of
/// three samples (start, middle, end).
pub fn looks_binary(s: &str) -> bool {
    let sample = |from: usize| -> bool {
        let mut i = from.min(s.len());
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        let (mut total, mut bad) = (0usize, 0usize);
        for c in s[i..].chars().take(SAMPLE_CHARS) {
            total += 1;
            bad += usize::from(is_suspicious(c));
        }
        total > 0 && bad * SUSPICIOUS_ONE_IN > total
    };
    let n = s.len();
    sample(0) || (n > 3 * SAMPLE_CHARS && (sample(n / 2) || sample(n - SAMPLE_CHARS)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rnd(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn utf8_and_bom_variants() {
        let d = decode("héllo\r\nwörld".as_bytes(), 1000).unwrap();
        assert_eq!((d.text.as_str(), d.encoding), ("héllo\r\nwörld", "UTF-8"));
        let d = decode(b"\xEF\xBB\xBFabc", 1000).unwrap();
        assert_eq!(d.text, "abc");
        let mut le = vec![0xFF, 0xFE];
        le.extend("hi €".encode_utf16().flat_map(u16::to_le_bytes));
        let d = decode(&le, 1000).unwrap();
        assert_eq!((d.text.as_str(), d.encoding), ("hi €", "UTF-16LE"));
        let be: Vec<u8> = "plain ascii text in utf-16be"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        let d = decode(&be, 1000).unwrap();
        assert_eq!(d.encoding, "UTF-16BE");
        assert_eq!(d.text, "plain ascii text in utf-16be");
        let mut u32le = vec![0xFF, 0xFE, 0, 0];
        u32le.extend("ab😀".chars().flat_map(|c| (c as u32).to_le_bytes()));
        let d = decode(&u32le, 1000).unwrap();
        assert_eq!((d.text.as_str(), d.encoding), ("ab😀", "UTF-32LE"));
    }

    #[test]
    fn legacy_encoding_is_detected() {
        // "Café crème brûlée, déjà vu" in windows-1252.
        let b = b"Caf\xe9 cr\xe8me br\xfbl\xe9e, d\xe9j\xe0 vu. Ceci est un texte fran\xe7ais.";
        let d = decode(b, 1000).unwrap();
        assert!(d.text.starts_with("Café crème"), "{}", d.text);
        assert_ne!(d.encoding, "UTF-8");
    }

    #[test]
    fn random_bytes_and_nul_are_binary() {
        for seed in 1..20 {
            let e = decode(&rnd(64 * 1024, seed), 1 << 20).unwrap_err();
            assert_eq!(e.code, WorkerFailure::Binary, "seed {seed}");
        }
        // Random bytes with every NUL removed still fail (control-char ratio).
        let no_nul: Vec<u8> = rnd(1 << 20, 99).into_iter().filter(|&b| b != 0).collect();
        assert_eq!(
            decode(&no_nul, 1 << 20).unwrap_err().code,
            WorkerFailure::Binary
        );
        assert_eq!(
            decode(b"abc\0def", 100).unwrap_err().code,
            WorkerFailure::Binary
        );
    }

    #[test]
    fn binary_tail_is_caught_by_the_end_sample() {
        let mut b = "real text line\n".repeat(10_000).into_bytes();
        let tail: Vec<u8> = rnd(200_000, 5).into_iter().filter(|&b| b != 0).collect();
        b.extend(tail);
        assert_eq!(decode(&b, 1 << 22).unwrap_err().code, WorkerFailure::Binary);
    }

    #[test]
    fn caps_cut_on_char_boundaries() {
        let s = "é".repeat(100);
        let d = decode(s.as_bytes(), 11).unwrap();
        assert_eq!(d.text, "é".repeat(5));
        assert!(d.truncated);
        let d = decode(&[b'a'; 10], 10).unwrap();
        assert!(!d.truncated);
    }

    #[test]
    fn a_stray_invalid_byte_in_a_big_utf8_file_stays_utf8() {
        let mut b = "ok ".repeat(10_000).into_bytes();
        b[5000] = 0xFF;
        let d = decode(&b, 1 << 20).unwrap();
        assert_eq!(d.encoding, "UTF-8");
        assert_eq!(d.text.matches('\u{FFFD}').count(), 1);
    }

    #[test]
    fn empty_is_ok() {
        let d = decode(b"", 10).unwrap();
        assert!(d.text.is_empty() && !d.truncated);
    }
}
