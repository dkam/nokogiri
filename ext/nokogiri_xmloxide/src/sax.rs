//! The SAX parsers: `XML::SAX::Parser`, `ParserContext` and `PushParser`
//! and their HTML4 subclasses, mirroring `ext/nokogiri/xml_sax_*.c` and
//! `html4_sax_*.c`. A context holds the input; `parse_with` parses it with
//! xmloxide's XML or HTML parser and calls the SAX parser's document for
//! each event the parser reports.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};

use magnus::{
    Error, Module, Object, RClass, RObject, RString, Ruby, Value, method, prelude::*,
    typed_data::Obj,
};
use xmloxide::encoding::{declared_encoding, detect_encoding};
use xmloxide::error::{ErrorSeverity, ParseDiagnostic, ParseError, SourceLocation};
use xmloxide::events::{Event, EventHandler};
use xmloxide::html::HtmlParseOptions;
use xmloxide::parser::ParseOptions;

use crate::classes;
use crate::document::{libxml2_html_diagnostic, read_all};
use crate::encoding_handler::Codec;
use crate::html_encoding;
use crate::html4_tables::{self, DataMode};
use crate::syntax_error;

/// libxml2's `XML_PARSE_RECOVER` and `XML_PARSE_NOENT` option bits.
const RECOVER: i64 = 1 << 0;
const NOENT: i64 = 1 << 1;

#[derive(Clone, Copy, Default, PartialEq)]
enum Language {
    #[default]
    Xml,
    Html,
}

