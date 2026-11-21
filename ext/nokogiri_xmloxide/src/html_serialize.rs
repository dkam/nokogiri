//! libxml2 2.14's HTML serializer, for `Node#write_to` on a node of an HTML
//! document, or with `SaveOptions::AS_HTML`: `htmlNodeDumpOutputInternal`
//! (xmlsave.c), `htmlNodeDumpFormatOutput` and `htmlSetMetaEncoding`
//! (HTMLtree.c), and the escaping and output encoding they use.

use std::fmt::Write as _;

use xmloxide::tree::{Attribute, NodeKind};
use xmloxide::{Document, NodeId};

use crate::document::doctype;
use crate::encoding_handler::Codec;
use crate::html4_tables;
use crate::namespace;

/// Serializes `id` as `htmlNodeDumpOutputInternal` does, into bytes in the
/// output encoding: `encoding` if the caller gave one, else the document's,
/// else its `<meta>` charset's, else libxml2's "HTML" encoding, which writes
/// characters beyond ASCII as entities. As in libxml2, writing an HTML
/// document in an encoding first records it in the document's `<meta>`
/// charset, adding one to its `head` if there is none.
pub fn serialize(
    tree: &mut Document,
    target: &Target,
    html_document: bool,
    encoding: Option<&str>,
    format: bool,
) -> Vec<u8> {
    // libxml2 can't write in an encoding it doesn't know: for the caller's,
    // it can't open a save context, so leaves the document as it is.
    let output = match encoding {
        Some(name) => {
            let Some(output) = Output::named(name) else {
                return Vec::new();
            };
            set_meta_encoding(tree, name);
            output
        }
        None => {
            let name = match tree.encoding.clone() {
                Some(name) => {
                    set_meta_encoding(tree, &name);
                    Some(name)
                }
                None => meta_encoding(tree),
            };
            match name.as_deref().map_or(Some(Output::Html), Output::named) {
                Some(output) => output,
                None => return Vec::new(),
            }
        }
    };
    let writer = Writer {
        tree,
        html_document,
        non_ascii: !html_document && tree.encoding.is_none(),
        format,
    };
    let mut out = String::new();
    match target {
        Target::Node(id) => writer.node(*id, &mut out),
        Target::Attribute { owner, attribute } => writer.attribute(attribute, *owner, &mut out),
    }
    output.encode(out)
}

/// What to serialize: a node, or an attribute (a copy of it, since the
/// serializer may change the document's `<meta>` first) of `owner`.
pub enum Target {
    Node(NodeId),
    Attribute { owner: NodeId, attribute: Attribute },
}

struct Writer<'a> {
    tree: &'a Document,
    /// Text is escaped as libxml2 escapes it in an HTML document
    /// (`XML_ESCAPE_HTML`): no character references for `\r` or for
    /// characters beyond ASCII, and control characters kept.
    html_document: bool,
    /// Characters beyond ASCII become hexadecimal character references
    /// (`XML_ESCAPE_NON_ASCII`), as for an XML document with no encoding.
    non_ascii: bool,
    format: bool,
}

