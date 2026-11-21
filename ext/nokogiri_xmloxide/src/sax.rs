//! The SAX parsers: `XML::SAX::Parser`, `ParserContext` and `PushParser`
//! and their HTML4 subclasses, mirroring `ext/nokogiri/xml_sax_*.c` and
//! `html4_sax_*.c`. A context holds the input; `parse_with` parses it with
//! xmloxide's XML or HTML parser and calls the SAX parser's document for
//! each event the parser reports.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};

use magnus::{
    Error, Module, Object, RClass, RObject, Ruby, Value, method, prelude::*, typed_data::Obj,
    value::LazyId,
};
use xmloxide::encoding::{declared_encoding, detect_encoding};
use xmloxide::error::{ErrorSeverity, ParseDiagnostic, ParseError, SourceLocation};
use xmloxide::events::{Event, EventHandler};

use crate::classes;
use crate::document::parse_options::{NOENT, RECOVER};
use crate::document::{
    Input, html_parser_options, libxml2_html_diagnostic, parser_options, read_all,
};
use crate::edit::check_string;
use crate::encoding_handler::{Codec, INVALID_BYTES};
use crate::html_encoding;
use crate::syntax_error::{self, UNKNOWN_IO_ERROR};

#[derive(Clone, Copy, Default, PartialEq)]
enum Language {
    #[default]
    Xml,
    Html,
}

impl Language {
    /// The libxml2 parse options to parse with, given `options`: an HTML
    /// parser always recovers.
    fn options(self, options: i64) -> i64 {
        match self {
            Language::Xml => options,
            Language::Html => options | RECOVER,
        }
    }

    /// `bytes` as text, and the problem decoding them, if there was one.
    /// Unless `last`, a character the end of `bytes` cuts off is left out.
    fn decode<'b>(
        self,
        bytes: &'b [u8],
        encoding: Option<&str>,
        options: i64,
        last: bool,
    ) -> (Cow<'b, str>, Option<InputError>) {
        match self {
            Language::Xml => xml_decode(bytes, encoding, options & RECOVER != 0, last),
            Language::Html => html_decode(bytes, encoding, last),
        }
    }

    /// Parses `text` with `options`, reporting to `handler`.
    fn parse(
        self,
        text: &str,
        options: i64,
        handler: &mut dyn EventHandler,
    ) -> Result<(), ParseError> {
        match self {
            Language::Xml => xmloxide::events::parse_str(text, &parser_options(options), handler),
            Language::Html => {
                xmloxide::events::parse_html(text, &html_parser_options(options), handler)
            }
        }
    }
}

/// The input and options for one parse.
#[magnus::wrap(class = "Nokogiri::XML::SAX::ParserContext", free_immediately)]
struct ParserContext {
    /// The input, or `None` for an IO, which is read when parsing starts
    /// and is held in the context's `@io` so that it is marked.
    bytes: Option<Vec<u8>>,
    /// The encoding the caller named, if any.
    encoding: Option<String>,
    recovery: Cell<bool>,
    replace_entities: Cell<bool>,
    /// Where the parser is: `line` and `column` report it.
    location: Cell<SourceLocation>,
}

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    for parser in ["Nokogiri::XML::SAX::Parser", "Nokogiri::HTML4::SAX::Parser"] {
        let parser: RClass = ruby.eval(parser)?;
        parser.define_private_method("initialize_native", method!(initialize_native, 0))?;
    }

    let context: RClass = ruby.eval("Nokogiri::XML::SAX::ParserContext")?;
    context.undef_default_alloc_func();
    context.define_singleton_method("native_memory", method!(native_memory, 2))?;
    context.define_singleton_method("native_io", method!(native_io, 2))?;
    context.define_singleton_method("native_file", method!(native_file, 2))?;
    context.define_method("parse_with", method!(parse_with, 1))?;
    context.define_method("recovery", method!(recovery, 0))?;
    context.define_method("recovery=", method!(set_recovery, 1))?;
    context.define_method("replace_entities", method!(replace_entities, 0))?;
    context.define_method("replace_entities=", method!(set_replace_entities, 1))?;
    context.define_method("line", method!(line, 0))?;
    context.define_method("column", method!(column, 0))?;

    let html_context: RClass = ruby.eval("Nokogiri::HTML4::SAX::ParserContext")?;
    html_context.define_singleton_method("native_memory", method!(html_native_memory, 2))?;
    html_context.define_singleton_method("native_file", method!(html_native_file, 2))?;
    html_context.define_method("parse_with", method!(html_parse_with, 1))?;

    let push_parser: RClass = ruby.eval("Nokogiri::XML::SAX::PushParser")?;
    push_parser.define_alloc_func::<PushParser>();
    push_parser.define_private_method("initialize_native", method!(push_initialize, 2))?;
    push_parser.define_private_method("native_write", method!(push_write, 2))?;
    push_parser.define_method("options", method!(push_options, 0))?;
    push_parser.define_method("options=", method!(set_push_options, 1))?;
    push_parser.define_method("replace_entities", method!(push_replace_entities, 0))?;
    push_parser.define_method("replace_entities=", method!(set_push_replace_entities, 1))?;

    let html_push_parser: RClass = ruby.eval("Nokogiri::HTML4::SAX::PushParser")?;
    html_push_parser
        .define_private_method("initialize_native", method!(html_push_initialize, 3))?;
    html_push_parser.define_private_method("native_write", method!(push_write, 2))?;
    Ok(())
}

