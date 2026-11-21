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
use crate::html4_tables::{self, DataMode};

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
        if bytes.starts_with(bom) {
            return Decoded {
                encoding: None,
                ..named(bytes, label, label)
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
/// reports as `reported`, after a byte order mark of that encoding, which
/// `xmlSwitchInputEncoding` skips. In UTF-8 an invalid sequence is an error
/// and becomes U+FFFD; in any other encoding the input ends there.
fn named(bytes: &[u8], name: &str, reported: &str) -> Decoded {
    let Some(codec) = Codec::named(name) else {
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
    let bytes = codec
        .bom()
        .and_then(|bom| bytes.strip_prefix(bom))
        .unwrap_or(bytes);
    let (text, invalid) = codec.decode(bytes);
    let severity = match codec {
        Codec::Utf8 => ErrorSeverity::Error,
        _ => ErrorSeverity::Fatal,
    };
    let diagnostics = invalid
        .map(|at| {
            diagnostic(
                severity,
                "Invalid bytes in character encoding".to_owned(),
                location(&text, at),
            )
        })
        .into_iter()
        .collect();
    Decoded {
        text,
        encoding: Some(reported.to_owned()),
        diagnostics,
    }
}

/// An encoding as libxml2 reads and writes it. Its own handlers cover
/// UTF-8, UTF-16, ISO-8859-1 and ASCII, where the WHATWG labels that
/// `encoding_rs` follows mean windows-1252 for the last two, and `encoding_rs`
/// writes UTF-16 as UTF-8. `encoding_rs` does the rest, as iconv does for
/// libxml2.
#[derive(Clone, Copy)]
pub enum Codec {
    Utf8,
    /// `bom` for libxml2's "UTF-16": little-endian, written after a byte
    /// order mark.
    Utf16 {
        big_endian: bool,
        bom: bool,
    },
    /// Each byte is the code point of that value.
    Latin1,
    Ascii,
    Other(&'static encoding_rs::Encoding),
}

impl Codec {
    /// The encoding called `name` or an alias of it, or `None` if libxml2
    /// wouldn't know it.
    pub fn named(name: &str) -> Option<Self> {
        let real = encoding_handler::resolve_alias(name);
        let label = real.as_deref().unwrap_or(name).trim();
        let is = |names: &[&str]| names.iter().any(|n| label.eq_ignore_ascii_case(n));
        let utf16 = |big_endian, bom| Codec::Utf16 { big_endian, bom };
        Some(if is(&["UTF-8", "UTF8"]) {
            Codec::Utf8
        } else if is(&["UTF-16", "UTF16"]) {
            utf16(false, true)
        } else if is(&["UTF-16LE", "UTF16LE"]) {
            utf16(false, false)
        } else if is(&["UTF-16BE", "UTF16BE"]) {
            utf16(true, false)
        } else if is(&[
            "ISO-8859-1",
            "ISO8859-1",
            "ISO_8859-1",
            "ISO_8859-1:1987",
            "ISO88591",
            "ISO-IR-100",
            "LATIN1",
            "L1",
            "CP819",
            "IBM819",
            "CSISOLATIN1",
        ]) {
            Codec::Latin1
        } else if is(&["US-ASCII", "ASCII", "ANSI_X3.4-1968", "ISO646-US"]) {
            Codec::Ascii
        } else {
            Codec::Other(encoding_rs::Encoding::for_label(label.as_bytes())?)
        })
    }

    /// The byte order mark input in this encoding may start with.
    fn bom(self) -> Option<&'static [u8]> {
        match self {
            Codec::Utf8 => Some(b"\xEF\xBB\xBF"),
            Codec::Utf16 { big_endian, .. } => {
                Some(if big_endian { b"\xFE\xFF" } else { b"\xFF\xFE" })
            }
            _ => None,
        }
    }

    /// `bytes` as text, and the offset in the text of the first invalid
    /// sequence, if any: in UTF-8 it becomes U+FFFD, and elsewhere the text
    /// ends there.
    fn decode(self, bytes: &[u8]) -> (String, Option<usize>) {
        let encoding = match self {
            Codec::Utf8 => {
                let invalid = std::str::from_utf8(bytes).err().map(|e| e.valid_up_to());
                return (String::from_utf8_lossy(bytes).into_owned(), invalid);
            }
            Codec::Latin1 => return (latin1(bytes), None),
            Codec::Ascii => {
                let invalid = bytes.iter().position(|b| !b.is_ascii());
                return (latin1(&bytes[..invalid.unwrap_or(bytes.len())]), invalid);
            }
            Codec::Utf16 { big_endian, .. } => {
                if big_endian {
                    encoding_rs::UTF_16BE
                } else {
                    encoding_rs::UTF_16LE
                }
            }
            Codec::Other(encoding) => encoding,
        };
        let mut decoder = encoding.new_decoder_without_bom_handling();
        let capacity = decoder
            .max_utf8_buffer_length_without_replacement(bytes.len())
            .unwrap_or(bytes.len());
        let mut text = String::with_capacity(capacity);
        let (result, _) = decoder.decode_to_string_without_replacement(bytes, &mut text, true);
        let invalid =
            matches!(result, encoding_rs::DecoderResult::Malformed(..)).then_some(text.len());
        (text, invalid)
    }

    /// `text` in this encoding, each character it can't represent as a
    /// decimal character reference, as libxml2 writes them.
    pub fn encode(self, text: &str) -> Vec<u8> {
        let limit = match self {
            Codec::Utf8 => return text.as_bytes().to_vec(),
            // encoding_rs writes unmappable characters as decimal
            // references too.
            Codec::Other(encoding) => return encoding.encode(text).0.into_owned(),
            Codec::Utf16 { big_endian, bom } => {
                let mut out = Vec::with_capacity(text.len() * 2 + 2);
                if bom {
                    out.extend_from_slice(b"\xFF\xFE");
                }
                for unit in text.encode_utf16() {
                    let bytes = if big_endian {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    };
                    out.extend_from_slice(&bytes);
                }
                return out;
            }
            Codec::Latin1 => 0xFF,
            Codec::Ascii => 0x7F,
        };
        let mut out = Vec::with_capacity(text.len());
        for c in text.chars() {
            match u8::try_from(u32::from(c)) {
                Ok(byte) if u32::from(byte) <= limit => out.push(byte),
                _ => out.extend_from_slice(format!("&#{};", u32::from(c)).as_bytes()),
            }
        }
        out
    }
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// The charset the first `<meta>` element in `ascii` declares, as
/// `htmlCheckMeta` reads it from the tag the tokenizer gives it. Comments,
/// end tags and the content of raw-text elements (`script`, `title` and the
/// like) are skipped, as the tokenizer skips them.
fn meta_charset(ascii: &[u8]) -> Option<String> {
    let mut rest = std::str::from_utf8(ascii).ok()?;
    let after = |text: &'static str, rest: &str| -> usize {
        rest.find(text).map_or(rest.len(), |at| at + text.len())
    };
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        if let Some(comment) = rest.strip_prefix("!--") {
            rest = &comment[after("-->", comment)..];
        } else if rest.starts_with(['!', '?', '/']) {
            rest = &rest[after(">", rest)..];
        } else if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
            let (name, attributes, tail) = start_tag(rest);
            rest = tail;
            if name.eq_ignore_ascii_case("meta") {
                if let Some(charset) = meta_declaration(&attributes) {
                    return Some(charset.to_owned());
                }
                continue;
            }
            match html4_tables::lookup(name).map(|info| info.data_mode) {
                Some(DataMode::PlainText) => return None,
                Some(DataMode::Normal) | None => {}
                Some(_) => {
                    let end_tag = format!("</{}", name.to_ascii_lowercase());
                    let at = rest.to_ascii_lowercase().find(&end_tag);
                    rest = &rest[at.unwrap_or(rest.len())..];
                }
            }
        }
    }
    None
}

/// The name and attributes of the start tag `input` begins with (after its
/// `<`), as the tokenizer reads them, and the input after the tag. An
/// attribute without a value has none, rather than an empty one.
fn start_tag(input: &str) -> (&str, Vec<(&str, Option<&str>)>, &str) {
    let bytes = input.as_bytes();
    let skip = |from: usize, stop: &dyn Fn(u8) -> bool| {
        bytes[from..]
            .iter()
            .position(|&b| stop(b))
            .map_or(bytes.len(), |at| from + at)
    };
    let ends_name = |b: u8| b.is_ascii_whitespace() || b == b'/' || b == b'>';
    let name_end = skip(0, &ends_name);
    let mut attributes = Vec::new();
    let mut i = name_end;
    loop {
        i = skip(i, &|b| !(b.is_ascii_whitespace() || b == b'/'));
        if i >= bytes.len() || bytes[i] == b'>' {
            break;
        }
        // An attribute's name may start with "=".
        let attribute_end = skip(i + 1, &|b| ends_name(b) || b == b'=');
        let attribute = &input[i..attribute_end];
        i = skip(attribute_end, &|b| !b.is_ascii_whitespace());
        let mut value = None;
        if bytes.get(i) == Some(&b'=') {
            i = skip(i + 1, &|b| !b.is_ascii_whitespace());
            let (start, end, next) = match bytes.get(i) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let end = skip(i + 1, &|b| b == quote);
                    (i + 1, end, end + 1)
                }
                _ => {
                    let end = skip(i, &|b| b.is_ascii_whitespace() || b == b'>');
                    (i, end, end)
                }
            };
            value = Some(&input[start..end]);
            i = next.min(bytes.len());
        }
        attributes.push((attribute, value));
    }
    let tail = &input[(i + 1).min(input.len())..];
    (&input[..name_end], attributes, tail)
}

