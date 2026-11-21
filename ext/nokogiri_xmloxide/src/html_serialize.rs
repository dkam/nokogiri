//! libxml2 2.14's HTML serializer, for `Node#write_to` on a node of an HTML
//! document, or with `SaveOptions::AS_HTML`: `htmlNodeDumpOutputInternal`
//! (xmlsave.c), `htmlNodeDumpFormatOutput` and `htmlSetMetaEncoding`
//! (HTMLtree.c), and the escaping and output encoding they use.

use xmloxide::tree::{Attribute, NodeKind};
use xmloxide::{Document, NodeId};

use crate::document::doctype;
use crate::encoding_handler;
use crate::html4_tables::{self, ElementInfo};
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
    let named = encoding
        .map(str::to_owned)
        .or_else(|| tree.encoding.clone());
    if let Some(name) = &named {
        set_meta_encoding(tree, name);
    }
    let output_encoding = encoding
        .map(str::to_owned)
        .or(named)
        .or_else(|| meta_encoding(tree))
        .unwrap_or_else(|| "HTML".to_owned());
    let writer = Writer {
        tree,
        html_document,
        non_ascii: !html_document && tree.encoding.is_none(),
        format,
    };
    let mut out = String::new();
    match target {
        Target::Node(id) => writer.node(*id, &mut out),
        Target::Attribute { owner, attribute } => {
            let (element, in_no_namespace) = match &tree.node(*owner).kind {
                NodeKind::Element {
                    name, namespace, ..
                } => (name.as_str(), namespace.is_none()),
                _ => ("", true),
            };
            writer.attribute(attribute, element, in_no_namespace, &mut out);
        }
    }
    encode(&out, &output_encoding)
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
            // libxml2's HTML serializer writes none of these.
            NodeKind::CData { .. } | NodeKind::DocumentType { .. } | NodeKind::DocumentFragment => {
            }
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
            self.attribute(attribute, name, element_namespace.is_none(), out);
        }

        let block = |info: Option<&ElementInfo>| info.is_some_and(|info| info.inline == 0);
        let Some(first) = tree.first_child(id) else {
            // A void element, or one whose end tag may be left out (except
            // html and body), ends at its start tag.
            if info.is_some_and(|info| {
                info.empty != 0
                    || (info.save_end_tag != 0 && info.name != "html" && info.name != "body")
            }) {
                out.push('>');
            } else {
                out.push_str("></");
                qname(out);
                out.push('>');
            }
            if self.format && block(info) {
                self.break_before_next(id, out);
            }
            return;
        };

        out.push('>');
        let last = tree.last_child(id).unwrap_or(first);
        let several = first != last;
        if self.format
            && block(info)
            && !is_text_like(tree, first)
            && several
            && !name.starts_with('p')
        {
            out.push('\n');
        }
        for child in tree.children(id) {
            self.node(child, out);
        }
        if self.format
            && block(info)
            && !is_text_like(tree, last)
            && several
            && !name.starts_with('p')
        {
            out.push('\n');
        }
        out.push_str("</");
        qname(out);
        out.push('>');
        if self.format && block(info) {
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

    /// `htmlAttrDumpOutput`.
    fn attribute(
        &self,
        attribute: &Attribute,
        element: &str,
        element_in_no_namespace: bool,
        out: &mut String,
    ) {
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
            && element_in_no_namespace
            && ["href", "action", "src"]
                .iter()
                .any(|uri| attribute.name.eq_ignore_ascii_case(uri))
            || (attribute.name.eq_ignore_ascii_case("name") && element.eq_ignore_ascii_case("a"));
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
/// `htmlSetMetaEncoding` look at, and the head they look in: the first such
/// meta among the document's top-level elements, the html element's, or the
/// head's, depending on which of those comes first.
fn find_meta(tree: &Document, case_insensitive: bool) -> (Option<NodeId>, Option<NodeId>) {
    let is = |id: NodeId, wanted: &str| {
        element_name(tree, id).is_some_and(|name| {
            if case_insensitive {
                name.eq_ignore_ascii_case(wanted)
            } else {
                name == wanted
            }
        })
    };
    let elements = |parent: NodeId| {
        tree.children(parent)
            .filter(|&child| element_name(tree, child).is_some())
            .collect::<Vec<_>>()
    };
    let content_meta = |siblings: &[NodeId]| {
        siblings
            .iter()
            .copied()
            .find(|&id| is(id, "meta") && content_type(tree, id).is_some())
    };

    let top = elements(tree.root());
    let (head, siblings) = 'found: {
        for (i, &id) in top.iter().enumerate() {
            if is(id, "html") {
                let inner = elements(id);
                for (j, &id) in inner.iter().enumerate() {
                    if is(id, "head") {
                        break 'found (Some(id), elements(id));
                    }
                    if is(id, "meta") {
                        break 'found (tree.parent(id), inner[j..].to_vec());
                    }
                }
                return (None, None);
            }
            if is(id, "head") {
                break 'found (Some(id), elements(id));
            }
            if is(id, "meta") {
                break 'found (None, top[i..].to_vec());
            }
        }
        return (None, None);
    };
    (head, content_meta(&siblings))
}

/// A meta element's `content` attribute, if it has `http-equiv` of
/// Content-Type.
fn content_type(tree: &Document, meta: NodeId) -> Option<String> {
    let NodeKind::Element { attributes, .. } = &tree.node(meta).kind else {
        return None;
    };
    let mut http = false;
    let mut content = None;
    // As libxml2, an attribute with no value (no text child) is skipped.
    for attribute in attributes.iter().filter(|a| !a.value.is_empty()) {
        if attribute.name.eq_ignore_ascii_case("http-equiv")
            && attribute.value.eq_ignore_ascii_case("Content-Type")
        {
            http = true;
        } else if attribute.name.eq_ignore_ascii_case("content") {
            content = Some(attribute.value.clone());
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
    let content = content_type(tree, meta?)?;
    let after = ["charset=", "Charset=", "CHARSET="]
        .iter()
        .find_map(|key| content.find(key).map(|at| at + key.len()))
        .or_else(|| {
            ["charset =", "Charset =", "CHARSET ="]
                .iter()
                .find_map(|key| content.find(key).map(|at| at + key.len()))
        })?;
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
    let content = format!("text/html; charset={encoding}");
    match find_meta(tree, true) {
        (_, Some(meta)) => {
            let current = content_type(tree, meta).unwrap_or_default();
            if !current
                .to_ascii_lowercase()
                .contains(&encoding.to_ascii_lowercase())
            {
                tree.set_attribute(meta, "content", &content);
            }
        }
        (Some(head), None) => {
            let meta = tree.create_element("meta");
            tree.set_attribute(meta, "http-equiv", "Content-Type");
            tree.set_attribute(meta, "content", &content);
            match tree.first_child(head) {
                Some(first) => tree.insert_before(first, meta),
                None => tree.append_child(head, meta),
            }
        }
        (None, None) => {}
    }
}

// --- Output encoding --------------------------------------------------------

/// The serialized text in `encoding`. libxml2's "HTML" encoding writes each
/// character beyond ASCII as its HTML 4 entity, or a decimal character
/// reference; other encodings write a character they can't represent as a
/// decimal character reference.
fn encode(text: &str, encoding: &str) -> Vec<u8> {
    if encoding.eq_ignore_ascii_case("html") {
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if c.is_ascii() {
                out.push(c);
            } else {
                match html4_tables::entity_for(u32::from(c)) {
                    Some(name) => {
                        out.push('&');
                        out.push_str(name);
                        out.push(';');
                    }
                    None => out.push_str(&format!("&#{};", u32::from(c))),
                }
            }
        }
        return out.into_bytes();
    }
    let real = encoding_handler::resolve_alias(encoding);
    let label = real.as_deref().unwrap_or(encoding).trim();
    if ["ISO-8859-1", "ISO8859-1", "ISO_8859-1", "LATIN1", "L1"]
        .iter()
        .any(|name| label.eq_ignore_ascii_case(name))
    {
        let mut out = Vec::with_capacity(text.len());
        for c in text.chars() {
            match u8::try_from(u32::from(c)) {
                Ok(byte) => out.push(byte),
                Err(_) => out.extend_from_slice(format!("&#{};", u32::from(c)).as_bytes()),
            }
        }
        return out;
    }
    match encoding_rs::Encoding::for_label(label.as_bytes()) {
        // encoding_rs writes unmappable characters as decimal references,
        // as libxml2 does.
        Some(encoding) if encoding != encoding_rs::UTF_8 => encoding.encode(text).0.into_owned(),
        _ => text.as_bytes().to_vec(),
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
            encode("caf\u{e9} \u{2713} \u{20ac}", "HTML"),
            b"caf&eacute; &#10003; &euro;"
        );
        assert_eq!(
            encode("caf\u{e9} \u{2713}", "ISO-8859-1"),
            b"caf\xE9 &#10003;"
        );
    }
}