/// `Parser#initialize_native`: the C extension sets up libxml2's handler
/// here. Here `parse_with` reads `@document` when it starts.
fn initialize_native(rb_self: Value) -> Value {
    rb_self
}

/// `ParserContext.native_memory(string, encoding)`
fn native_memory(
    ruby: &Ruby,
    class: RClass,
    input: Value,
    encoding: Value,
) -> Result<Obj<ParserContext>, Error> {
    let input = Input::from_string(check_string(ruby, input)?);
    if input.bytes.is_empty() {
        return Err(Error::new(
            ruby.exception_runtime_error(),
            "input string cannot be empty",
        ));
    }
    new_context(ruby, class, Some(input.bytes), encoding)
}

/// `ParserContext.native_io(io, encoding)`
fn native_io(
    ruby: &Ruby,
    class: RClass,
    io: Value,
    encoding: Value,
) -> Result<Obj<ParserContext>, Error> {
    if !io.respond_to("read", false)? {
        return Err(Error::new(
            ruby.exception_type_error(),
            "argument expected to respond to :read",
        ));
    }
    let context = new_context(ruby, class, None, encoding)?;
    context.ivar_set("@io", io)?;
    Ok(context)
}

/// `ParserContext.native_file(path, encoding)`
fn native_file(
    ruby: &Ruby,
    class: RClass,
    path: String,
    encoding: Value,
) -> Result<Obj<ParserContext>, Error> {
    let bytes = std::fs::read(&path).map_err(|_| {
        Error::new(
            ruby.exception_runtime_error(),
            "failed to create xml sax parser context",
        )
    })?;
    new_context(ruby, class, Some(bytes), encoding)
}

/// `HTML4::SAX::ParserContext.native_memory(string, encoding)`: as for XML,
/// recovering, as an HTML parser does.
fn html_native_memory(
    ruby: &Ruby,
    class: RClass,
    input: Value,
    encoding: Value,
) -> Result<Obj<ParserContext>, Error> {
    let context = native_memory(ruby, class, input, encoding)?;
    context.recovery.set(true);
    Ok(context)
}

/// `HTML4::SAX::ParserContext.native_file(path, encoding)`
fn html_native_file(
    ruby: &Ruby,
    class: RClass,
    path: String,
    encoding: Value,
) -> Result<Obj<ParserContext>, Error> {
    let context = native_file(ruby, class, path, encoding)?;
    context.recovery.set(true);
    Ok(context)
}

fn new_context(
    ruby: &Ruby,
    class: RClass,
    bytes: Option<Vec<u8>>,
    encoding: Value,
) -> Result<Obj<ParserContext>, Error> {
    let encoding = if encoding.is_nil() {
        None
    } else if encoding.is_kind_of(ruby.class_encoding()) {
        Some(encoding.funcall::<_, _, String>("name", ())?)
    } else {
        return Err(Error::new(
            ruby.exception_type_error(),
            "argument must be an Encoding object",
        ));
    };
    if encoding
        .as_deref()
        .is_some_and(|name| Codec::named(name).is_none())
    {
        return Err(Error::new(
            ruby.exception_runtime_error(),
            "could not set encoding",
        ));
    }
    let context = ParserContext {
        bytes,
        encoding,
        recovery: Cell::new(false),
        replace_entities: Cell::new(false),
        location: Cell::new(SourceLocation {
            line: 1,
            column: 1,
            byte_offset: 0,
        }),
    };
    Ok(ruby.obj_wrap_as(context, class))
}

