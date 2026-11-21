//! HTML5: `Nokogiri::Gumbo.parse` and `.fragment` (`ext/nokogiri/gumbo.c`)
//! on xmloxide's WHATWG parser, and the HTML5 serializer,
//! `Node#html_standard_serialize` (`ext/nokogiri/xml_node.c`).
//!
//! Gumbo builds a tree of its own, which `gumbo.c` copies into libxml2's.
//! xmloxide parses straight into an arena tree, which [`reshape`] then makes
//! look like that copy:
//!
//! - SVG and MathML elements get the prefixes `svg` and `math`, and XLink
//!   and xmlns attributes `xlink` and `xmlns`, declared on the top element
//!   of their subtree (the `html` element, for a document), as
//!   `lookup_or_add_ns` declares them.
//! - Gumbo never declares namespaces from markup. An SVG or MathML element's
//!   `xmlns` attributes are in the xmlns namespace, with that prefix; an HTML
//!   element's `xmlns` attribute is an ordinary attribute (see
//!   [`namespace::is_plain_xmlns`]).
//! - The doctype is the document's first child, as `xmlCreateIntSubset`
//!   leaves it, with empty identifiers as none.

use magnus::{
    Error, Module, RClass, RHash, RModule, RString, Ruby, Value, function, method,
    prelude::*,
    scan_args::{get_kwargs, scan_args},
    typed_data::Obj,
};
use xmloxide::html5::{
    Html5ErrorKind, Html5ErrorToken, Html5Limit, Html5ParseOptions, Html5ParseResult, QuirksMode,
    doctype_quirks_mode, parse_html5_full_with_options,
};
use xmloxide::tree::NodeKind;
use xmloxide::{Document, NodeId};

use crate::classes;
use crate::node::{DocKind, NodeKey, NodeObj, Receiver, find_attr};
use crate::{document, namespace, syntax_error};

const XHTML: &str = "http://www.w3.org/1999/xhtml";
const SVG: &str = "http://www.w3.org/2000/svg";
const MATHML: &str = "http://www.w3.org/1998/Math/MathML";
const XLINK: &str = "http://www.w3.org/1999/xlink";
const XMLNS: &str = namespace::XMLNS_NAMESPACE;

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    let gumbo: RModule = ruby.eval("Nokogiri::Gumbo")?;
    gumbo.define_singleton_method("parse", function!(parse, -1))?;
    gumbo.define_singleton_method("fragment", function!(fragment, -1))?;
    let node = classes::get(ruby, &classes::NODE);
    node.define_private_method(
        "html_standard_serialize",
        method!(html_standard_serialize, 2),
    )?;
    node.define_private_method("prepend_newline?", method!(prepend_newline, 0))?;
    Ok(())
}

// --- Options -------------------------------------------------------------

/// A count option: negative means no limit.
fn limit(value: i64) -> Option<usize> {
    usize::try_from(value).ok()
}

/// The keyword options both entry points take (`common_options` in C).
fn common_options(kwargs: RHash) -> Result<Html5ParseOptions, Error> {
    let kwargs = get_kwargs::<_, (i64, i64, i64), (Option<Value>,), ()>(
        kwargs,
        &["max_attributes", "max_errors", "max_tree_depth"],
        &["parse_noscript_content_as_text"],
    )?;
    let (max_attributes, max_errors, max_tree_depth) = kwargs.required;
    let (noscript_as_text,) = kwargs.optional;
    Ok(Html5ParseOptions {
        scripting: noscript_as_text.is_some_and(|v| v.to_bool()),
        max_attributes: limit(max_attributes),
        max_tree_depth: limit(max_tree_depth),
        // Gumbo records no more than this, so neither does xmloxide.
        max_errors: limit(max_errors),
        ..Default::default()
    })
}

/// Parses `input`, raising as `perform_parse` does when a limit stops it.
fn perform_parse(
    ruby: &Ruby,
    options: &Html5ParseOptions,
    input: &str,
) -> Result<Html5ParseResult, Error> {
    let result = parse_html5_full_with_options(input, options);
    match result.limit_exceeded {
        None => Ok(result),
        Some(Html5Limit::Attributes) => Err(Error::new(
            ruby.exception_arg_error(),
            "Attributes per element limit exceeded",
        )),
        Some(Html5Limit::TreeDepth) => Err(Error::new(
            ruby.exception_arg_error(),
            "Document tree depth limit exceeded",
        )),
    }
}

/// Gumbo's `GumboQuirksModeEnum` value.
fn quirks_mode_value(mode: QuirksMode) -> i64 {
    match mode {
        QuirksMode::NoQuirks => 0,
        QuirksMode::Quirks => 1,
        QuirksMode::LimitedQuirks => 2,
    }
}