impl Language {
    /// Parses `text` in this language, reporting to `handler`.
    fn parse(
        self,
        text: &str,
        recover: bool,
        handler: &mut dyn EventHandler,
    ) -> Result<(), ParseError> {
        match self {
            Language::Xml => xmloxide::events::parse_str(
                text,
                &ParseOptions::default().recover(recover),
                handler,
            ),
            Language::Html => {
                xmloxide::events::parse_html(text, &HtmlParseOptions::default(), handler)
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
    let input = expect_string(ruby, input)?;
    // Copied out: the context must not hold a borrow of a Ruby string.
    let bytes = unsafe { input.as_slice() }.to_vec();
    if bytes.is_empty() {
        return Err(Error::new(
            ruby.exception_runtime_error(),
            "input string cannot be empty",
        ));
    }
    new_context(ruby, class, Some(bytes), encoding)
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
/// but recovering, as an HTML parser does.
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

/// `Check_Type(value, T_STRING)`'s error for anything else.
fn expect_string(ruby: &Ruby, value: Value) -> Result<RString, Error> {
    RString::from_value(value).ok_or_else(|| {
        // nil, true and false by name, as Ruby's rb_builtin_class_name does.
        let special = [
            ruby.class_nil_class(),
            ruby.class_true_class(),
            ruby.class_false_class(),
        ];
        let type_name = if special.iter().any(|&class| value.is_kind_of(class)) {
            value.inspect()
        } else {
            value.class().inspect()
        };
        Error::new(
            ruby.exception_type_error(),
            format!("wrong argument type {type_name} (expected String)"),
        )
    })
}

/// `ParserContext#parse_with(sax_parser)`: parses the input as XML, calling
/// the parser's document for each event, and returns nil. An exception
/// from a callback ends the parse and propagates.
fn parse_with(ruby: &Ruby, rb_self: Obj<ParserContext>, parser: Value) -> Result<(), Error> {
    let document = sax_document(ruby, parser)?;
    let (bytes, io_error) = context_input(ruby, &rb_self)?;
    let recover = rb_self.recovery.get();
    let (text, input_error) = decode(&bytes, rb_self.encoding.as_deref(), recover);
    let mut handler = Callbacks::new(ruby, document, Language::Xml, &rb_self.location);
    handler.replace_entities = rb_self.replace_entities.get();
    handler.input_error = input_error.or_else(|| io_error.then(|| unknown_io_error(&text)));
    // The parser's errors have gone to the document's callbacks.
    let _ = Language::Xml.parse(&text, recover, &mut handler);
    handler.raised.map_or(Ok(()), Err)
}

/// `HTML4::SAX::ParserContext#parse_with(sax_parser)`: as for XML, parsing
/// the input as HTML.
fn html_parse_with(ruby: &Ruby, rb_self: Obj<ParserContext>, parser: Value) -> Result<(), Error> {
    let document = sax_document(ruby, parser)?;
    let (bytes, io_error) = context_input(ruby, &rb_self)?;
    let (text, input_error) = html_decode(&bytes, rb_self.encoding.as_deref());
    let mut handler = Callbacks::new(ruby, document, Language::Html, &rb_self.location);
    handler.input_error = input_error.or_else(|| io_error.then(|| unknown_io_error(&text)));
    let _ = Language::Html.parse(&text, true, &mut handler);
    handler.raised.map_or(Ok(()), Err)
}

/// The document of `parser`, which must be a SAX parser.
fn sax_document(ruby: &Ruby, parser: Value) -> Result<Value, Error> {
    if !parser.is_kind_of(classes::get(ruby, &classes::XML_SAX_PARSER)) {
        return Err(Error::new(
            ruby.exception_arg_error(),
            "argument must be a Nokogiri::XML::SAX::Parser",
        ));
    }
    RObject::try_convert(parser)?.ivar_get("@document")
}

/// The context's input, and whether reading it stopped on an IO error.
fn context_input<'c>(
    ruby: &Ruby,
    context: &'c Obj<ParserContext>,
) -> Result<(Cow<'c, [u8]>, bool), Error> {
    Ok(match &context.bytes {
        Some(bytes) => (Cow::Borrowed(bytes.as_slice()), false),
        None => {
            let input = read_all(ruby, context.ivar_get("@io")?)?;
            (Cow::Owned(input.bytes), input.io_error)
        }
    })
}

/// A problem with the input at a point in it, which libxml2 reports there:
/// a byte invalid in the input's encoding, an unsupported encoding, or a
/// failed read.
struct InputError {
    at: usize,
    /// The document's method to call: `error` or `warning`.
    callback: &'static str,
    message: String,
    /// Whether the input ends there. Recovering from an invalid byte, the
    /// parser carries on.
    halts: bool,
}

const INVALID_BYTES: &str = "Invalid bytes in character encoding";

/// The error for the end of `text`, where reading the input failed.
fn unknown_io_error(text: &str) -> InputError {
    InputError {
        at: text.len(),
        callback: "error",
        message: "Unknown IO error\n".to_owned(),
        halts: true,
    }
}

/// XML input as text, and the error for its first byte that is invalid in
/// its encoding, if it has one: without recovery the input ends there, and
/// with it UTF-8 input carries on with U+FFFD. As in libxml2, the encoding
/// is the one the caller named, else the one a byte order mark or the XML
/// declaration gives, else UTF-8.
fn decode(bytes: &[u8], encoding: Option<&str>, recover: bool) -> (String, Option<InputError>) {
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
    let (mut text, invalid_at) = codec.decode(bytes);
    let error = invalid_at.map(|at| {
        if !recover {
            text.truncate(at);
        }
        InputError {
            at,
            callback: "error",
            message: format!("{INVALID_BYTES}\n"),
            halts: !recover,
        }
    });
    (text, error)
}

/// HTML input as text, decoded as `HTML4::Document.read_memory` decodes it,
/// and the problem decoding it, if there was one.
fn html_decode(bytes: &[u8], encoding: Option<&str>) -> (String, Option<InputError>) {
    let decoded = html_encoding::decode(bytes, encoding);
    let error = decoded.diagnostics.into_iter().next().map(|d| InputError {
        at: d.location.byte_offset,
        callback: callback(d.severity),
        message: format!("{}\n", d.message),
        halts: d.severity == ErrorSeverity::Fatal,
    });
    (decoded.text, error)
}

/// The document's method for a diagnostic of `severity`.
fn callback(severity: ErrorSeverity) -> &'static str {
    match severity {
        ErrorSeverity::Warning => "warning",
        ErrorSeverity::Error | ErrorSeverity::Fatal => "error",
    }
}

/// Calls the document's SAX callbacks for each event, as the C extension's
/// libxml2 handler does.
struct Callbacks<'a> {
    ruby: &'a Ruby,
    document: Value,
    language: Language,
    replace_entities: bool,
    location: &'a Cell<SourceLocation>,
    /// The names of the open HTML elements, innermost last: in some, text
    /// is raw.
    open: Vec<String>,
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
        location: &'a Cell<SourceLocation>,
    ) -> Self {
        Callbacks {
            ruby,
            document,
            language,
            replace_entities: false,
            location,
            open: Vec::new(),
            input_error: None,
            reported_input_error: false,
            raised: None,
        }
    }