/// `ParserContext#parse_with(sax_parser)`: parses the input as XML, calling
/// the parser's document for each event, and returns nil. An exception
/// from a callback ends the parse and propagates.
fn parse_with(ruby: &Ruby, rb_self: Obj<ParserContext>, parser: Value) -> Result<(), Error> {
    parse_as(ruby, &rb_self, parser, Language::Xml)
}

/// `HTML4::SAX::ParserContext#parse_with(sax_parser)`: as for XML, parsing
/// the input as HTML.
fn html_parse_with(ruby: &Ruby, rb_self: Obj<ParserContext>, parser: Value) -> Result<(), Error> {
    parse_as(ruby, &rb_self, parser, Language::Html)
}

fn parse_as(
    ruby: &Ruby,
    context: &Obj<ParserContext>,
    parser: Value,
    language: Language,
) -> Result<(), Error> {
    if !parser.is_kind_of(classes::get(ruby, &classes::XML_SAX_PARSER)) {
        return Err(Error::new(
            ruby.exception_arg_error(),
            "argument must be a Nokogiri::XML::SAX::Parser",
        ));
    }
    let document: Value = RObject::try_convert(parser)?.ivar_get("@document")?;
    let (bytes, io_error) = match &context.bytes {
        Some(bytes) => (Cow::Borrowed(bytes.as_slice()), false),
        None => {
            let input = read_all(ruby, context.ivar_get("@io")?)?;
            (Cow::Owned(input.bytes), input.io_error)
        }
    };
    let mut options = if context.recovery.get() { RECOVER } else { 0 };
    if context.replace_entities.get() {
        options |= NOENT;
    }
    let options = language.options(options);
    let (text, input_error) = language.decode(&bytes, context.encoding.as_deref(), options, true);
    let mut handler = Callbacks::new(ruby, document, language, options, &context.location);
    handler.input_error = input_error.or_else(|| io_error.then(|| InputError::io(text.len())));
    // The parser's errors have gone to the document's callbacks.
    let _ = language.parse(&text, options, &mut handler);
    handler.raised.map_or(Ok(()), Err)
}

/// A problem with the input at a point in it, which libxml2 reports there:
/// a byte invalid in the input's encoding, an unsupported encoding, or a
/// failed read.
struct InputError {
    at: usize,
    severity: ErrorSeverity,
    message: String,
}

impl InputError {
    /// A failed read, which ends the input at `at`.
    fn io(at: usize) -> Self {
        InputError {
            at,
            severity: ErrorSeverity::Fatal,
            message: UNKNOWN_IO_ERROR.trim_end().to_owned(),
        }
    }

    /// Whether the input ends there. Recovering from an invalid byte, the
    /// parser carries on.
    fn halts(&self) -> bool {
        self.severity == ErrorSeverity::Fatal
    }
}

/// XML input as text, and the error for its first byte that is invalid in
/// its encoding, if it has one: without recovery the input ends there, and
/// with it UTF-8 input carries on with U+FFFD. As in libxml2, the encoding
/// is the one the caller named, else the one a byte order mark or the XML
/// declaration gives, else UTF-8.
fn xml_decode<'b>(
    bytes: &'b [u8],
    encoding: Option<&str>,
    recover: bool,
    last: bool,
) -> (Cow<'b, str>, Option<InputError>) {
    let (detected, bom) = detect_encoding(bytes);
    let declared = || declared_encoding(bytes).and_then(|name| Codec::named(&name));
    let codec = encoding
        .and_then(Codec::named)
        .or_else(|| (bom > 0).then(|| Codec::named(detected)).flatten())
        .or_else(declared)
        .unwrap_or(Codec::Utf8);
    let bytes = codec
        .bom()
        .and_then(|bom| bytes.strip_prefix(bom))
        .unwrap_or(bytes);
    let (mut text, invalid_at) = codec.decode(bytes, last);
    let error = invalid_at.map(|at| {
        if !recover {
            text.to_mut().truncate(at);
        }
        InputError {
            at,
            severity: if recover {
                ErrorSeverity::Error
            } else {
                ErrorSeverity::Fatal
            },
            message: INVALID_BYTES.to_owned(),
        }
    });
    (text, error)
}