/// The input as UTF-8 text. `HTML5.read_and_encode` has already transcoded
/// it; invalid sequences become U+FFFD. A leading byte order mark is
/// dropped, as Gumbo drops it (and the HTML standard's decoder does).
fn input_text(input: RString) -> String {
    // Copied out before any further Ruby call.
    let bytes = unsafe { input.as_slice() };
    let bytes = bytes.strip_prefix("\u{feff}".as_bytes()).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

// --- Reshaping the tree --------------------------------------------------

/// Makes the parsed tree under `top` look like the one `build_tree` makes:
/// see the module documentation.
fn reshape(tree: &mut Document, top: NodeId) {
    let top_level: Vec<NodeId> = tree.children(top).collect();
    for subtree in top_level {
        if !tree.is_element(subtree) {
            continue;
        }
        let elements: Vec<NodeId> = std::iter::once(subtree)
            .chain(tree.descendants(subtree))
            .filter(|id| tree.is_element(*id))
            .collect();
        for id in elements {
            reshape_element(tree, subtree, id);
        }
    }
}

fn reshape_element(tree: &mut Document, root: NodeId, id: NodeId) {
    let mut declare: Vec<(&str, &str)> = Vec::new();
    if let NodeKind::Element {
        prefix,
        namespace,
        attributes,
        ..
    } = tree.kind_mut(id)
    {
        let element_prefix = match namespace.as_deref() {
            Some(SVG) => Some("svg"),
            Some(MATHML) => Some("math"),
            _ => None,
        };
        if let Some(p) = element_prefix {
            *prefix = Some(p.to_owned());
            declare.push((p, if p == "svg" { SVG } else { MATHML }));
        }
        for attribute in attributes.iter_mut() {
            match attribute.namespace.as_deref() {
                Some(XLINK) => declare.push(("xlink", XLINK)),
                // An SVG or MathML element's `xmlns` and `xmlns:xlink`, in
                // the xmlns namespace with that prefix, as gumbo.c puts them.
                Some(XMLNS) if element_prefix.is_some() => {
                    attribute.prefix.get_or_insert_with(|| "xmlns".to_owned());
                    declare.push(("xmlns", XMLNS));
                }
                Some(_) => {}
                // An HTML element's `xmlns` attribute is an attribute, not a
                // declaration.
                None if attribute.prefix.is_none() && attribute.name == "xmlns" => {
                    attribute.namespace = Some(XMLNS.to_owned());
                }
                None => {}
            }
        }
    }
    for (prefix, href) in declare {
        if namespace::lookup(tree, root, Some(prefix)).is_none() {
            namespace::declare(tree, root, Some(prefix), href);
        }
    }
}

/// Moves the doctype to the front of the document, with empty identifiers
/// as none.
fn reshape_doctype(tree: &mut Document) {
    let document = tree.root();
    let Some(doctype) = tree
        .children(document)
        .find(|id| matches!(tree.node(*id).kind, NodeKind::DocumentType { .. }))
    else {
        return;
    };
    if let NodeKind::DocumentType {
        public_id,
        system_id,
        ..
    } = tree.kind_mut(doctype)
    {
        public_id.take_if(|id| id.is_empty());
        system_id.take_if(|id| id.is_empty());
    }
    if let Some(first) = tree.first_child(document).filter(|first| *first != doctype) {
        tree.detach(doctype);
        tree.insert_before(first, doctype);
    }
}

// --- Errors --------------------------------------------------------------

/// The parse errors (xmloxide kept at most `max_errors`) as
/// `Nokogiri::XML::SyntaxError`s, as `add_errors` makes them: each with its
/// code (or `"generic-parser"`) in `str1` and Gumbo's message, followed by
/// the line with a caret.
fn errors(
    ruby: &Ruby,
    result: &Html5ParseResult,
    input: &str,
    file: Value,
) -> Result<Option<magnus::RArray>, Error> {
    let details = &result.error_details;
    if details.is_empty() {
        return Ok(None);
    }
    let positions = Positions::new(input);
    let array = ruby.ary_new_capa(details.len());
    for error in details {
        let location = &error.location;
        let column = positions.column(location.byte_offset);
        let (code, text) = match &error.kind {
            Html5ErrorKind::Code(code) => (*code, tokenizer_message(code).to_owned()),
            Html5ErrorKind::TreeConstruction {
                token,
                before_doctype,
                open_elements,
            } => (
                "generic-parser",
                parser_message(token, *before_doctype, open_elements),
            ),
        };
        let offset = positions.original_offset(location.byte_offset);
        let message = caret_diagnostic(&text, input, offset, column);
        array.push(syntax_error::gumbo(
            ruby,
            &message,
            code,
            location.line,
            column,
            file,
        )?)?;
    }
    Ok(Some(array))
}

/// Error positions as Gumbo reports them. xmloxide locates an error by its
/// byte offset in the input with newlines normalized (CR and CRLF to LF), as
/// both parsers read it. Gumbo counts columns in characters, moving to the
/// next tab stop at a tab, and shows the line as it is in the input.
struct Positions<'a> {
    input: &'a str,
    normalized: String,
    /// Offsets in `normalized` of the newlines that are CRLF in `input`.
    crlf: Vec<usize>,
}