    /// Keeps track of the open HTML elements, for events reported and for
    /// those a push parser skips as reported before.
    fn track(&mut self, event: &Event<'_>) {
        if self.language == Language::Html {
            match event {
                Event::StartElement { name, .. } => self.open.push((*name).to_owned()),
                Event::EndElement { .. } => drop(self.open.pop()),
                _ => {}
            }
        }
    }

    fn call(&mut self, event: Event<'_>, location: SourceLocation) -> Result<(), Error> {
        self.track(&event);
        if let Some(error) = &self.input_error {
            let reached = match event {
                Event::EndDocument => true,
                Event::Diagnostic(d) => d.location.byte_offset >= error.at,
                _ => !error.halts && location.byte_offset >= error.at,
            };
            let halts = error.halts;
            if reached && !self.reported_input_error {
                let (callback, message) = (error.callback, error.message.clone());
                self.reported_input_error = true;
                self.send(callback, (message,))?;
            }
            if halts && self.reported_input_error && !matches!(event, Event::EndDocument) {
                return Ok(());
            }
        }
        match self.language {
            Language::Xml => self.call_xml(event),
            Language::Html => self.call_html(event),
        }
    }

    fn call_xml(&mut self, event: Event<'_>) -> Result<(), Error> {
        match event {
            Event::XmlDeclaration {
                version,
                encoding,
                standalone,
            } => {
                let standalone = standalone.map(|yes| if yes { "yes" } else { "no" });
                self.send("xmldecl", (version, encoding, standalone))
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
                    "start_element_namespace",
                    (name, rb_attributes, prefix, namespace, namespaces),
                )
            }
            Event::EndElement {
                name,
                prefix,
                namespace,
            } => self.send("end_element_namespace", (name, prefix, namespace)),
            Event::Text(text) => self.send("characters", (text,)),
            Event::EntityReference { name, replacement } if !self.replace_entities => {
                self.send("reference", (name, replacement))
            }
            Event::EntityReference { .. } => Ok(()),
            Event::Diagnostic(diagnostic) => self.send(
                callback(diagnostic.severity),
                (format!("{}\n", diagnostic.message),),
            ),
            event => self.call_common(event),
        }
    }

    /// The HTML parser's events, as libxml2's HTML parser reports them:
    /// elements by name, with their attributes as name-value pairs, the
    /// content of raw text elements (`script`, `style` and the like) as
    /// CDATA, and its errors as `HTML4::Document` does.
    fn call_html(&mut self, event: Event<'_>) -> Result<(), Error> {
        match event {
            Event::StartElement {
                name, attributes, ..
            } => {
                let pairs = self.ruby.ary_new();
                for attribute in attributes {
                    pairs.push((attribute.name.as_str(), attribute.value.as_str()))?;
                }
                self.send("start_element", (name, pairs))
            }
            Event::EndElement { name, .. } => self.send("end_element", (name,)),
            Event::Text(text) => {
                let raw = self
                    .open
                    .last()
                    .and_then(|name| html4_tables::lookup(name))
                    .is_some_and(|info| {
                        !matches!(info.data_mode, DataMode::Normal | DataMode::RcData)
                    });
                self.send(if raw { "cdata_block" } else { "characters" }, (text,))
            }
            Event::Diagnostic(diagnostic) => match libxml2_html_diagnostic(diagnostic.clone()) {
                Some(d) => self.send(callback(d.severity), (format!("{}\n", d.message),)),
                None => Ok(()),
            },
            event => self.call_common(event),
        }
    }

    /// The events reported the same way for XML and HTML.
    fn call_common(&mut self, event: Event<'_>) -> Result<(), Error> {
        match event {
            Event::StartDocument => self.send("start_document", ()),
            Event::EndDocument => self.send("end_document", ()),
            Event::CData(text) => self.send("cdata_block", (text,)),
            Event::Comment(text) => self.send("comment", (text,)),
            Event::ProcessingInstruction { target, data } => {
                self.send("processing_instruction", (target, data))
            }
            _ => Ok(()),
        }
    }

    fn send<A: magnus::ArgList>(&self, callback: &str, args: A) -> Result<(), Error> {
        self.document
            .funcall::<_, _, Value>(callback, args)
            .map(drop)
    }
}