/// HTML input as text, decoded as `HTML4::Document.read_memory` decodes it,
/// and the problem decoding it, if there was one.
fn html_decode<'b>(
    bytes: &'b [u8],
    encoding: Option<&str>,
    last: bool,
) -> (Cow<'b, str>, Option<InputError>) {
    let decoded = html_encoding::decode(bytes, encoding, last);
    let error = decoded.diagnostics.into_iter().next().map(|d| InputError {
        at: d.location.byte_offset,
        severity: d.severity,
        message: d.message,
    });
    (decoded.text, error)
}

/// The document's SAX methods.
static XMLDECL: LazyId = LazyId::new("xmldecl");
static START_DOCUMENT: LazyId = LazyId::new("start_document");
static END_DOCUMENT: LazyId = LazyId::new("end_document");
static START_ELEMENT: LazyId = LazyId::new("start_element");
static END_ELEMENT: LazyId = LazyId::new("end_element");
static START_ELEMENT_NAMESPACE: LazyId = LazyId::new("start_element_namespace");
static END_ELEMENT_NAMESPACE: LazyId = LazyId::new("end_element_namespace");
static CHARACTERS: LazyId = LazyId::new("characters");
static CDATA_BLOCK: LazyId = LazyId::new("cdata_block");
static COMMENT: LazyId = LazyId::new("comment");
static PROCESSING_INSTRUCTION: LazyId = LazyId::new("processing_instruction");
static REFERENCE: LazyId = LazyId::new("reference");
static ERROR: LazyId = LazyId::new("error");
static WARNING: LazyId = LazyId::new("warning");

/// Calls the document's SAX callbacks for each event, as the C extension's
/// libxml2 handler does.
struct Callbacks<'a> {
    ruby: &'a Ruby,
    document: Value,
    language: Language,
    replace_entities: bool,
    location: &'a Cell<SourceLocation>,
    /// A problem with the input, reported when the parser reaches it. If
    /// the input ends there, that error replaces what the parser says about
    /// the premature end, and only the end of the document follows.
    input_error: Option<InputError>,
    reported_input_error: bool,
    /// An exception raised by a callback, which ends the parse.
    raised: Option<Error>,
}

impl EventHandler for Callbacks<'_> {
    fn event(&mut self, event: Event<'_>, location: SourceLocation) {
        if self.raised.is_none() {
            self.location.set(location);
            if let Err(error) = self.call(event, location) {
                self.raised = Some(error);
            }
        }
    }
}

impl<'a> Callbacks<'a> {
    fn new(
        ruby: &'a Ruby,
        document: Value,
        language: Language,
        options: i64,
        location: &'a Cell<SourceLocation>,
    ) -> Self {
        Callbacks {
            ruby,
            document,
            language,
            replace_entities: options & NOENT != 0,
            location,
            input_error: None,
            reported_input_error: false,
            raised: None,
        }
    }

    fn call(&mut self, event: Event<'_>, location: SourceLocation) -> Result<(), Error> {
        if self.report_input_error(&event, location)? {
            return Ok(());
        }
        match self.language {
            Language::Xml => self.call_xml(event),
            Language::Html => self.call_html(event),
        }
    }

    /// Reports the problem with the input, if there is one, once the
    /// parser reaches it, and says whether to drop `event`: one past the
    /// end of input that the problem cut short. Where the input ends, it is
    /// what the parser says about the premature end that reaches it.
    fn report_input_error(
        &mut self,
        event: &Event<'_>,
        location: SourceLocation,
    ) -> Result<bool, Error> {
        let Some(error) = &self.input_error else {
            return Ok(false);
        };
        let end = matches!(event, Event::EndDocument);
        if !self.reported_input_error {
            let reached = end
                || location.byte_offset >= error.at
                    && (!error.halts() || matches!(event, Event::Diagnostic(_)));
            if !reached {
                return Ok(false);
            }
            self.reported_input_error = true;
            self.send_diagnostic(error.severity, &error.message)?;
        }
        Ok(error.halts() && !end)
    }