impl<'a> Positions<'a> {
    fn new(input: &'a str) -> Self {
        let mut normalized = String::with_capacity(input.len());
        let mut crlf = Vec::new();
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\r' {
                if chars.next_if_eq(&'\n').is_some() {
                    crlf.push(normalized.len());
                }
                normalized.push('\n');
            } else {
                normalized.push(c);
            }
        }
        Self {
            input,
            normalized,
            crlf,
        }
    }

    /// Gumbo's column at `offset`: from 1, one per character, and a tab to
    /// the next multiple of `tab_stop`, 8 (`utf8.c`).
    fn column(&self, offset: usize) -> u32 {
        let offset = self.normalized.floor_char_boundary(offset);
        let before = &self.normalized[..offset];
        let line = &before[before.rfind('\n').map_or(0, |n| n + 1)..];
        line.chars().fold(1, |column: u32, c| {
            if c == '\t' {
                (column / 8 + 1) * 8
            } else {
                column.saturating_add(1)
            }
        })
    }

    /// The offset in the input of `offset` in the normalized input.
    fn original_offset(&self, offset: usize) -> usize {
        (offset + self.crlf.partition_point(|&n| n < offset)).min(self.input.len())
    }
}

/// Gumbo's message for a tree construction error (`handle_parser_error`).
fn parser_message(
    token: &Html5ErrorToken,
    before_doctype: bool,
    open_elements: &[String],
) -> String {
    let stack = || format!(" Currently open tags: {}.", open_elements.join(", "));
    match token {
        Html5ErrorToken::Doctype => "This is not a legal doctype".to_owned(),
        _ if before_doctype => "Expected a doctype token".to_owned(),
        Html5ErrorToken::Comment => "Comments aren't legal here".to_owned(),
        Html5ErrorToken::Character('\0') => "Null bytes are not allowed in HTML5".to_owned(),
        Html5ErrorToken::Character(_) => "Character tokens aren't legal here".to_owned(),
        Html5ErrorToken::EndOfFile => format!("Premature end of file.{}", stack()),
        Html5ErrorToken::StartTag(name) => {
            format!("Start tag '{name}' isn't allowed here.{}", stack())
        }
        Html5ErrorToken::EndTag(name) => format!("End tag '{name}' isn't allowed here.{}", stack()),
    }
}

