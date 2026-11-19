//! `Nokogiri::XML::Document`, mirroring `ext/nokogiri/xml_document.c`.

use magnus::{
    Error, Module, Object, RArray, RClass, RString, Ruby, Value, method, prelude::*,
    typed_data::Obj,
};
use xmloxide::Document;
use xmloxide::encoding::EncodingError;
use xmloxide::error::ParseError;
use xmloxide::parser::{ParseOptions, parse_str_with_options};

use crate::classes;
use crate::encoding_handler;
use crate::node::{DocKind, DocState, NodeObj, Receiver};
use crate::syntax_error;

/// Nokogiri's `ParseOptions` bits (libxml2's `xmlParserOption`) that the
/// backend acts on so far.
mod parse_options {
    pub const RECOVER: i64 = 1 << 0;
    pub const NOBLANKS: i64 = 1 << 8;
    pub const HUGE: i64 = 1 << 19;
}

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    let class = classes::get(ruby, &classes::XML_DOCUMENT);
    class.define_singleton_method("read_memory", method!(read_memory, 4))?;
    class.define_singleton_method("read_io", method!(read_io, 4))?;
    class.define_singleton_method("new", method!(new, -1))?;
    class.define_method("root", method!(root, 0))?;
    class.define_method("encoding", method!(encoding, 0))?;
    class.define_method("version", method!(version, 0))?;
    class.define_method("url", method!(url, 0))?;
    Ok(())
}

/// Wraps a tree as a new Ruby document of class `class`, the way
/// `noko_xml_document_wrap_with_init_args` does: `@decorators` and `@errors`
/// start nil, then `initialize(*init_args)` runs.
pub fn wrap(
    ruby: &Ruby,
    class: RClass,
    tree: Document,
    url: Option<String>,
    kind: DocKind,
    init_args: &[Value],
) -> Result<Value, Error> {
    let document = ruby.obj_wrap_as(NodeObj::new_document(DocState::new(tree, url, kind)), class);
    NodeObj::init_document(document);
    document.ivar_set("@decorators", ruby.qnil())?;
    document.ivar_set("@errors", ruby.qnil())?;
    let _: Value = document.funcall("initialize", init_args)?;
    Ok(document.as_value())
}

fn parser_options(options: i64) -> ParseOptions {
    let mut parse = ParseOptions::default()
        .recover(options & parse_options::RECOVER != 0)
        .no_blanks(options & parse_options::NOBLANKS != 0);
    if options & parse_options::HUGE != 0 {
        // libxml2's XML_PARSE_HUGE lifts its hard-coded limits.
        parse = parse
            .max_depth(2048)
            .max_text_length(1 << 30)
            .max_attribute_length(1 << 30)
            .max_name_length(1 << 30);
    }
    parse
}

/// Decodes `bytes` to UTF-8, the way `xmlReadMemory` does: an explicit
/// `encoding` overrides detection from the BOM and XML declaration. Names
/// registered with `EncodingHandler.alias` resolve to their encodings, both
/// for `encoding` and in the declaration.
fn decode(bytes: &[u8], encoding: Option<&str>) -> Result<String, EncodingError> {
    use xmloxide::encoding::{decode_to_utf8, transcode};
    if let Some(name) = encoding {
        let real = encoding_handler::resolve_alias(name);
        return transcode(bytes, real.as_deref().unwrap_or(name));
    }
    decode_to_utf8(bytes).or_else(|failure| {
        match declared_encoding(bytes).and_then(|d| encoding_handler::resolve_alias(&d)) {
            Some(real) => transcode(bytes, &real),
            None => Err(failure),
        }
    })
}

/// The `encoding` named in an ASCII-compatible XML declaration.
fn declared_encoding(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let head = &bytes[..bytes.len().min(200)];
    let decl = &head[..head.windows(2).position(|w| w == b"?>")?];
    if !decl.starts_with(b"<?xml") {
        return None;
    }
    let after = &decl[decl.windows(8).position(|w| w == b"encoding")? + 8..];
    let after = after
        .trim_ascii_start()
        .strip_prefix(b"=")?
        .trim_ascii_start();
    let (&quote, rest) = after.split_first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let value = &rest[..rest.iter().position(|&b| b == quote)?];
    String::from_utf8(value.to_vec()).ok()
}

/// Decodes and parses.
fn parse(bytes: &[u8], encoding: Option<&str>, options: i64) -> Result<Document, ParseError> {
    let text = decode(bytes, encoding).map_err(|e| ParseError {
        message: e.to_string(),
        location: Default::default(),
        diagnostics: Vec::new(),
    })?;
    let mut tree = parse_str_with_options(&text, &parser_options(options))?;
    if tree.encoding.is_none() {
        tree.encoding = encoding.map(str::to_owned);
    }
    Ok(tree)
}

