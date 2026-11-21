//! `Nokogiri::XML::Document`, mirroring `ext/nokogiri/xml_document.c`.

use magnus::{
    Error, Exception, Module, Object, RArray, RClass, RString, Ruby, Value, method, prelude::*,
    rb_sys::AsRawValue, typed_data::Obj,
};
use xmloxide::encoding::EncodingError;
use xmloxide::error::ParseError;
use xmloxide::html::{HtmlParseOptions, parse_html_with_options};
use xmloxide::parser::{ParseOptions, parse_str_with_options};
use xmloxide::tree::NodeKind;
use xmloxide::{Document, NodeId};

use crate::classes;
use crate::edit::{CopyLevel, copy_tree_node};
use crate::encoding_handler;
use crate::html_encoding;
use crate::node::{DocKind, DocState, NodeObj, Receiver, document_receiver};
use crate::syntax_error;

/// Nokogiri's `ParseOptions` bits (libxml2's `xmlParserOption`) that the
/// backend acts on so far.
mod parse_options {
    pub const RECOVER: i64 = 1 << 0;
    /// `HTML_PARSE_NODEFDTD`: no default DOCTYPE for an HTML document without one.
    pub const NODEFDTD: i64 = 1 << 2;
    pub const NOBLANKS: i64 = 1 << 8;
    /// `HTML_PARSE_NOIMPLIED`: no implied html, head or body elements.
    pub const NOIMPLIED: i64 = 1 << 13;
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
    let node = classes::get(ruby, &classes::NODE);
    node.define_method("internal_subset", method!(internal_subset, 0))?;
    node.define_method("external_subset", method!(external_subset, 0))?;

    let html = classes::get(ruby, &classes::HTML4_DOCUMENT);
    html.define_singleton_method("new", method!(html_new, -1))?;
    html.define_singleton_method("read_memory", method!(html_read_memory, 4))?;
    html.define_singleton_method("read_io", method!(html_read_io, 4))?;
    html.define_method("type", method!(crate::node::node_type_of, 0))?;