impl Writer<'_> {
    fn node(&self, id: NodeId, out: &mut String) {
        let tree = self.tree;
        match &tree.node(id).kind {
            NodeKind::Document => {
                if let Some(dtd) = doctype(tree) {
                    self.doctype(dtd, out);
                }
                // The DOCTYPE is among the children, where it writes nothing.
                for child in tree.children(id) {
                    self.node(child, out);
                }
                out.push('\n');
            }
            NodeKind::Element { .. } => self.element(id, out),
            NodeKind::Text { content } => {
                let raw = tree.parent(id).is_some_and(|parent| {
                    element_name(tree, parent).is_some_and(|name| {
                        name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style")
                    })
                });
                if raw {
                    out.push_str(content);
                } else {
                    self.escape(content, out);
                }
            }
            NodeKind::Comment { content } => {
                out.push_str("<!--");
                out.push_str(content);
                out.push_str("-->");
            }
            NodeKind::ProcessingInstruction { target, data } => {
                out.push_str("<?");
                out.push_str(target);
                if let Some(data) = data {
                    out.push(' ');
                    out.push_str(data);
                }
                out.push('>');
            }
            NodeKind::EntityRef { name, .. } => {
                out.push('&');
                out.push_str(name);
                out.push(';');
            }
            // HTML_PRESERVE_NODE: as it is, unescaped.
            NodeKind::CData { content } => out.push_str(content),
            // libxml2's HTML serializer writes neither of these.
            NodeKind::DocumentType { .. } | NodeKind::DocumentFragment => {}
        }
    }

    fn element(&self, id: NodeId, out: &mut String) {
        let tree = self.tree;
        let NodeKind::Element {
            name,
            prefix,
            namespace: element_namespace,
            attributes,
        } = &tree.node(id).kind
        else {
            return;
        };
        let info = if element_namespace.is_none() {
            html4_tables::lookup(name)
        } else {
            None
        };
        let qname = |out: &mut String| {
            if let Some(prefix) = prefix {
                out.push_str(prefix);
                out.push(':');
            }
            out.push_str(name);
        };
        out.push('<');
        qname(out);
        for (prefix, href) in attributes.iter().filter_map(namespace::declaration) {
            if prefix == Some("xml") {
                continue;
            }
            out.push_str(" xmlns");
            if let Some(prefix) = prefix {
                out.push(':');
                out.push_str(prefix);
            }
            out.push_str("=\"");
            escape_namespace(href, out);
            out.push('"');
        }
        for attribute in attributes
            .iter()
            .filter(|a| namespace::declaration(a).is_none())
        {
            self.attribute(attribute, id, out);
        }

        // With FORMAT, a block element gets line breaks around it, and
        // inside it if it has several children, unless its name starts with
        // "p" (p, pre, param).
        let block = self.format && info.is_some_and(|info| info.inline == 0);
        let void = info.is_some_and(|info| info.empty != 0);
        match tree.first_child(id) {
            Some(first) if !void => {
                out.push('>');
                let last = tree.last_child(id).unwrap_or(first);
                let break_inside = block && first != last && !name.starts_with('p');
                if break_inside && !is_text_like(tree, first) {
                    out.push('\n');
                }
                for child in tree.children(id) {
                    self.node(child, out);
                }
                if break_inside && !is_text_like(tree, last) {
                    out.push('\n');
                }
                out.push_str("</");
                qname(out);
                out.push('>');
            }
            // A void element ends at its start tag, leaving out any
            // children, as does an empty one whose end tag may be left out
            // (except html and body).
            _ if void
                || info.is_some_and(|info| {
                    info.save_end_tag != 0 && info.name != "html" && info.name != "body"
                }) =>
            {
                out.push('>');
            }
            _ => {
                out.push_str("></");
                qname(out);
                out.push('>');
            }
        }
        if block {
            self.break_before_next(id, out);
        }
    }

    /// The line break after a block element: before a next sibling that
    /// isn't text, in a parent whose name doesn't start with "p" (p, pre,
    /// param). A document or fragment has no name, so its children get none.
    fn break_before_next(&self, id: NodeId, out: &mut String) {
        let tree = self.tree;
        let Some(next) = tree.next_sibling(id) else {
            return;
        };
        let parent_name = tree
            .parent(id)
            .and_then(|parent| element_name(tree, parent));
        if !is_text_like(tree, next) && parent_name.is_some_and(|name| !name.starts_with('p')) {
            out.push('\n');
        }
    }

    /// `htmlAttrDumpOutput`, for an attribute of `owner`.
    fn attribute(&self, attribute: &Attribute, owner: NodeId, out: &mut String) {
        out.push(' ');
        let in_no_namespace = namespace::attribute_namespace(attribute).is_none();
        if let (Some(prefix), false) = (&attribute.prefix, in_no_namespace) {
            out.push_str(prefix);
            out.push(':');
        }
        out.push_str(&attribute.name);
        if is_boolean_attribute(&attribute.name) {
            return;
        }
        let mut value = String::new();
        self.escape(&attribute.value, &mut value);
        out.push('=');
        let is_uri = in_no_namespace
            && self.tree.node_namespace(owner).is_none()
            && (["href", "action", "src"]
                .iter()
                .any(|uri| attribute.name.eq_ignore_ascii_case(uri))
                || (attribute.name.eq_ignore_ascii_case("name")
                    && element_name(self.tree, owner)
                        .is_some_and(|e| e.eq_ignore_ascii_case("a"))));
        if is_uri {
            let trimmed = value.trim_start_matches([' ', '\t', '\n', '\r']);
            write_quoted(&uri_escape(trimmed), out);
        } else {
            write_quoted(&value, out);
        }
    }

    /// `xmlEscapeText` as `xmlEncodeEntitiesReentrant` calls it.
    fn escape(&self, text: &str, out: &mut String) {
        let mut run = 0;
        for (i, c) in text.char_indices() {
            let replacement: std::borrow::Cow<'static, str> = match c {
                '<' => "&lt;".into(),
                '>' => "&gt;".into(),
                '&' => "&amp;".into(),
                '\r' if !self.html_document => "&#13;".into(),
                c if self.non_ascii && !c.is_ascii() => {
                    let c = if is_xml_char(c) { c } else { '\u{FFFD}' };
                    format!("&#x{:X};", u32::from(c)).into()
                }
                c if !self.html_document
                    && u32::from(c) < 0x20
                    && !matches!(c, '\t' | '\n' | '\r') =>
                {
                    "".into()
                }
                _ => continue,
            };
            out.push_str(&text[run..i]);
            out.push_str(&replacement);
            run = i + c.len_utf8();
        }
        out.push_str(&text[run..]);
    }

    /// `htmlDtdDumpOutput`.
    fn doctype(&self, id: NodeId, out: &mut String) {
        let NodeKind::DocumentType {
            name,
            public_id,
            system_id,
            ..
        } = &self.tree.node(id).kind
        else {
            return;
        };
        out.push_str("<!DOCTYPE ");
        out.push_str(name);
        if let Some(public_id) = public_id {
            out.push_str(" PUBLIC ");
            write_quoted(public_id, out);
            if let Some(system_id) = system_id {
                out.push(' ');
                write_quoted(system_id, out);
            }
        } else if let Some(system_id) = system_id.as_deref().filter(|s| *s != "about:legacy-compat")
        {
            out.push_str(" SYSTEM ");
            write_quoted(system_id, out);
        }
        out.push_str(">\n");
    }
}