/// Gumbo's message for an error the specification names, without the
/// details Gumbo interpolates; the code itself for one it doesn't know.
fn tokenizer_message(code: &str) -> &str {
    match code {
        "abrupt-closing-of-empty-comment" => "Empty comment abruptly closed, use '-->'.",
        "abrupt-doctype-public-identifier" => "DOCTYPE public identifier missing closing quote.",
        "abrupt-doctype-system-identifier" => "DOCTYPE system identifier missing closing quote.",
        "absence-of-digits-in-numeric-character-reference" => {
            "Numeric character reference does not contain any digits."
        }
        "cdata-in-html-content" => "CDATA section outside foreign (SVG or MathML) content.",
        "character-reference-outside-unicode-range" => {
            "Numeric character reference references a code point that is outside the valid Unicode range."
        }
        "control-character-in-input-stream" => "Input contains prohibited control code point.",
        "control-character-reference" => {
            "Numeric character reference references prohibited control code point."
        }
        "end-tag-with-attributes" => "End tag contains attributes.",
        "duplicate-attribute" => "Tag contains multiple attributes with the same name.",
        "end-tag-with-trailing-solidus" => "End tag ends with '/>', use '>'.",
        "eof-before-tag-name" => "End of input where a tag name is expected.",
        "eof-in-cdata" => "End of input in CDATA section.",
        "eof-in-comment" => "End of input in comment.",
        "eof-in-doctype" => "End of input in DOCTYPE.",
        "eof-in-script-html-comment-like-text" => {
            "End of input in text that resembles an HTML comment inside script element content."
        }
        "eof-in-tag" => "End of input in tag.",
        "incorrectly-closed-comment" => "Comment closed incorrectly by '--!>', use '-->'.",
        "incorrectly-opened-comment" => {
            "Comment, DOCTYPE, or CDATA opened incorrectly, use '<!--', '<!DOCTYPE', or '<![CDATA['."
        }
        "invalid-character-sequence-after-doctype-name" => {
            "Invalid character sequence after DOCTYPE name, expected 'PUBLIC', 'SYSTEM', or '>'."
        }
        "invalid-first-character-of-tag-name" => "Invalid first character of tag name.",
        "missing-attribute-value" => "Missing attribute value.",
        "missing-doctype-name" => "Missing DOCTYPE name.",
        "missing-doctype-public-identifier" => "Missing DOCTYPE public identifier.",
        "missing-doctype-system-identifier" => "Missing DOCTYPE system identifier.",
        "missing-end-tag-name" => "Missing end tag name.",
        "missing-quote-before-doctype-public-identifier" => {
            "Missing quote before DOCTYPE public identifier."
        }
        "missing-quote-before-doctype-system-identifier" => {
            "Missing quote before DOCTYPE system identifier."
        }
        "missing-semicolon-after-character-reference" => {
            "Missing semicolon after character reference."
        }
        "missing-whitespace-after-doctype-public-keyword" => {
            "Missing whitespace after 'PUBLIC' keyword."
        }
        "missing-whitespace-after-doctype-system-keyword" => {
            "Missing whitespace after 'SYSTEM' keyword."
        }
        "missing-whitespace-before-doctype-name" => {
            "Missing whitespace between 'DOCTYPE' keyword and DOCTYPE name."
        }
        "missing-whitespace-between-attributes" => "Missing whitespace between attributes.",
        "missing-whitespace-between-doctype-public-and-system-identifiers" => {
            "Missing whitespace between DOCTYPE public and system identifiers."
        }
        "nested-comment" => "Nested comment.",
        "noncharacter-character-reference" => {
            "Numeric character reference references a noncharacter."
        }
        "noncharacter-in-input-stream" => "Input contains a noncharacter.",
        "non-void-html-element-start-tag-with-trailing-solidus" => {
            "Start tag of nonvoid HTML element ends with '/>', use '>'."
        }
        "null-character-reference" => "Numeric character reference references U+0000.",
        "surrogate-character-reference" => "Numeric character reference references a surrogate.",
        "surrogate-in-input-stream" => "Input contains a surrogate.",
        "unexpected-character-after-doctype-system-identifier" => {
            "Unexpected character after DOCTYPE system identifier."
        }
        "unexpected-character-in-attribute-name" => "Unexpected character in attribute name.",
        "unexpected-character-in-unquoted-attribute-value" => {
            "Unexpected character in unquoted attribute value."
        }
        "unexpected-equals-sign-before-attribute-name" => {
            "Unexpected '=' before an attribute name."
        }
        "unexpected-null-character" => "Input contains unexpected U+0000.",
        "unexpected-question-mark-instead-of-tag-name" => {
            "Unexpected '?' where start tag name is expected."
        }
        "unexpected-solidus-in-tag" => "Unexpected '/' in tag.",
        "unknown-named-character-reference" => "Unknown named character reference.",
        other => other,
    }
}

/// Gumbo's `caret_diagnostic_to_string`: the message, then the input line
/// the error is on with a caret under its column.
fn caret_diagnostic(message: &str, input: &str, offset: usize, column: u32) -> String {
    let offset = offset.min(input.len());
    let bytes = input.as_bytes();
    let mut start = offset;
    if start > 0 && (start == bytes.len() || bytes[start] == b'\n') {
        start -= 1;
    }
    while start > 0 && bytes[start] != b'\n' {
        start -= 1;
    }
    if start > 0 {
        start += 1;
    }
    let end = input[offset..]
        .find('\n')
        .map_or(input.len(), |n| offset + n);
    let line = String::from_utf8_lossy(&bytes[start..end]);
    let spaces = (column as usize).saturating_sub(1);
    format!("{message}\n{line}\n{}^\n", " ".repeat(spaces))
}

// --- Parsing --------------------------------------------------------------