    let dtd = classes::get(ruby, &classes::DTD);
    dtd.define_method("external_id", method!(dtd_external_id, 0))?;
    dtd.define_method("system_id", method!(dtd_system_id, 0))?;
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

/// libxml2 records a version for every XML document, defaulting to "1.0",
/// and none for HTML documents.
fn version(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let version = rcv.tree().version.clone();
    Ok(match rcv.state().kind {
        DocKind::Xml => Some(version.unwrap_or_else(|| "1.0".to_owned())),
        DocKind::Html => version,
    })
}

/// `HTML4::Document.read_memory(string, url, encoding, options)`
fn html_read_memory(
    ruby: &Ruby,
    class: RClass,
    input: RString,
    url: Option<String>,
    encoding: Option<String>,
    options: i64,
) -> Result<Value, Error> {
    // Copied out first, as for XML.
    let input = Input {
        bytes: unsafe { input.as_slice() }.to_vec(),
        io_error: false,
    };
    html_read(ruby, class, &input, url, encoding, options)
}

/// `HTML4::Document.read_io(io, url, encoding, options)`
fn html_read_io(
    ruby: &Ruby,
    class: RClass,
    io: Value,
    url: Option<String>,
    encoding: Option<String>,
    options: i64,
) -> Result<Value, Error> {
    let input = read_all(ruby, io)?;
    // EncodingReader raises EncodingFound through the IO when it finds a
    // meta charset, so that the caller can read again knowing it.
    if io.respond_to("encoding_found", false)? {
        let found: Value = io.funcall("encoding_found", ())?;
        if let Some(found) = Exception::from_value(found) {
            return Err(found.into());
        }
    }
    html_read(ruby, class, &input, url, encoding, options)
}

/// Parses HTML as `htmlReadMemory` does with libxml2's recovering HTML
/// parser. Unlike XML, a parse without RECOVER fails on any error at all.
fn html_read(
    ruby: &Ruby,
    class: RClass,
    input: &Input,
    url: Option<String>,
    encoding: Option<String>,
    options: i64,
) -> Result<Value, Error> {
    let decoded = html_encoding::decode(&input.bytes, encoding.as_deref());
    let html_options = HtmlParseOptions::default()
        .recover(options & parse_options::RECOVER != 0)
        .no_blanks(options & parse_options::NOBLANKS != 0)
        .no_implied(options & parse_options::NOIMPLIED != 0);
    let errors = ruby.ary_new();
    for diagnostic in &decoded.diagnostics {
        errors.push(syntax_error::from_diagnostic(ruby, diagnostic)?)?;
    }
    let tree = match parse_html_with_options(&decoded.text, &html_options) {
        Ok(tree) => {
            for diagnostic in &tree.diagnostics {
                errors.push(syntax_error::from_diagnostic(ruby, diagnostic)?)?;
            }
            Some(tree)
        }
        Err(failure) => {
            for error in syntax_error::array_for_failure(ruby, &failure)? {
                errors.push(error)?;
            }
            None
        }
    };
    if input.io_error {
        errors.push(syntax_error::io_error(ruby)?)?;
    }
    let mut tree = match tree {
        Some(tree) if options & parse_options::RECOVER != 0 || errors.is_empty() => tree,
        _ => return Err(syntax_error::raise_without_recover(ruby, errors)),
    };
    tree.encoding = decoded.encoding;
    if options & parse_options::NODEFDTD == 0 && doctype(&tree).is_none() {
        let dtd = default_doctype(&mut tree);
        let document_node = tree.root();
        tree.prepend_child(document_node, dtd);
    }
    let document = wrap(ruby, class, tree, url, DocKind::Html, &[])?;
    Obj::<NodeObj>::try_convert(document)?.ivar_set("@errors", errors)?;
    Ok(document)
}

/// The DOCTYPE `htmlNewDoc` and `htmlParseDocument` give an HTML document
/// that has none: HTML 4.0 Transitional's.
fn default_doctype(tree: &mut Document) -> NodeId {
    tree.create_node(NodeKind::DocumentType {
        name: "html".to_owned(),
        public_id: Some("-//W3C//DTD HTML 4.0 Transitional//EN".to_owned()),
        system_id: Some("http://www.w3.org/TR/REC-html40/loose.dtd".to_owned()),
        internal_subset: None,
    })
}

/// `HTML4::Document.new(uri = nil, external_id = nil, *rest)`, as
/// `htmlNewDoc` makes it: an empty HTML document whose internal subset is
/// `html` with the given identifiers, or HTML 4.0 Transitional's if neither
/// is given.
fn html_new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let string_at = |index: usize| -> Result<Option<String>, Error> {
        match args.get(index) {
            Some(value) if value.to_bool() => Ok(Some(RString::try_convert(*value)?.to_string()?)),
            _ => Ok(None),
        }
    };
    let (system_id, public_id) = match (string_at(0)?, string_at(1)?) {
        (None, None) => (
            Some("http://www.w3.org/TR/REC-html40/loose.dtd".to_owned()),
            Some("-//W3C//DTD HTML 4.0 Transitional//EN".to_owned()),
        ),
        ids => ids,
    };
    let mut tree = Document::new();
    let doctype = tree.create_node(NodeKind::DocumentType {
        name: "html".to_owned(),
        public_id,
        system_id,
        internal_subset: None,
    });
    let document_node = tree.root();
    tree.append_child(document_node, doctype);
    wrap(ruby, class, tree, None, DocKind::Html, args)
}

/// The document's DOCTYPE node, if it has one (libxml2's `xmlGetIntSubset`).
pub fn doctype(tree: &Document) -> Option<NodeId> {
    tree.children(tree.root())
        .find(|id| matches!(tree.node(*id).kind, NodeKind::DocumentType { .. }))
}

/// `Node#internal_subset`: the document's DTD node, or nil.
fn internal_subset(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let id = doctype(&rcv.tree());
    rcv.wrap_opt(ruby, id)
}

/// `Node#external_subset`: xmloxide doesn't load external subsets.
fn external_subset(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    Receiver::of(ruby, rb_self)?;
    Ok(None)
}

/// One of a DTD node's identifiers.
fn dtd_id(
    ruby: &Ruby,
    rb_self: &NodeObj,
    pick: fn(&NodeKind) -> Option<&String>,
) -> Result<Option<String>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let tree = rcv.tree();
    Ok(rcv
        .tree_id()
        .and_then(|id| pick(&tree.node(id).kind).cloned()))
}

/// `DTD#external_id`: the public identifier.
fn dtd_external_id(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    dtd_id(ruby, rb_self, |kind| match kind {
        NodeKind::DocumentType { public_id, .. } => public_id.as_ref(),
        _ => None,
    })
}

/// `DTD#system_id`.
fn dtd_system_id(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    dtd_id(ruby, rb_self, |kind| match kind {
        NodeKind::DocumentType { system_id, .. } => system_id.as_ref(),
        _ => None,
    })
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