/// `htmlCheckMeta`: a `charset` attribute's value, or, if `http-equiv` is
/// Content-Type, what follows "charset=" in `content` (`htmlCheckEncoding`).
fn meta_declaration<'a>(attributes: &[(&str, Option<&'a str>)]) -> Option<&'a str> {
    let value = |wanted: &str| {
        attributes
            .iter()
            .find(|(name, value)| value.is_some() && name.eq_ignore_ascii_case(wanted))
            .and_then(|(_, value)| *value)
    };
    if let Some(charset) = value("charset") {
        return Some(charset);
    }
    if !value("http-equiv").is_some_and(|v| v.eq_ignore_ascii_case("Content-Type")) {
        return None;
    }
    let content = value("content")?;
    let at = content.to_ascii_lowercase().find("charset")? + "charset".len();
    let after = &content[at..];
    // libxml2 looks for the content's first "=" if a blank follows "charset".
    let after = if after.starts_with(|c: char| c.is_ascii_whitespace()) {
        &content[content.find('=')?..]
    } else {
        after
    };
    after.strip_prefix('=')
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
    fn meta_charset_comes_only_from_a_meta_element() {
        let found = |html: &[u8]| meta_charset(html);
        assert_eq!(
            found(b"<!-- <meta charset=a> --><meta charset=b>").as_deref(),
            Some("b")
        );
        assert_eq!(
            found(b"<script>'<meta charset=a>'</script><meta charset=b>").as_deref(),
            Some("b")
        );
        assert_eq!(
            found(b"<title><meta charset=a></TITLE><meta charset=b>").as_deref(),
            Some("b")
        );
        assert_eq!(
            found(b"<metadata charset=a><meta content=x charset='b c'>").as_deref(),
            Some("b c")
        );
        assert_eq!(found(b"<meta name=description content='charset=a'>"), None);
        assert_eq!(
            found(b"<meta charset><meta http-equiv=content-type content='x;charset=b'>").as_deref(),
            Some("b")
        );
        assert_eq!(found(b"<plaintext><meta charset=a>"), None);
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