/// `Gumbo.parse(input, url, klass, **options)`: a new document of class
/// `klass`.
fn parse(ruby: &Ruby, args: &[Value]) -> Result<Value, Error> {
    let args = scan_args::<(RString, Value, RClass), (), (), (), RHash, ()>(args)?;
    let (input, url, class) = args.required;
    let options = common_options(args.keywords)?;
    let text = input_text(input);
    let mut result = perform_parse(ruby, &options, &text)?;

    let document_node = result.document.root();
    reshape(&mut result.document, document_node);
    reshape_doctype(&mut result.document);
    let errors = errors(ruby, &result, &text, url)?;
    let quirks_mode = quirks_mode_value(result.quirks_mode);

    // The document's own URL, for a String; @url keeps whatever was given,
    // as gumbo.c keeps it.
    let url_string = RString::from_value(url)
        .map(|url| url.to_string())
        .transpose()?;
    let tree = std::mem::take(&mut result.document);
    let document = document::wrap(ruby, class, tree, url_string, DocKind::Html, &[])?;
    let object = Obj::<NodeObj>::try_convert(document)?;
    object.ivar_set("@url", url)?;
    object.ivar_set("@quirks_mode", quirks_mode)?;
    if let Some(errors) = errors {
        object.ivar_set("@errors", errors)?;
    }
    Ok(document)
}

/// What `noko_gumbo_s_fragment` works out about the context a fragment is
/// parsed in.
struct FragmentContext {
    /// The context element, in xmloxide's notation (`"svg path"`).
    name: String,
    /// Whether the context is a `form` or has one as an ancestor.
    form: bool,
    /// An `annotation-xml` context's `encoding` attribute.
    encoding: Option<String>,
    /// Whether the context is a node rather than a tag name.
    is_node: bool,
}

/// A namespace's place in Gumbo's `GumboNamespaceEnum`, for
/// `lookup_namespace`: `None` for one it doesn't know.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ContextNamespace {
    Html,
    Svg,
    MathMl,
}

/// `lookup_namespace`: the namespace of `node` (any object answering
/// `namespace`). An unknown one raises if `required`.
fn lookup_namespace(
    ruby: &Ruby,
    node: Value,
    required: bool,
) -> Result<Option<ContextNamespace>, Error> {
    let namespace: Value = node.funcall("namespace", ())?;
    if namespace.is_nil() {
        return Ok(Some(ContextNamespace::Html));
    }
    let href: RString = namespace.funcall("href", ())?;
    let href = href.to_string()?;
    Ok(Some(match href.as_str() {
        XHTML => ContextNamespace::Html,
        MATHML => ContextNamespace::MathMl,
        SVG => ContextNamespace::Svg,
        _ if required => {
            return Err(Error::new(
                ruby.exception_arg_error(),
                format!("Unexpected namespace URI \"{href}\""),
            ));
        }
        _ => return Ok(None),
    }))
}

/// A `name` that must be a String, as `Check_Type(…, T_STRING)` requires.
fn string_name(ruby: &Ruby, node: Value) -> Result<String, Error> {
    let name: Value = node.funcall("name", ())?;
    match RString::from_value(name) {
        Some(name) => name.to_string(),
        None => Err(Error::new(
            ruby.exception_type_error(),
            format!(
                "wrong argument type {} (expected String)",
                name.class().inspect()
            ),
        )),
    }
}

fn fragment_context(ruby: &Ruby, context: Value) -> Result<FragmentContext, Error> {
    let tag = |name: String, form: bool| FragmentContext {
        name,
        form,
        encoding: None,
        is_node: false,
    };
    if context.is_nil() {
        return Ok(tag("body".to_owned(), false));
    }
    if let Some(name) = RString::from_value(context) {
        let name = name.to_string()?;
        let (namespace, local) = match name.split_once(':') {
            Some((ns, local)) => match ns.to_ascii_lowercase().as_str() {
                "svg" => (ContextNamespace::Svg, local),
                "math" => (ContextNamespace::MathMl, local),
                "html" => (ContextNamespace::Html, local),
                _ => {
                    return Err(Error::new(
                        ruby.exception_arg_error(),
                        format!("Invalid context namespace '{ns}'"),
                    ));
                }
            },
            // For convenience, svg and math are in their namespaces.
            None if name.eq_ignore_ascii_case("svg") => (ContextNamespace::Svg, name.as_str()),
            None if name.eq_ignore_ascii_case("math") => (ContextNamespace::MathMl, name.as_str()),
            None => (ContextNamespace::Html, name.as_str()),
        };
        let form = namespace == ContextNamespace::Html && local.eq_ignore_ascii_case("form");
        return Ok(tag(xmloxide_context(namespace, local), form));
    }

    let name = string_name(ruby, context)?;
    let namespace = lookup_namespace(ruby, context, true)?.unwrap_or(ContextNamespace::Html);
    // A form ancestor, or the context itself.
    let mut form = false;
    let mut node = context;
    while !node.is_nil() {
        let is_element: Value = node.funcall("element?", ())?;
        if is_element.to_bool() {
            let node_name = string_name(ruby, node)?;
            if node_name.eq_ignore_ascii_case("form")
                && lookup_namespace(ruby, node, false)? == Some(ContextNamespace::Html)
            {
                form = true;
                break;
            }
        }
        node = if node.respond_to("parent", false)? {
            node.funcall("parent", ())?
        } else {
            ruby.qnil().as_value()
        };
    }
    let encoding =
        if namespace == ContextNamespace::MathMl && name.eq_ignore_ascii_case("annotation-xml") {
            let value: Value = context.funcall("[]", ("encoding",))?;
            if value.to_bool() {
                Some(RString::try_convert(value)?.to_string()?)
            } else {
                None
            }
        } else {
            None
        };
    Ok(FragmentContext {
        name: xmloxide_context(namespace, &name),
        form,
        encoding,
        is_node: true,
    })
}