    fn call_xml(&mut self, event: Event<'_>) -> Result<(), Error> {
        match event {
            Event::XmlDeclaration {
                version,
                encoding,
                standalone,
            } => {
                let standalone = standalone.map(|yes| if yes { "yes" } else { "no" });
                self.send(&XMLDECL, (version, encoding, standalone))
            }
            Event::StartElement {
                name,
                prefix,
                namespace,
                attributes,
            } => {
                let ruby = self.ruby;
                let class = classes::get(ruby, &classes::XML_SAX_PARSER_ATTRIBUTE);
                let (rb_attributes, namespaces) = (ruby.ary_new(), ruby.ary_new());
                for attribute in attributes {
                    let value = match &attribute.raw_value {
                        Some(raw) if !self.replace_entities => Cow::Owned(unreplaced(raw)),
                        _ => Cow::Borrowed(attribute.value.as_str()),
                    };
                    let value = value.as_ref();
                    match (attribute.prefix.as_deref(), attribute.name.as_str()) {
                        (Some("xmlns"), prefix) => namespaces.push((Some(prefix), value))?,
                        (None, "xmlns") => namespaces.push((None::<&str>, value))?,
                        (prefix, local) => rb_attributes.push(class.new_instance((
                            local,
                            prefix,
                            attribute.namespace.as_deref(),
                            value,
                        ))?)?,
                    }
                }
                self.send(
                    &START_ELEMENT_NAMESPACE,
                    (name, rb_attributes, prefix, namespace, namespaces),
                )
            }
            Event::EndElement {
                name,
                prefix,
                namespace,
            } => self.send(&END_ELEMENT_NAMESPACE, (name, prefix, namespace)),
            Event::EntityReference { name, replacement } if !self.replace_entities => {
                self.send(&REFERENCE, (name, replacement))
            }
            Event::Diagnostic(diagnostic) => {
                self.send_diagnostic(diagnostic.severity, &diagnostic.message)
            }
            event => self.call_common(event),
        }
    }

    /// The HTML parser's events, as libxml2's HTML parser reports them:
    /// elements by name, with their attributes as name-value pairs, and its
    /// errors as `HTML4::Document` does.
    fn call_html(&mut self, event: Event<'_>) -> Result<(), Error> {
        match event {
            Event::StartElement {
                name, attributes, ..
            } => {
                let pairs = self.ruby.ary_new();
                for attribute in attributes {
                    pairs.push((attribute.name.as_str(), attribute.value.as_str()))?;
                }
                self.send(&START_ELEMENT, (name, pairs))
            }
            Event::EndElement { name, .. } => self.send(&END_ELEMENT, (name,)),
            Event::Diagnostic(diagnostic) => match libxml2_html_diagnostic(diagnostic) {
                Some(d) => self.send_diagnostic(d.severity, &d.message),
                None => Ok(()),
            },
            event => self.call_common(event),
        }
    }

    /// The events reported the same way for XML and HTML. Raw text (the
    /// content of `script` and the like) is CDATA.
    fn call_common(&mut self, event: Event<'_>) -> Result<(), Error> {
        match event {
            Event::StartDocument => self.send(&START_DOCUMENT, ()),
            Event::EndDocument => self.send(&END_DOCUMENT, ()),
            Event::Text(text) => self.send(&CHARACTERS, (text,)),
            Event::CData(text) | Event::RawText(text) => self.send(&CDATA_BLOCK, (text,)),
            Event::Comment(text) => self.send(&COMMENT, (text,)),
            Event::ProcessingInstruction { target, data } => {
                self.send(&PROCESSING_INSTRUCTION, (target, data))
            }
            _ => Ok(()),
        }
    }

    /// Calls `error` or `warning` with `message`, ended by a newline as
    /// libxml2's messages are.
    fn send_diagnostic(&self, severity: ErrorSeverity, message: &str) -> Result<(), Error> {
        let method = match severity {
            ErrorSeverity::Warning => &WARNING,
            ErrorSeverity::Error | ErrorSeverity::Fatal => &ERROR,
        };
        self.send(method, (format!("{message}\n"),))
    }

    fn send<A: magnus::ArgList>(&self, method: &LazyId, args: A) -> Result<(), Error> {
        let method = LazyId::get_inner_with(method, self.ruby);
        self.document.funcall::<_, _, Value>(method, args).map(drop)
    }
}

