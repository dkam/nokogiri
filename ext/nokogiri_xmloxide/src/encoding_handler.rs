//! `Nokogiri::EncodingHandler`, mirroring `ext/nokogiri/xml_encoding_handler.c`.
//!
//! libxml2 keeps a process-wide table of encoding aliases; this keeps the same
//! table in Rust. A handler exists for a name when xmloxide can decode that
//! encoding (after resolving aliases), so lookups reflect what the parser can
//! actually do rather than a separate list.

use std::borrow::Cow;
use std::io::Write as _;
use std::sync::Mutex;

use magnus::{Error, Module, Object, RClass, RModule, Ruby, function, method};

/// Alias table: (alias in ASCII uppercase, real encoding name). libxml2
/// uppercases aliases on insert and lookup, so matching is case-insensitive.
static ALIASES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

#[magnus::wrap(
    class = "Nokogiri::EncodingHandler",
    free_immediately,
    frozen_shareable
)]
struct EncodingHandler {
    name: String,
}

impl EncodingHandler {
    /// `EncodingHandler#name`: the resolved encoding name.
    fn name(&self) -> String {
        self.name.clone()
    }
}

pub fn init(ruby: &Ruby, nokogiri: RModule) -> Result<(), Error> {
    let class = nokogiri.define_class("EncodingHandler", ruby.class_object())?;
    class.define_singleton_method("[]", function!(get, 1))?;
    class.define_singleton_method("delete", function!(delete, 1))?;
    class.define_singleton_method("alias", function!(add_alias, 2))?;
    class.define_singleton_method("clear_aliases!", method!(clear_aliases, 0))?;
    class.define_method("name", method!(EncodingHandler::name, 0))?;
    Ok(())
}

/// `EncodingHandler.[](name)`: the handler for `name`, or nil.
fn get(name: String) -> Option<EncodingHandler> {
    let resolved = resolve_alias(&name).unwrap_or(name);
    is_decodable(&resolved).then_some(EncodingHandler { name: resolved })
}

/// `EncodingHandler.alias(real_name, alias_name)`: returns `alias_name`.
fn add_alias(real_name: String, alias_name: String) -> String {
    let key = alias_name.to_ascii_uppercase();
    let mut aliases = lock_aliases();
    match aliases.iter_mut().find(|(a, _)| *a == key) {
        Some(entry) => entry.1 = real_name,
        None => aliases.push((key, real_name)),
    }
    alias_name
}

/// `EncodingHandler.delete(name)`: true if an alias was removed, else nil.
fn delete(alias_name: String) -> Option<bool> {
    let key = alias_name.to_ascii_uppercase();
    let mut aliases = lock_aliases();
    let before = aliases.len();
    aliases.retain(|(a, _)| *a != key);
    (aliases.len() < before).then_some(true)
}

/// `EncodingHandler.clear_aliases!`: removes every alias; returns the class.
fn clear_aliases(class: RClass) -> RClass {
    lock_aliases().clear();
    class
}

/// The encoding `name` is an alias for, if it is one.
pub fn resolve_alias(name: &str) -> Option<String> {
    let key = name.to_ascii_uppercase();
    lock_aliases()
        .iter()
        .find(|(a, _)| *a == key)
        .map(|(_, real)| real.clone())
}

/// True when libxml2 knows the encoding `name`.
fn is_decodable(name: &str) -> bool {
    Codec::named(name).is_some()
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
        let real = resolve_alias(name);
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
    pub fn bom(self) -> Option<&'static [u8]> {
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
    /// ends there. Unless `last`, a character the end of `bytes` cuts off
    /// is left out, not invalid: the bytes after may complete it.
    pub fn decode(self, bytes: &[u8], last: bool) -> (Cow<'_, str>, Option<usize>) {
        let encoding = match self {
            Codec::Utf8 => {
                return match std::str::from_utf8(bytes) {
                    Ok(text) => (Cow::Borrowed(text), None),
                    Err(error) if !last && error.error_len().is_none() => {
                        let complete = &bytes[..error.valid_up_to()];
                        (String::from_utf8_lossy(complete), None)
                    }
                    Err(error) => (String::from_utf8_lossy(bytes), Some(error.valid_up_to())),
                };
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
        let (result, _) = decoder.decode_to_string_without_replacement(bytes, &mut text, last);
        let invalid =
            matches!(result, encoding_rs::DecoderResult::Malformed(..)).then_some(text.len());
        (Cow::Owned(text), invalid)
    }

    /// `text` in this encoding, each character it can't represent as a
    /// decimal character reference, as libxml2 writes them.
    pub fn encode(self, text: String) -> Vec<u8> {
        let limit = match self {
            Codec::Utf8 => return text.into_bytes(),
            // encoding_rs writes unmappable characters as decimal
            // references too, and borrows text it leaves as it is.
            Codec::Other(encoding) => {
                return match encoding.encode(&text).0 {
                    Cow::Borrowed(_) => text.into_bytes(),
                    Cow::Owned(bytes) => bytes,
                };
            }
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
            _ if text.is_ascii() => return text.into_bytes(),
            Codec::Latin1 => 0xFF,
            Codec::Ascii => 0x7F,
        };
        let mut out = Vec::with_capacity(text.len());
        for c in text.chars() {
            match u8::try_from(u32::from(c)) {
                Ok(byte) if u32::from(byte) <= limit => out.push(byte),
                _ => {
                    let _ = write!(out, "&#{};", u32::from(c));
                }
            }
        }
        out
    }
}

/// The table holds plain strings, so a panic mid-update can't leave it
/// inconsistent; recover from poisoning rather than propagate it.
fn lock_aliases() -> std::sync::MutexGuard<'static, Vec<(String, String)>> {
    ALIASES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// libxml2's error for input that is invalid in its encoding.
pub const INVALID_BYTES: &str = "Invalid bytes in character encoding";

fn latin1(bytes: &[u8]) -> Cow<'_, str> {
    encoding_rs::mem::decode_latin1(bytes)
}