/// The context element in xmloxide's notation. Gumbo looks the name up
/// ignoring case (`gumbo_tagn_enum`), but xmloxide compares names exactly:
/// lower case, but for SVG's camel-case names, of which only foreignObject
/// (an HTML integration point) changes how a fragment parses.
fn xmloxide_context(namespace: ContextNamespace, name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match namespace {
        ContextNamespace::Html => name,
        ContextNamespace::Svg if name == "foreignobject" => "svg foreignObject".to_owned(),
        ContextNamespace::Svg => format!("svg {name}"),
        ContextNamespace::MathMl => format!("math {name}"),
    }
}

/// The compatibility mode a fragment is parsed in, as `gumbo.c` works it
/// out: the document's, from its DTD, if the context is a node of a parsed
/// document; otherwise no quirks.
fn fragment_quirks_mode(context: &FragmentContext, document: Value) -> Result<QuirksMode, Error> {
    let parsed_mode: Value = document.funcall("instance_variable_get", ("@quirks_mode",))?;
    if !context.is_node || parsed_mode.is_nil() {
        return Ok(QuirksMode::NoQuirks);
    }
    let dtd: Value = document.funcall("internal_subset", ())?;
    if dtd.is_nil() {
        return Ok(QuirksMode::Quirks);
    }
    let field = |method: &str| -> Result<Option<String>, Error> {
        let value: Value = dtd.funcall(method, ())?;
        Option::<String>::try_convert(value)
    };
    let name = field("name")?;
    let public_id = field("external_id")?;
    let system_id = field("system_id")?;
    Ok(doctype_quirks_mode(
        name.as_deref().unwrap_or(""),
        public_id.as_deref(),
        system_id.as_deref(),
    ))
}

/// `Gumbo.fragment(fragment, input, context, **options)`: parses `input` in
/// `context` into the (empty) `fragment`.
fn fragment(ruby: &Ruby, args: &[Value]) -> Result<Value, Error> {
    let args = scan_args::<(Value, RString, Value), (), (), (), RHash, ()>(args)?;
    let (rb_fragment, input, context) = args.required;
    let mut options = common_options(args.keywords)?;
    let context = fragment_context(ruby, context)?;
    let document: Value = rb_fragment.funcall("document", ())?;
    options.fragment_quirks_mode = fragment_quirks_mode(&context, document)?;
    options.fragment_form_ancestor = context.form;
    options.fragment_context_encoding = context.encoding;
    options.fragment_context = Some(context.name);
    // Gumbo counts the html element the fragment is parsed under.
    options.max_tree_depth = options.max_tree_depth.map(|d| d.saturating_add(1));
    let text = input_text(input);
    let result = perform_parse(ruby, &options, &text)?;

    let frag = Receiver::of_value(ruby, rb_fragment)?;
    let Some(frag_id) = frag.tree_id() else {
        return Err(Error::new(
            ruby.exception_type_error(),
            "not a document fragment",
        ));
    };
    // The parsed nodes are the html element's children, or, in a foreign
    // context, those of the context element under it.
    let parsed = &result.document;
    let html = parsed.root_element();
    let foreign = options
        .fragment_context
        .as_deref()
        .is_some_and(|c| c.contains(' '));
    let parent = if foreign {
        html.and_then(|h| parsed.first_child(h))
    } else {
        html
    };
    {
        let mut tree = frag.tree_mut(ruby)?;
        if let Some(parent) = parent {
            for child in parsed.children(parent).collect::<Vec<_>>() {
                let copy = tree.import_node(parsed, child);
                tree.append_child(frag_id, copy);
            }
        }
        reshape(&mut tree, frag_id);
    }
    let fragment_file = ruby.str_new("#fragment").as_value();
    let errors = errors(ruby, &result, &text, fragment_file)?;
    let object = Obj::<NodeObj>::try_convert(rb_fragment)?;
    object.ivar_set("@quirks_mode", quirks_mode_value(result.quirks_mode))?;
    if let Some(errors) = errors {
        object.ivar_set("@errors", errors)?;
    }
    Ok(ruby.qnil().as_value())
}