/// An attribute's value as libxml2 reports it when it doesn't replace
/// entities, from the value as written: references to other entities are
/// left as they are, and an ampersand from a reference is "&#38;".
fn unreplaced(raw: &str) -> String {
    let mut value = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        push_literal(&mut value, &rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';') else {
            break;
        };
        let name = &rest[1..end];
        let character = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix('x') {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match character {
            Some('&') => value.push_str("&#38;"),
            Some(character) => value.push(character),
            None => value.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    push_literal(&mut value, rest);
    value
}

/// Appends `literal` as attribute-value normalization leaves it: a line
/// end (CRLF included) or tab is a space.
fn push_literal(value: &mut String, literal: &str) {
    let mut chars = literal.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' && chars.peek() == Some(&'\n') {
            chars.next();
        }
        value.push(if matches!(c, '\t' | '\n' | '\r') {
            ' '
        } else {
            c
        });
    }
}

/// The push parser's state: all the input written so far, which each write
/// parses again from the start, reporting only the events not yet
/// reported. That costs time quadratic in the number of writes, where
/// libxml2 resumes where it stopped (see notes/PLAN.md).
#[derive(Default)]
#[magnus::wrap(class = "Nokogiri::XML::SAX::PushParser", free_immediately)]
struct PushParser(RefCell<PushState>);

#[derive(Default)]
struct PushState {
    language: Language,
    /// The encoding an HTML push parser was given.
    encoding: Option<String>,
    input: Vec<u8>,
    /// libxml2 parse options: RECOVER and NOENT.
    options: i64,
    /// How many events have been reported.
    reported: usize,
    /// Whether a problem with the input has been reported.
    reported_input_error: bool,
}

/// `PushParser#initialize_native(sax_parser, filename)`. The writes call
/// the SAX parser's document, which `@sax_parser` holds.
fn push_initialize(
    ruby: &Ruby,
    rb_self: Obj<PushParser>,
    parser: Value,
    _filename: Value,
) -> Result<Obj<PushParser>, Error> {
    if !parser.is_kind_of(classes::get(ruby, &classes::XML_SAX_PARSER)) {
        return Err(Error::new(
            ruby.exception_type_error(),
            "wrong argument type (expected Nokogiri::XML::SAX::Parser)",
        ));
    }
    Ok(rb_self)
}

/// `HTML4::SAX::PushParser#initialize_native(sax_parser, filename,
/// encoding)`
fn html_push_initialize(
    ruby: &Ruby,
    rb_self: Obj<PushParser>,
    parser: Value,
    filename: Value,
    encoding: Option<String>,
) -> Result<Obj<PushParser>, Error> {
    if encoding
        .as_deref()
        .is_some_and(|name| Codec::named(name).is_none())
    {
        return Err(Error::new(
            ruby.exception_arg_error(),
            "Unsupported Encoding",
        ));
    }
    let rb_self = push_initialize(ruby, rb_self, parser, filename)?;
    let mut state = rb_self.0.borrow_mut();
    state.language = Language::Html;
    state.encoding = encoding;
    drop(state);
    Ok(rb_self)
}

