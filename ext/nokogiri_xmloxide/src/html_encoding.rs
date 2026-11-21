//! How libxml2's HTML parser decides what encoding a document is in, for
//! `HTML4::Document.read_memory` and `read_io`.
//!
//! - An encoding the caller names wins. One libxml2 doesn't know is a
//!   warning, and the input is read as ISO-8859-1.
//! - Otherwise a byte order mark decides, and the document reports no
//!   encoding.
//! - Otherwise a `<meta>` charset before the first non-ASCII byte does, as
//!   libxml2 switches to it when it parses the tag.
//! - Otherwise ISO-8859-1, which libxml2 falls back to at the first
//!   non-ASCII byte. An all-ASCII document reports no encoding.

use xmloxide::error::{ErrorSeverity, ParseDiagnostic, SourceLocation};

use crate::encoding_handler;

/// The input as text, what `Document#encoding` reports, and any problem
/// decoding it.
pub struct Decoded {
    pub text: String,
    pub encoding: Option<String>,
    pub diagnostics: Vec<ParseDiagnostic>,
}

pub fn decode(bytes: &[u8], encoding: Option<&str>) -> Decoded {
    if let Some(name) = encoding {
        return named(bytes, name, name);
    }
    for (bom, label) in [
        (&b"\xEF\xBB\xBF"[..], "UTF-8"),
        (b"\xFF\xFE", "UTF-16LE"),
        (b"\xFE\xFF", "UTF-16BE"),
    ] {
        if let Some(rest) = bytes.strip_prefix(bom) {
            let decoded = named(rest, label, label);
            return Decoded {
                encoding: None,
                ..decoded
            };
        }
    }
    let ascii = bytes
        .iter()
        .position(|b| !b.is_ascii())
        .unwrap_or(bytes.len());
    if let Some(charset) = meta_charset(&bytes[..ascii]) {
        return named(bytes, &charset, &charset);
    }
    if ascii == bytes.len() {
        return Decoded {
            text: latin1(bytes),
            encoding: None,
            diagnostics: Vec::new(),
        };
    }
    Decoded {
        text: latin1(bytes),
        encoding: Some("ISO-8859-1".to_owned()),
        diagnostics: Vec::new(),
    }
}

/// Decodes `bytes` as the encoding called `name`, which the document
/// reports as `reported`.
fn named(bytes: &[u8], name: &str, reported: &str) -> Decoded {
    let real = encoding_handler::resolve_alias(name);
    let label = real.as_deref().unwrap_or(name);
    if is_latin1(label) {
        return Decoded {
            text: latin1(bytes),
            encoding: Some(reported.to_owned()),
            diagnostics: Vec::new(),
        };
    }
    let Some(encoding) = encoding_rs::Encoding::for_label(label.trim().as_bytes()) else {
        return Decoded {
            text: latin1(bytes),
            encoding: Some("ISO-8859-1".to_owned()),
            diagnostics: vec![diagnostic(
                ErrorSeverity::Warning,
                format!("Unsupported encoding: {name}"),
                SourceLocation::default(),
            )],
        };
    };
    let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
    let mut diagnostics = Vec::new();
    if had_errors {
        let at = text.find('\u{FFFD}').unwrap_or(0);
        diagnostics.push(diagnostic(
            ErrorSeverity::Error,
            "Invalid bytes in character encoding".to_owned(),
            location(&text, at),
        ));
    }
    Decoded {
        text: text.into_owned(),
        encoding: Some(reported.to_owned()),
        diagnostics,
    }
}

/// libxml2's ISO-8859-1 maps every byte to the code point of that value,
/// where the WHATWG label (and so `encoding_rs`) means windows-1252.
fn is_latin1(label: &str) -> bool {
    ["ISO-8859-1", "ISO8859-1", "ISO_8859-1", "LATIN1", "L1"]
        .iter()
        .any(|name| label.trim().eq_ignore_ascii_case(name))
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// The charset a `<meta>` tag in `ascii` declares, in its `charset`
/// attribute or in a `content` attribute's `charset=` parameter.
fn meta_charset(ascii: &[u8]) -> Option<String> {
    let original = std::str::from_utf8(ascii).ok()?;
    let lower = original.to_ascii_lowercase();
    let skip_space = |mut i: usize, end: usize| {
        while i < end && ascii[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut from = 0;
    while let Some(start) = lower[from..].find("<meta").map(|i| i + from) {
        let end = lower[start..].find('>').map_or(lower.len(), |i| i + start);
        if let Some(at) = lower[start..end].find("charset") {
            let i = skip_space(start + at + "charset".len(), end);
            if i < end && ascii[i] == b'=' {
                let mut i = skip_space(i + 1, end);
                while i < end && matches!(ascii[i], b'"' | b'\'') {
                    i += 1;
                }
                let len = ascii[i..end]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(b))
                    .count();
                if len > 0 {
                    return Some(original[i..i + len].to_owned());
                }
            }
        }
        from = end;
    }
    None
}

fn diagnostic(
    severity: ErrorSeverity,
    message: String,
    location: SourceLocation,
) -> ParseDiagnostic {
    ParseDiagnostic {
        severity,
        message,
        location,
    }
}

/// The 1-based line and column of byte offset `at` in `text`.
fn location(text: &str, at: usize) -> SourceLocation {
    let before = &text[..at];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    SourceLocation {
        line: u32::try_from(line).unwrap_or(u32::MAX),
        column: u32::try_from(column).unwrap_or(u32::MAX),
        byte_offset: at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_charset_is_found_in_either_form() {
        assert_eq!(
            meta_charset(br#"<meta charset="utf-8">"#).as_deref(),
            Some("utf-8")
        );
        assert_eq!(
            meta_charset(
                br#"<META HTTP-EQUIV="Content-Type" CONTENT="text/html; Charset=Shift_JIS">"#
            )
            .as_deref(),
            Some("Shift_JIS")
        );
        assert_eq!(meta_charset(b"<meta name=x><p>charset=utf-8"), None);
    }

    #[test]
    fn undeclared_non_ascii_is_latin1() {
        let decoded = decode(b"<p>caf\xC3\xA9", None);
        assert_eq!(decoded.text, "<p>caf\u{C3}\u{A9}");
        assert_eq!(decoded.encoding.as_deref(), Some("ISO-8859-1"));
        assert_eq!(decode(b"<p>x", None).encoding, None);
        assert_eq!(decode(b"<p>\x80", None).text, "<p>\u{80}");
    }

    #[test]
    fn bad_bytes_in_a_named_encoding_are_an_error() {
        let decoded = decode(b"<p>caf\xE9", Some("UTF-8"));
        assert_eq!(decoded.text, "<p>caf\u{FFFD}");
        assert_eq!(decoded.diagnostics.len(), 1);
        let unknown = decode(b"<p>caf\xE9", Some("bogus-enc"));
        assert_eq!(unknown.text, "<p>caf\u{E9}");
        assert_eq!(unknown.encoding.as_deref(), Some("ISO-8859-1"));
    }
}