// --- Serializing ----------------------------------------------------------

/// Elements in no namespace (HTML) named `name` that are one of `names`.
fn is_html_one_of(kind: &NodeKind, names: &[&str]) -> bool {
    matches!(kind, NodeKind::Element { name, namespace: None, .. } if names.contains(&name.as_str()))
}

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];
const UNESCAPED_TEXT_ELEMENTS: &[&str] = &[
    "style",
    "script",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "plaintext",
];

/// `should_prepend_newline`: a `pre`, `textarea` or `listing` whose first
/// child is text starting with a newline, which the HTML syntax would drop.
fn should_prepend_newline(tree: &Document, id: NodeId) -> bool {
    let NodeKind::Element { name, .. } = &tree.node(id).kind else {
        return false;
    };
    if !matches!(name.as_str(), "pre" | "textarea" | "listing") {
        return false;
    }
    tree.first_child(id).is_some_and(|child| {
        matches!(&tree.node(child).kind, NodeKind::Text { content } if content.starts_with('\n'))
    })
}

fn prepend_newline(ruby: &Ruby, rb_self: &NodeObj) -> Result<bool, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    Ok(rcv
        .tree_id()
        .is_some_and(|id| should_prepend_newline(&rcv.tree(), id)))
}

fn escape(out: &mut String, text: &str, attribute: bool) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            '"' if attribute => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

/// `output_tagname`: elements in the HTML, SVG and MathML namespaces are
/// written without their prefix.
fn tag_name(prefix: Option<&str>, namespace: Option<&str>, name: &str, out: &mut String) {
    if let (Some(prefix), Some(href)) = (prefix, namespace)
        && !matches!(href, XHTML | MATHML | SVG)
    {
        out.push_str(prefix);
        out.push(':');
    }
    out.push_str(name);
}

/// `output_attr_name`. A namespaced attribute is written with the prefix
/// the HTML syntax gives its namespace and its local name (the part of its
/// name after any colon).
fn attribute_name(attribute: &xmloxide::tree::Attribute, out: &mut String) {
    let name = attribute.name.as_str();
    let Some(namespace) = attribute.namespace.as_deref() else {
        out.push_str(name);
        return;
    };
    let local = name.split_once(':').map_or(name, |(_, local)| local);
    match namespace {
        namespace::XML_NAMESPACE => out.push_str("xml:"),
        XMLNS => {
            if local != "xmlns" {
                out.push_str("xmlns:");
            }
        }
        XLINK => out.push_str("xlink:"),
        _ => match &attribute.prefix {
            Some(prefix) => {
                out.push_str(prefix);
                out.push(':');
            }
            None => {
                out.push_str(name);
                return;
            }
        },
    }
    out.push_str(local);
}

fn write_attribute(attribute: &xmloxide::tree::Attribute, out: &mut String) {
    attribute_name(attribute, out);
    out.push_str("=\"");
    escape(out, &attribute.value, true);
    out.push('"');
}

/// `output_node`.
fn output_node(
    ruby: &Ruby,
    tree: &Document,
    id: NodeId,
    preserve_newline: bool,
    scripting: bool,
    out: &mut String,
) -> Result<(), Error> {
    match &tree.node(id).kind {
        NodeKind::Element {
            name,
            prefix,
            namespace: ns,
            attributes,
        } => {
            out.push('<');
            tag_name(prefix.as_deref(), ns.as_deref(), name, out);
            for attribute in attributes
                .iter()
                .filter(|a| namespace::declaration(a).is_none())
            {
                out.push(' ');
                write_attribute(attribute, out);
            }
            out.push('>');
            if !is_html_one_of(&tree.node(id).kind, VOID_ELEMENTS) {
                if preserve_newline && should_prepend_newline(tree, id) {
                    out.push('\n');
                }
                for child in tree.children(id) {
                    output_node(ruby, tree, child, preserve_newline, scripting, out)?;
                }
                out.push_str("</");
                tag_name(prefix.as_deref(), ns.as_deref(), name, out);
                out.push('>');
            }
        }
        NodeKind::Text { content } => {
            let raw = tree.parent(id).is_some_and(|parent| {
                let kind = &tree.node(parent).kind;
                is_html_one_of(kind, UNESCAPED_TEXT_ELEMENTS)
                    || (scripting && is_html_one_of(kind, &["noscript"]))
            });
            if raw {
                out.push_str(content);
            } else {
                escape(out, content, false);
            }
        }
        NodeKind::CData { content } => {
            out.push_str("<![CDATA[");
            out.push_str(content);
            out.push_str("]]>");
        }
        NodeKind::Comment { content } => {
            out.push_str("<!--");
            out.push_str(content);
            out.push_str("-->");
        }
        NodeKind::ProcessingInstruction { data, .. } => {
            out.push_str("<?");
            out.push_str(data.as_deref().unwrap_or(""));
            out.push('>');
        }
        NodeKind::DocumentType { name, .. } => {
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            out.push('>');
        }
        NodeKind::Document | NodeKind::DocumentFragment => {
            for child in tree.children(id) {
                output_node(ruby, tree, child, preserve_newline, scripting, out)?;
            }
        }
        NodeKind::EntityRef { .. } => {
            return Err(Error::new(
                ruby.exception_runtime_error(),
                "Unsupported document node (5); this is a bug in Nokogiri",
            ));
        }
    }
    Ok(())
}