/// `PushParser#native_write(chunk, last_chunk)`: adds `chunk` to the input
/// and reports the events it completes; with `last_chunk`, the rest of
/// them. A fatal error raises a SyntaxError, unless the parser recovers,
/// and is raised again by every later write.
fn push_write(
    ruby: &Ruby,
    rb_self: Obj<PushParser>,
    chunk: Value,
    last_chunk: Value,
) -> Result<Obj<PushParser>, Error> {
    if !chunk.is_nil() {
        let chunk = check_string(ruby, chunk)?;
        // Copied in before any further call into Ruby.
        let bytes = unsafe { chunk.as_slice() };
        rb_self.0.borrow_mut().input.extend_from_slice(bytes);
    }
    let last = last_chunk.to_bool();
    let parser = RObject::try_convert(rb_self.ivar_get::<_, Value>("@sax_parser")?)?;
    let document: Value = parser.ivar_get("@document")?;

    let state = rb_self.0.borrow();
    let (language, reported) = (state.language, state.reported);
    let options = language.options(state.options);
    let (text, input_error) =
        language.decode(&state.input, state.encoding.as_deref(), options, last);
    // Owned: the callbacks may write to this parser again.
    let mut text = text.into_owned();
    let reported_input_error = state.reported_input_error;
    drop(state);
    // Before the last chunk, only up to the last `>`: what follows it is
    // text or markup that the next chunk may continue.
    if !last {
        text.truncate(text.rfind('>').map_or(0, |at| at + 1));
    }

    let location = Cell::new(SourceLocation::default());
    let mut callbacks = Callbacks::new(ruby, document, language, options, &location);
    callbacks.input_error = input_error;
    callbacks.reported_input_error = reported_input_error;
    let mut handler = Push {
        callbacks,
        last,
        len: text.len(),
        skip: reported,
        complete: 0,
        stopped: false,
    };
    let result = language.parse(&text, options, &mut handler);
    let callbacks = handler.callbacks;
    let mut state = rb_self.0.borrow_mut();
    state.reported = handler.complete.max(reported);
    state.reported_input_error = callbacks.reported_input_error;
    drop(state);
    if let Some(raised) = callbacks.raised {
        return Err(raised);
    }
    match result {
        // A fatal error at the end of the input so far may be just that.
        Err(error)
            if options & RECOVER == 0 && (last || error.location.byte_offset < text.len()) =>
        {
            let message = match callbacks.input_error {
                Some(input) if input.halts() && error.location.byte_offset >= input.at => {
                    input.message
                }
                _ => error.message,
            };
            let diagnostic = ParseDiagnostic {
                severity: ErrorSeverity::Fatal,
                message,
                location: error.location,
            };
            Err(syntax_error::raise(ruby, &diagnostic))
        }
        _ => Ok(rb_self),
    }
}

/// Passes on the events of a parse of the input so far that weren't
/// reported before and that the rest of the input can't change: before
/// the last chunk, nothing from the end of the input on, where the start
/// of the document (an XML declaration may yet come), text and errors may
/// change, and in HTML an element may be closed only because the input
/// ends.
struct Push<'a> {
    callbacks: Callbacks<'a>,
    last: bool,
    len: usize,
    /// How many events were reported before.
    skip: usize,
    /// How many events of this parse are complete.
    complete: usize,
    /// Whether an incomplete event has come: nothing after it is complete.
    stopped: bool,
}

impl EventHandler for Push<'_> {
    fn event(&mut self, event: Event<'_>, location: SourceLocation) {
        if self.stopped {
            return;
        }
        let at_end = location.byte_offset >= self.len;
        let incomplete = match event {
            Event::EndDocument => true,
            Event::StartDocument | Event::Text(_) | Event::RawText(_) | Event::Diagnostic(_) => {
                at_end
            }
            Event::EndElement { .. } => at_end && self.callbacks.language == Language::Html,
            _ => false,
        };
        self.stopped = !self.last && incomplete;
        if !self.stopped {
            self.complete += 1;
            if self.complete > self.skip {
                self.callbacks.event(event, location);
            }
        }
    }
}

fn push_options(rb_self: &PushParser) -> i64 {
    rb_self.0.borrow().options
}

fn set_push_options(rb_self: &PushParser, options: i64) {
    rb_self.0.borrow_mut().options = options;
}

fn push_replace_entities(rb_self: &PushParser) -> bool {
    rb_self.0.borrow().options & NOENT != 0
}

fn set_push_replace_entities(rb_self: &PushParser, value: Value) -> Value {
    let mut state = rb_self.0.borrow_mut();
    if value.to_bool() {
        state.options |= NOENT;
    } else {
        state.options &= !NOENT;
    }
    value
}

fn recovery(rb_self: &ParserContext) -> bool {
    rb_self.recovery.get()
}

fn set_recovery(rb_self: &ParserContext, value: Value) -> Value {
    rb_self.recovery.set(value.to_bool());
    value
}

fn replace_entities(rb_self: &ParserContext) -> bool {
    rb_self.replace_entities.get()
}

fn set_replace_entities(rb_self: &ParserContext, value: Value) -> Value {
    rb_self.replace_entities.set(value.to_bool());
    value
}

fn line(rb_self: &ParserContext) -> u32 {
    rb_self.location.get().line
}

fn column(rb_self: &ParserContext) -> u32 {
    rb_self.location.get().column
}