/// Parses `input` into a new Ruby document. An `io_error` (a failed
/// `io.read`) is reported after the parse's own errors, as the fatal error
/// that stopped it.
fn read(
    ruby: &Ruby,
    class: RClass,
    input: &Input,
    url: Option<String>,
    encoding: Option<String>,
    options: i64,
) -> Result<Value, Error> {
    let parsed = parse(&input.bytes, encoding.as_deref(), options);
    let (tree, errors): (Option<Document>, RArray) = match parsed {
        Ok(tree) => {
            let errors = syntax_error::array(ruby, &tree.diagnostics)?;
            (Some(tree), errors)
        }
        Err(failure) => (None, syntax_error::array_for_failure(ruby, &failure)?),
    };
    if input.io_error {
        errors.push(syntax_error::io_error(ruby)?)?;
    }
    let tree = match tree {
        Some(tree) if !input.io_error => tree,
        _ => {
            if options & parse_options::RECOVER == 0 {
                return Err(syntax_error::raise_aggregate(ruby, errors));
            }
            // libxml2 would return a partial tree here. xmloxide gives up at
            // some errors even in recovery mode (see notes/PLAN.md), so fall
            // back to an empty document that still carries the errors.
            tree.unwrap_or_default()
        }
    };
    let document = wrap(ruby, class, tree, url, DocKind::Xml, &[])?;
    Obj::<NodeObj>::try_convert(document)?.ivar_set("@errors", errors)?;
    Ok(document)
}

/// `Document.read_memory(string, url, encoding, options)`
fn read_memory(
    ruby: &Ruby,
    class: RClass,
    input: RString,
    url: Option<String>,
    encoding: Option<String>,
    options: i64,
) -> Result<Value, Error> {
    // Copy the bytes out first: parsing must not hold a borrow of a Ruby string.
    let input = Input {
        bytes: unsafe { input.as_slice() }.to_vec(),
        io_error: false,
    };
    read(ruby, class, &input, url, encoding, options)
}

/// The bytes to parse, and whether reading them stopped on an IO error.
struct Input {
    bytes: Vec<u8>,
    io_error: bool,
}

/// libxml2's read size for `xmlReadIO` callbacks (`MINLEN` in xmlIO.c).
const READ_CHUNK: usize = 4000;

/// Reads `io` to the end the way `noko_io_read_protected` feeds libxml2:
/// `io.read(4000)` until it returns nil or an empty string. A
/// StandardError from `read`, or a result that isn't a String, is an IO
/// error that ends the input; any other exception propagates. Like the C
/// callback, a chunk longer than requested is truncated.
fn read_all(ruby: &Ruby, io: Value) -> Result<Input, Error> {
    let mut input = Input {
        bytes: Vec::new(),
        io_error: false,
    };
    loop {
        let chunk = match io.funcall::<_, _, Value>("read", (READ_CHUNK,)) {
            Ok(chunk) => chunk,
            Err(error) if error.is_kind_of(ruby.exception_standard_error()) => {
                input.io_error = true;
                return Ok(input);
            }
            Err(error) => return Err(error),
        };
        if chunk.is_nil() {
            return Ok(input);
        }
        let Some(chunk) = RString::from_value(chunk) else {
            input.io_error = true;
            return Ok(input);
        };
        // Copied before any further Ruby call.
        let bytes = unsafe { chunk.as_slice() };
        if bytes.is_empty() {
            return Ok(input);
        }
        input
            .bytes
            .extend_from_slice(&bytes[..bytes.len().min(READ_CHUNK)]);
    }
}

/// `Document.read_io(io, url, encoding, options)`. Reads the whole IO, then
/// parses.
fn read_io(
    ruby: &Ruby,
    class: RClass,
    io: Value,
    url: Option<String>,
    encoding: Option<String>,
    options: i64,
) -> Result<Value, Error> {
    let input = read_all(ruby, io)?;
    read(ruby, class, &input, url, encoding, options)
}

/// `Document.new(version = "1.0", *rest)`
fn new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let version = match args.first() {
        Some(v) if !v.is_nil() => String::try_convert(*v)?,
        _ => "1.0".to_owned(),
    };
    let mut tree = Document::new();
    tree.version = Some(version);
    wrap(ruby, class, tree, None, DocKind::Xml, args)
}

fn root(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let root = rcv.tree().root_element();
    rcv.wrap_opt(ruby, root)
}

fn encoding(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    Ok(Receiver::of(ruby, rb_self)?.tree().encoding.clone())
}

/// libxml2 always records a version, defaulting to "1.0".
fn version(ruby: &Ruby, rb_self: &NodeObj) -> Result<String, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let version = rcv
        .tree()
        .version
        .clone()
        .unwrap_or_else(|| "1.0".to_owned());
    Ok(version)
}

fn url(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    Ok(Receiver::of(ruby, rb_self)?.state().url.clone())
}