/// `Node#html_standard_serialize(preserve_newline, scripting_enabled)`
/// (private): the node in HTML syntax, per the HTML standard's fragment
/// serialization algorithm, but including the node itself.
fn html_standard_serialize(
    ruby: &Ruby,
    rb_self: &NodeObj,
    preserve_newline: Value,
    scripting: Value,
) -> Result<RString, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let mut out = String::with_capacity(4096);
    {
        let tree = rcv.tree();
        match &rcv.key {
            NodeKey::Tree(id) => output_node(
                ruby,
                &tree,
                *id,
                preserve_newline.to_bool(),
                scripting.to_bool(),
                &mut out,
            )?,
            key @ NodeKey::Attr { .. } => {
                if let Some(attribute) = find_attr(&tree, key) {
                    write_attribute(attribute, &mut out);
                }
            }
        }
    }
    Ok(ruby.str_new(&out))
}

#[cfg(test)]
mod tests {
    use super::{ContextNamespace, Positions, caret_diagnostic, xmloxide_context};

    #[test]
    fn columns_follow_gumbo_tab_stops() {
        let positions = Positions::new("x\nab\t\tc<");
        // a, b, then the tabs to 8 and 16, then c.
        assert_eq!(positions.column(7), 17);
        assert_eq!(positions.column(2), 1);
        assert_eq!(Positions::new("\t<").column(1), 8);
        assert_eq!(Positions::new("\u{e9}<").column(2), 2);
    }

    #[test]
    fn offsets_map_back_to_the_input_as_given() {
        let input = "a\r\nb\rc\r\nd";
        let positions = Positions::new(input);
        assert_eq!(positions.normalized, "a\nb\nc\nd");
        let original = |offset| &input[positions.original_offset(offset)..];
        assert_eq!(original(2), "b\rc\r\nd");
        assert_eq!(original(4), "c\r\nd");
        assert_eq!(original(6), "d");
        assert_eq!(
            caret_diagnostic("m", input, positions.original_offset(4), 1),
            "m\nb\rc\r\n^\n"
        );
    }

    #[test]
    fn context_names_are_looked_up_ignoring_case() {
        assert_eq!(xmloxide_context(ContextNamespace::Html, "TABLE"), "table");
        assert_eq!(
            xmloxide_context(ContextNamespace::Svg, "FOREIGNOBJECT"),
            "svg foreignObject"
        );
        assert_eq!(
            xmloxide_context(ContextNamespace::MathMl, "Annotation-XML"),
            "math annotation-xml"
        );
    }

    #[test]
    fn caret_points_at_the_column() {
        let input = "one\ntwo <x\nthree";
        assert_eq!(
            caret_diagnostic("eof-in-tag", input, 8, 5),
            "eof-in-tag\ntwo <x\n    ^\n"
        );
    }

    #[test]
    fn parser_messages_name_the_token_and_open_tags() {
        use super::parser_message;
        use xmloxide::html5::Html5ErrorToken;
        let stack = ["html".to_owned(), "foo".to_owned(), "table".to_owned()];
        assert_eq!(
            parser_message(&Html5ErrorToken::StartTag("foo".to_owned()), false, &stack),
            "Start tag 'foo' isn't allowed here. Currently open tags: html, foo, table."
        );
        assert_eq!(
            parser_message(&Html5ErrorToken::StartTag("p".to_owned()), true, &[]),
            "Expected a doctype token"
        );
    }

    #[test]
    fn caret_at_end_of_input_shows_the_last_line() {
        assert_eq!(caret_diagnostic("m", "ab", 2, 3), "m\nab\n  ^\n");
    }
}