fn element_name(tree: &Document, id: NodeId) -> Option<&str> {
    match &tree.node(id).kind {
        NodeKind::Element { name, .. } => Some(name),
        _ => None,
    }
}

/// Text and entity references, after which libxml2 adds no line break.
fn is_text_like(tree: &Document, id: NodeId) -> bool {
    matches!(
        tree.node(id).kind,
        NodeKind::Text { .. } | NodeKind::EntityRef { .. }
    )
}

/// `htmlIsBooleanAttr`: written with no value, as HTML allows.
fn is_boolean_attribute(name: &str) -> bool {
    [
        "checked", "compact", "declare", "defer", "disabled", "ismap", "multiple", "nohref",
        "noresize", "noshade", "nowrap", "readonly", "selected",
    ]
    .iter()
    .any(|boolean| name.eq_ignore_ascii_case(boolean))
}

fn is_xml_char(c: char) -> bool {
    matches!(u32::from(c), 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10_FFFF)
}

/// `xmlOutputBufferWriteQuotedString`: in double quotes, or in single
/// quotes if the value has a double quote and no single one; with both,
/// double quotes become `&quot;`.
fn write_quoted(value: &str, out: &mut String) {
    if !value.contains('"') {
        out.push('"');
        out.push_str(value);
        out.push('"');
    } else if !value.contains('\'') {
        out.push('\'');
        out.push_str(value);
        out.push('\'');
    } else {
        out.push('"');
        out.push_str(&value.replace('"', "&quot;"));
        out.push('"');
    }
}