/// An attribute's value as libxml2 reports it when it doesn't replace
/// entities, from the value as written: references to other entities are
/// left as they are, and an ampersand from a reference is "&#38;".
fn unreplaced(raw: &str) -> String {
    let mut value = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        value.extend(rest[..at].chars().map(normalize_blank));
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
    value.extend(rest.chars().map(normalize_blank));
    value
}

/// Attribute-value normalization: a literal tab or line end is a space.
fn normalize_blank(c: char) -> char {
    if matches!(c, '\t' | '\n' | '\r') {
        ' '
    } else {
        c
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
    /// libxml2 parse options: only RECOVER and NOENT matter.
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
        let chunk = expect_string(ruby, chunk)?;
        let bytes = unsafe { chunk.as_slice() }.to_vec();
        rb_self.0.borrow_mut().input.extend_from_slice(&bytes);
    }
    let last = last_chunk.to_bool();
    let parser = RObject::try_convert(rb_self.ivar_get::<_, Value>("@sax_parser")?)?;
    let document: Value = parser.ivar_get("@document")?;

    let state = rb_self.0.borrow();
    let (language, reported) = (state.language, state.reported);
    let options = match language {
        Language::Xml => state.options,
        Language::Html => state.options | RECOVER,
    };
    let recover = options & RECOVER != 0;
    let decode_input = |input: &[u8]| match language {
        Language::Xml => decode(input, None, recover),
        Language::Html => html_decode(input, state.encoding.as_deref()),
    };
    let (mut text, mut input_error) = decode_input(&state.input);
    // Before the last chunk, an invalid sequence that leaving out the last
    // few bytes removes is a character the next chunk may complete.
    if !last && input_error.is_some() {
        let input = &state.input;
        let complete = (1..4.min(input.len()))
            .map(|k| decode_input(&input[..input.len() - k]))
            .find(|(_, error)| error.is_none());
        if let Some(complete) = complete {
            (text, input_error) = complete;
        }
    }
    let reported_input_error = state.reported_input_error;
    drop(state);
    // Before the last chunk, only up to the last `>`: what follows it is
    // text or markup that the next chunk may continue. (Where xmloxide
    // reports running out of input in a construct depends on the construct;
    // it is the end of the input in one cut at a `>` inside it.)
    let text = if last {
        text.as_str()
    } else {
        &text[..text.rfind('>').map_or(0, |at| at + 1)]
    };
    let invalid_at = input_error.as_ref().map(|error| error.at);
    let location = Cell::new(SourceLocation::default());
    let mut callbacks = Callbacks::new(ruby, document, language, &location);
    callbacks.replace_entities = options & NOENT != 0;
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
    let result = language.parse(text, recover, &mut handler);
    let mut state = rb_self.0.borrow_mut();
    state.reported = handler.complete.max(reported);
    state.reported_input_error = handler.callbacks.reported_input_error;
    drop(state);
    if let Some(raised) = handler.callbacks.raised {
        return Err(raised);
    }
    match result {
        // A fatal error at the end of the input so far may be just that.
        Err(error) if !recover && (last || error.location.byte_offset < text.len()) => {
            let message = if invalid_at.is_some_and(|at| error.location.byte_offset >= at) {
                INVALID_BYTES.to_owned()
            } else {
                error.message
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
/// the last chunk, nothing from the start of the document, text, or an
/// error at the end of the input on. In HTML, nor from an element closed
/// there, which may be closed only because the input ends.
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
        let at_end = |offset| !self.last && offset >= self.len;
        self.stopped = match event {
            // At the end, an XML declaration may yet come before it.
            Event::StartDocument | Event::Text(_) => at_end(location.byte_offset),
            Event::EndElement { .. } if self.callbacks.language == Language::Html => {
                at_end(location.byte_offset)
            }
            Event::Diagnostic(diagnostic) => at_end(diagnostic.location.byte_offset),
            Event::EndDocument => !self.last,
            _ => false,
        };
        if !self.stopped {
            self.complete += 1;
            if self.complete > self.skip {
                self.callbacks.event(event, location);
            } else {
                self.callbacks.track(&event);
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
