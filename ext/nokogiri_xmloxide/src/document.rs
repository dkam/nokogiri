//! `Nokogiri::XML::Document`, mirroring `ext/nokogiri/xml_document.c`.

use magnus::{
    Error, Module, Object, RArray, RClass, RString, Ruby, Value, method, prelude::*,
    typed_data::Obj,
};
use xmloxide::Document;
use xmloxide::error::ParseError;
use xmloxide::parser::{ParseOptions, parse_str_with_options};

use crate::classes;
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

/// Decodes and parses, the way `xmlReadMemory` does: an explicit `encoding`
/// overrides detection from the BOM and XML declaration.
fn parse(bytes: &[u8], encoding: Option<&str>, options: i64) -> Result<Document, ParseError> {
    let decoded = match encoding {
        Some(name) => xmloxide::encoding::transcode(bytes, name),
        None => xmloxide::encoding::decode_to_utf8(bytes),
    };
    let text = decoded.map_err(|e| ParseError {
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

fn read(
    ruby: &Ruby,
    class: RClass,
    bytes: &[u8],
    url: Option<String>,
    encoding: Option<String>,
    options: i64,
) -> Result<Value, Error> {
    let (tree, errors): (Document, RArray) = match parse(bytes, encoding.as_deref(), options) {
        Ok(tree) => {
            let errors = syntax_error::array(ruby, &tree.diagnostics)?;
            (tree, errors)
        }
        Err(failure) => {
            let errors = syntax_error::array_for_failure(ruby, &failure)?;
            if options & parse_options::RECOVER == 0 {
                return Err(syntax_error::raise_aggregate(ruby, errors));
            }
            // libxml2 would return a partial tree here. xmloxide gives up at
            // some errors even in recovery mode (see notes/PLAN.md), so fall
            // back to an empty document that still carries the errors.
            (Document::new(), errors)
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
    let bytes = unsafe { input.as_slice() }.to_vec();
    read(ruby, class, &bytes, url, encoding, options)
}

/// `Document.read_io(io, url, encoding, options)`. Reads the whole IO, then
/// parses; errors raised by `io.read` propagate as they do from C.
fn read_io(
    ruby: &Ruby,
    class: RClass,
    io: Value,
    url: Option<String>,
    encoding: Option<String>,
    options: i64,
) -> Result<Value, Error> {
    let content: Option<RString> = io.funcall("read", ())?;
    let bytes = content
        .map(|s| unsafe { s.as_slice() }.to_vec())
        .unwrap_or_default();
    read(ruby, class, &bytes, url, encoding, options)
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