/// `xmlURIEscapeStr` with the characters `htmlAttrDumpOutput` keeps: each
/// byte that isn't unreserved, `@`, or one of them is percent-encoded.
fn uri_escape(value: &str) -> String {
    const KEEP: &[u8] = b"\"#$%&+,/:;<=>?@[\\]^`{|}-_.!~*'()";
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || KEEP.contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `xmlNsDumpOutput` without a save context: `xmlSerializeText` with
/// `XML_ESCAPE_ATTR | XML_ESCAPE_NON_ASCII`.
fn escape_namespace(href: &str, out: &mut String) {
    for c in href.chars() {
        match c {
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if !c.is_ascii() => out.push_str(&format!("&#x{:X};", u32::from(c))),
            c if u32::from(c) < 0x20 => {}
            c => out.push(c),
        }
    }
}

// --- The <meta> charset ----------------------------------------------------

/// The `http-equiv="Content-Type"` meta element `htmlGetMetaEncoding` and
/// `htmlSetMetaEncoding` look at, with its content, and the element a new
/// one would go in. They take the first html, head or meta element at the
/// top level, and in an html element the first head or meta; then search a
/// head's children, or a meta and its next siblings.
fn find_meta(tree: &Document, case_insensitive: bool) -> (Option<NodeId>, Option<(NodeId, &str)>) {
    let is = |id: NodeId, wanted: &str| {
        element_name(tree, id).is_some_and(|name| {
            if case_insensitive {
                name.eq_ignore_ascii_case(wanted)
            } else {
                name == wanted
            }
        })
    };
    let first = |parent: NodeId, names: &[&str]| {
        tree.children(parent)
            .find(|&id| names.iter().any(|name| is(id, name)))
    };
    let mut head = None;
    let mut found = first(tree.root(), &["html", "head", "meta"]);
    if let Some(html) = found.filter(|&id| is(id, "html")) {
        found = first(html, &["head", "meta"]);
        // A meta among the html element's children goes before them.
        if found.is_some_and(|id| is(id, "meta")) {
            head = Some(html);
        }
    }
    let start = match found {
        Some(id) if is(id, "head") => {
            head = Some(id);
            tree.first_child(id)
        }
        start => start,
    };
    let meta = std::iter::successors(start, |&id| tree.next_sibling(id))
        .filter(|&id| is(id, "meta"))
        .find_map(|id| Some((id, content_type(tree, id)?)));
    (head, meta)
}

/// A meta element's `content` attribute, if it has `http-equiv` of
/// Content-Type.
fn content_type(tree: &Document, meta: NodeId) -> Option<&str> {
    let mut http = false;
    let mut content = None;
    // As libxml2, an attribute with no value (no text child) is skipped.
    for attribute in tree.attributes(meta).iter().filter(|a| !a.value.is_empty()) {
        if attribute.name.eq_ignore_ascii_case("http-equiv")
            && attribute.value.eq_ignore_ascii_case("Content-Type")
        {
            http = true;
        } else if attribute.name.eq_ignore_ascii_case("content") {
            content = Some(attribute.value.as_str());
        }
        if http && content.is_some() {
            return content;
        }
    }
    None
}

/// `htmlGetMetaEncoding`: the charset in the document's Content-Type meta.
fn meta_encoding(tree: &Document) -> Option<String> {
    let (_, meta) = find_meta(tree, false);
    let (_, content) = meta?;
    let after = [
        "charset=",
        "Charset=",
        "CHARSET=",
        "charset =",
        "Charset =",
        "CHARSET =",
    ]
    .iter()
    .find_map(|key| content.find(key).map(|at| at + key.len()))?;
    Some(content[after..].trim_start_matches([' ', '\t']).to_owned())
}

/// `htmlSetMetaEncoding`: points the document's Content-Type meta at
/// `encoding`, unless it already names it, or adds one at the start of the
/// head.
fn set_meta_encoding(tree: &mut Document, encoding: &str) {
    // "HTML" isn't an encoding; it's libxml2's way of asking for entities.
    if encoding.eq_ignore_ascii_case("html") {
        return;
    }
    let (head, meta) = find_meta(tree, true);
    let meta = meta.map(|(id, current)| (id, contains_ignore_ascii_case(current, encoding)));
    let content = || format!("text/html; charset={encoding}");
    match (head, meta) {
        (_, Some((_, true))) | (None, None) => {}
        (_, Some((meta, false))) => {
            tree.set_attribute(meta, "content", &content());
        }
        (Some(head), None) => {
            let meta = tree.create_element("meta");
            tree.set_attribute(meta, "http-equiv", "Content-Type");
            tree.set_attribute(meta, "content", &content());
            match tree.first_child(head) {
                Some(first) => tree.insert_before(first, meta),
                None => tree.append_child(head, meta),
            }
        }
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    needle.is_empty()
        || haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

// --- Output encoding --------------------------------------------------------

/// The encoding output is written in: libxml2's "HTML", which writes each
/// character beyond ASCII as its HTML 4 entity or a decimal character
/// reference, or a real one.
#[derive(Clone, Copy)]
enum Output {
    Html,
    Codec(Codec),
}

impl Output {
    fn named(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("html") {
            Some(Output::Html)
        } else {
            Codec::named(name).map(Output::Codec)
        }
    }

    fn encode(self, text: String) -> Vec<u8> {
        let Output::Codec(codec) = self else {
            if text.is_ascii() {
                return text.into_bytes();
            }
            let mut out = String::with_capacity(text.len() + text.len() / 4);
            for c in text.chars() {
                if c.is_ascii() {
                    out.push(c);
                } else if let Some(name) = html4_tables::entity_for(u32::from(c)) {
                    out.push('&');
                    out.push_str(name);
                    out.push(';');
                } else {
                    let _ = write!(out, "&#{};", u32::from(c));
                }
            }
            return out.into_bytes();
        };
        codec.encode(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_attributes_escape_spaces_and_non_ascii() {
        assert_eq!(
            uri_escape("/a b?x=1&amp;y=&lt;2&gt;#f\u{e9}"),
            "/a%20b?x=1&amp;y=&lt;2&gt;#f%C3%A9"
        );
    }

    #[test]
    fn quotes_follow_the_value() {
        let quoted = |value: &str| {
            let mut out = String::new();
            write_quoted(value, &mut out);
            out
        };
        assert_eq!(quoted("a"), "\"a\"");
        assert_eq!(quoted("a\"b"), "'a\"b'");
        assert_eq!(quoted("a\"b'c"), "\"a&quot;b'c\"");
    }

    #[test]
    fn the_html_encoding_writes_entities() {
        assert_eq!(
            Output::Html.encode("caf\u{e9} \u{2713} \u{20ac}".to_owned()),
            b"caf&eacute; &#10003; &euro;"
        );
        assert_eq!(
            Output::named("ISO-8859-1")
                .unwrap()
                .encode("caf\u{e9} \u{2713}".to_owned()),
            b"caf\xE9 &#10003;"
        );
    }
}
