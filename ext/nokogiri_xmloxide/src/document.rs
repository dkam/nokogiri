//! `Nokogiri::XML::Document`, mirroring `ext/nokogiri/xml_document.c`.

use magnus::{
    Error, Module, Object, RArray, RClass, RString, Ruby, Value, method, prelude::*,
    rb_sys::AsRawValue, typed_data::Obj,
};
use xmloxide::Document;
use xmloxide::encoding::EncodingError;
use xmloxide::error::ParseError;
use xmloxide::parser::{ParseOptions, parse_str_with_options};

use crate::classes;
use crate::edit::{CopyLevel, copy_tree_node};
use crate::encoding_handler;
use crate::node::{DocKind, DocState, NodeObj, Receiver, document_receiver};
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
    class.define_method("encoding=", method!(set_encoding, 1))?;
    class.define_method("root=", method!(set_root, 1))?;
    class.define_method(
        "initialize_copy_with_args",
        method!(initialize_copy_with_args, 2),
    )?;
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

pub fn parser_options(options: i64) -> ParseOptions {
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
    use xmloxide::encoding::{declared_encoding, decode_to_utf8, transcode};
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

/// `Document#encoding=(encoding)`
fn set_encoding(ruby: &Ruby, rb_self: &NodeObj, encoding: String) -> Result<String, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    rcv.tree_mut(ruby)?.encoding = Some(encoding.clone());
    Ok(encoding)
}

/// `Document#root=(root)`: replaces the root element; nil removes it. A
/// root from another document is copied in, as in C; the object passed
/// keeps referring to the original.
fn set_root(ruby: &Ruby, rb_self: &NodeObj, new_root: Value) -> Result<Value, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let attach = if new_root.is_nil() {
        None
    } else {
        if !new_root.is_kind_of(classes::get(ruby, &classes::NODE)) {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!(
                    "expected Nokogiri::XML::Node but received {}",
                    new_root.class().inspect()
                ),
            ));
        }
        let node = Receiver::of_value(ruby, new_root)?;
        let Some(id) = node.tree_id().filter(|id| node.tree().is_element(*id)) else {
            return Err(Error::new(
                ruby.exception_type_error(),
                "root must be a Nokogiri::XML::Element",
            ));
        };
        if node.doc_value.as_raw() == rcv.doc_value.as_raw() {
            Some(id)
        } else {
            Some(copy_tree_node(
                &mut *rcv.tree_mut(ruby)?,
                Some(&node.tree()),
                id,
                CopyLevel::Deep,
            ))
        }
    };
    let mut tree = rcv.tree_mut(ruby)?;
    if let Some(current) = tree.root_element() {
        tree.detach(current);
    }
    if let Some(id) = attach {
        tree.detach(id);
        let document_node = tree.root();
        tree.append_child(document_node, id);
    }
    Ok(new_root)
}

/// `Document#initialize_copy_with_args(other, level)`: makes this newly
/// allocated object (from `dup` or `clone`) a copy of document `other`;
/// level 0 copies the document without its children.
fn initialize_copy_with_args(
    ruby: &Ruby,
    rb_self: Obj<NodeObj>,
    other: Value,
    level: i64,
) -> Result<Obj<NodeObj>, Error> {
    let source = document_receiver(ruby, other)?;
    let tree = {
        let original = source.tree();
        if level == 0 {
            let mut empty = Document::new();
            empty.version.clone_from(&original.version);
            empty.encoding.clone_from(&original.encoding);
            empty.standalone = original.standalone;
            empty
        } else {
            original.clone()
        }
    };
    let state = DocState::new(tree, source.state().url.clone(), source.state().kind);
    if !NodeObj::init_document_copy(rb_self, state) {
        return Err(Error::new(
            ruby.exception_runtime_error(),
            "document is already initialized",
        ));
    }
    Ok(rb_self)
}
