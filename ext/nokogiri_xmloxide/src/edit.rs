//! Creating and changing nodes: the node constructors, `dup`, `unlink`, and
//! the attribute and content setters, mirroring the corresponding functions
//! in `ext/nokogiri/xml_node.c`, `xml_text.c`, `xml_attr.c` and friends.
//!
//! Nodes are never freed: an unlinked node stays in its document's arena,
//! so the Ruby objects that refer to it stay valid, as pinned nodes do in C.

use magnus::{
    Error, Module, Object, RClass, RString, Ruby, Value, method, prelude::*, rb_sys::AsRawValue,
    typed_data::Obj,
};
use xmloxide::tree::{Attribute, NodeKind};
use xmloxide::{Document, NodeId};

use crate::classes;
use crate::namespace;
use crate::node::{
    NodeKey, NodeObj, Receiver, adopt, document_receiver, edit_attribute, find_attr, new_holder,
    remove_attribute, wrap_as,
};
use crate::warnings;

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    let node = classes::get(ruby, &classes::NODE);
    node.define_singleton_method("new", method!(node_new, -1))?;
    node.define_method("unlink", method!(unlink, 0))?;
    node.define_method(
        "initialize_copy_with_args",
        method!(initialize_copy_with_args, 3),
    )?;
    node.define_method("native_content=", method!(set_native_content, 1))?;
    node.define_method("encode_special_chars", method!(encode_special_chars, 1))?;
    node.define_method("node_name=", method!(set_node_name, 1))?;
    node.define_method("lang=", method!(set_lang, 1))?;
    node.define_private_method("set", method!(set, 2))?;

    classes::get(ruby, &classes::TEXT).define_singleton_method("new", method!(text_new, -1))?;
    classes::get(ruby, &classes::CDATA).define_singleton_method("new", method!(cdata_new, -1))?;
    classes::get(ruby, &classes::COMMENT)
        .define_singleton_method("new", method!(comment_new, -1))?;
    classes::get(ruby, &classes::PROCESSING_INSTRUCTION)
        .define_singleton_method("new", method!(pi_new, -1))?;
    classes::get(ruby, &classes::ENTITY_REFERENCE)
        .define_singleton_method("new", method!(entity_reference_new, -1))?;
    let attr = classes::get(ruby, &classes::ATTR);
    attr.define_singleton_method("new", method!(attr_new, -1))?;
    attr.define_method("value=", method!(set_attr_value, 1))?;
    Ok(())
}

// --- Constructors -------------------------------------------------------------

/// The first `N` arguments, after `rb_scan_args`'s "N*" check that there are
/// at least `N`.
fn leading<const N: usize>(ruby: &Ruby, args: &[Value]) -> Result<[Value; N], Error> {
    ruby.check_arity(args.len(), N..)?;
    Ok(std::array::from_fn(|i| args[i]))
}

/// `StringValueCStr`: a String (or something with `to_str`).
pub fn string(value: Value) -> Result<String, Error> {
    RString::try_convert(value)?.to_string()
}

fn class_name(value: Value) -> String {
    value.class().inspect()
}

/// `Check_Type(value, T_STRING)`: `value` if it is a String, or the
/// TypeError the C extension raises.
pub fn check_string(ruby: &Ruby, value: Value) -> Result<RString, Error> {
    RString::from_value(value).ok_or_else(|| {
        // nil, true and false by name, as rb_builtin_class_name gives them.
        let special = [
            ruby.class_nil_class(),
            ruby.class_true_class(),
            ruby.class_false_class(),
        ];
        let type_name = if special.iter().any(|&class| value.is_kind_of(class)) {
            value.inspect()
        } else {
            class_name(value)
        };
        Error::new(
            ruby.exception_type_error(),
            format!("wrong argument type {type_name} (expected String)"),
        )
    })
}

/// The node a constructor's document argument names. A non-Document node is
/// accepted, with a deprecation warning, for its document.
fn constructor_document(
    ruby: &Ruby,
    value: Value,
    not_a_node: impl FnOnce() -> Error,
    deprecation: &str,
) -> Result<Receiver, Error> {
    if !value.is_kind_of(classes::get(ruby, &classes::NODE)) {
        return Err(not_a_node());
    }
    if !value.is_kind_of(classes::get(ruby, &classes::XML_DOCUMENT)) {
        warnings::deprecated(ruby, deprecation)?;
    }
    Receiver::of_value(ruby, value)
}

/// Creates node `kind` in `doc`'s document, then, as the C constructors do,
/// makes its Ruby object (of `class`, if given), calls `initialize` with the
/// constructor's arguments, and yields it to the block, if there is one.
fn construct(
    ruby: &Ruby,
    doc: &Receiver,
    kind: NodeKind,
    class: Option<RClass>,
    args: &[Value],
) -> Result<Value, Error> {
    let key = NodeKey::Tree(doc.tree_mut(ruby)?.create_node(kind));
    finish(ruby, doc, key, class, args)
}

fn finish(
    ruby: &Ruby,
    doc: &Receiver,
    key: NodeKey,
    class: Option<RClass>,
    args: &[Value],
) -> Result<Value, Error> {
    let node = wrap_as(ruby, doc.doc_value, doc.state(), key, class)?;
    let _: Value = node.funcall("initialize", args)?;
    if ruby.block_given() {
        let _: Value = ruby.yield_value(node)?;
    }
    Ok(node)
}

/// `Node.new(name, document, *rest) { |node| ... }`
fn node_new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let [name, document] = leading(ruby, args)?;
    let doc = constructor_document(
        ruby,
        document,
        || {
            Error::new(
                ruby.exception_arg_error(),
                "document must be a Nokogiri::XML::Node",
            )
        },
        "Passing a Node as the second parameter to Node.new is deprecated. Please pass a \
         Document instead, or prefer an alternative constructor like Node#add_child. This \
         will become an error in Nokogiri v1.17.0.",
    )?;
    let kind = NodeKind::Element {
        name: string(name)?,
        prefix: None,
        namespace: None,
        attributes: Vec::new(),
    };
    // Node.new makes an object of the node's own class (Element); a
    // subclass's .new makes one of the subclass.
    let node_class = classes::get(ruby, &classes::NODE);
    let class = (class.as_raw() != node_class.as_raw()).then_some(class);
    construct(ruby, &doc, kind, class, args)
}

/// `Text.new(string, document)` and `CDATA.new(document, string)`, which
/// differ only in argument order.
fn character_data_new(
    ruby: &Ruby,
    class: RClass,
    args: &[Value],
    (document, content): (Value, Value),
    (position, class_label): (&str, &str),
    kind: fn(String) -> NodeKind,
) -> Result<Value, Error> {
    let doc = constructor_document(
        ruby,
        document,
        || {
            Error::new(
                ruby.exception_type_error(),
                format!(
                    "expected {position} parameter to be a Nokogiri::XML::Document, received {}",
                    class_name(document)
                ),
            )
        },
        &format!(
            "Passing a Node as the {position} parameter to {class_label}.new is deprecated. \
             Please pass a Document instead. This will become an error in Nokogiri v1.17.0."
        ),
    )?;
    construct(ruby, &doc, kind(string(content)?), Some(class), args)
}

/// `Text.new(string, document, *rest)`
fn text_new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let [content, document] = leading(ruby, args)?;
    character_data_new(
        ruby,
        class,
        args,
        (document, content),
        ("second", "Text"),
        |content| NodeKind::Text { content },
    )
}

/// `CDATA.new(document, content, *rest)`
fn cdata_new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let [document, content] = leading(ruby, args)?;
    character_data_new(
        ruby,
        class,
        args,
        (document, content),
        ("first", "CDATA"),
        |content| NodeKind::CData { content },
    )
}

/// `Comment.new(document_or_node, content, *rest)`
fn comment_new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let [document, content] = leading(ruby, args)?;
    let content = check_string(ruby, content)?;
    if !document.is_kind_of(classes::get(ruby, &classes::NODE)) {
        return Err(Error::new(
            ruby.exception_arg_error(),
            "first argument must be a XML::Document or XML::Node",
        ));
    }
    let doc = Receiver::of_value(ruby, document)?;
    let kind = NodeKind::Comment {
        content: content.to_string()?,
    };
    construct(ruby, &doc, kind, Some(class), args)
}

/// `ProcessingInstruction.new(document, name, content, *rest)`
fn pi_new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let [document, name, content] = leading(ruby, args)?;
    let doc = document_receiver(ruby, document)?;
    let kind = NodeKind::ProcessingInstruction {
        target: string(name)?,
        data: Some(string(content)?),
    };
    construct(ruby, &doc, kind, Some(class), args)
}

/// `EntityReference.new(document, name, *rest)`
fn entity_reference_new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let [document, name] = leading(ruby, args)?;
    let doc = document_receiver(ruby, document)?;
    let kind = NodeKind::EntityRef {
        name: string(name)?,
        value: None,
    };
    construct(ruby, &doc, kind, Some(class), args)
}

/// `Attr.new(document, name, *rest)`: an attribute on no element.
fn attr_new(ruby: &Ruby, class: RClass, args: &[Value]) -> Result<Value, Error> {
    let [document, name] = leading(ruby, args)?;
    if !document.is_kind_of(classes::get(ruby, &classes::XML_DOCUMENT)) {
        return Err(Error::new(
            ruby.exception_arg_error(),
            "parameter must be a Nokogiri::XML::Document",
        ));
    }
    let doc = document_receiver(ruby, document)?;
    let name = string(name)?;
    let attribute = Attribute {
        name: name.clone(),
        value: String::new(),
        prefix: None,
        namespace: None,
        raw_value: None,
    };
    let holder = new_holder(&mut *doc.tree_mut(ruby)?, doc.state(), attribute);
    let key = NodeKey::Attr {
        owner: holder,
        prefix: None,
        name,
    };
    finish(ruby, &doc, key, Some(class), args)
}

// --- Copying ------------------------------------------------------------------

/// What `xmlDocCopyNode` keeps of a node at each `level`: 0 the bare node
/// (no namespace, attributes or children), 2 the node with its namespace
/// and attributes, anything else the whole subtree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CopyLevel {
    Bare,
    Shallow,
    Deep,
}

impl CopyLevel {
    fn of(level: i64) -> Self {
        match level {
            0 => Self::Bare,
            2 => Self::Shallow,
            _ => Self::Deep,
        }
    }

    fn kind(self, kind: &NodeKind) -> NodeKind {
        match (self, kind) {
            (Self::Bare, NodeKind::Element { name, .. }) => NodeKind::Element {
                name: name.clone(),
                prefix: None,
                namespace: None,
                attributes: Vec::new(),
            },
            _ => kind.clone(),
        }
    }
}

/// Copies node `id` of `source` into `target`, detached. `source` is `None`
/// when the copy goes into the same document.
pub fn copy_tree_node(
    target: &mut Document,
    source: Option<&Document>,
    id: NodeId,
    level: CopyLevel,
) -> NodeId {
    let copy = match (source, level) {
        (None, CopyLevel::Deep) => target.clone_node(id, true),
        (Some(source), CopyLevel::Deep) => target.import_node(source, id),
        (None, _) => {
            let kind = level.kind(&target.node(id).kind);
            target.create_node(kind)
        }
        (Some(source), _) => target.create_node(level.kind(&source.node(id).kind)),
    };
    if level != CopyLevel::Bare {
        namespace::declare_missing(target, copy);
    }
    copy
}

/// `Node#initialize_copy_with_args(other, level, new_parent_doc)`: makes this
/// newly allocated object (from `dup` or `clone`) a copy of `other` in
/// `new_parent_doc`.
fn initialize_copy_with_args(
    ruby: &Ruby,
    rb_self: Obj<NodeObj>,
    other: Value,
    level: i64,
    new_parent_doc: Value,
) -> Result<Obj<NodeObj>, Error> {
    if !other.is_kind_of(classes::get(ruby, &classes::NODE)) {
        return Err(Error::new(
            ruby.exception_type_error(),
            "argument must be a kind of Nokogiri::XML::Node",
        ));
    }
    let src = Receiver::of_value(ruby, other)?;
    let dst = document_receiver(ruby, new_parent_doc)?;
    let same_document = src.doc_value.as_raw() == dst.doc_value.as_raw();
    let level = CopyLevel::of(level);
    let key = match &src.key {
        NodeKey::Tree(id) => {
            let copy = if same_document {
                copy_tree_node(&mut *dst.tree_mut(ruby)?, None, *id, level)
            } else {
                let source = src.tree();
                copy_tree_node(&mut *dst.tree_mut(ruby)?, Some(&source), *id, level)
            };
            NodeKey::Tree(copy)
        }
        key @ NodeKey::Attr { prefix, name, .. } => {
            let attribute = find_attr(&src.tree(), key).cloned().ok_or_else(|| {
                Error::new(
                    ruby.exception_runtime_error(),
                    "the attribute is no longer on its element",
                )
            })?;
            let holder = new_holder(&mut *dst.tree_mut(ruby)?, dst.state(), attribute);
            NodeKey::Attr {
                owner: holder,
                prefix: prefix.clone(),
                name: name.clone(),
            }
        }
    };
    adopt(ruby, rb_self, dst.doc_value, dst.state(), key)?;
    let _: Value = dst.doc_value.funcall("decorate", (rb_self,))?;
    Ok(rb_self)
}

// --- Unlinking ----------------------------------------------------------------

/// Detaches the node from its parent (or an attribute from its element).
/// The node stays in its document, ready to be inserted elsewhere.
pub fn unlink_receiver(ruby: &Ruby, rcv: &Receiver) -> Result<(), Error> {
    match &rcv.key {
        NodeKey::Tree(id) => {
            let mut tree = rcv.tree_mut(ruby)?;
            if *id != tree.root() {
                tree.detach(*id);
            }
            Ok(())
        }
        key @ NodeKey::Attr { .. } => remove_attribute(ruby, rcv.state(), key),
    }
}

/// `Node#unlink`
fn unlink(ruby: &Ruby, rb_self: Obj<NodeObj>) -> Result<Obj<NodeObj>, Error> {
    unlink_receiver(ruby, &Receiver::of(ruby, &rb_self)?)?;
    Ok(rb_self)
}

// --- Attributes -----------------------------------------------------------------

/// Sets attribute `name` in namespace `namespace` on element `id`, the way
/// `xmlSetNsProp` does: an existing attribute with that name and namespace
/// takes the value; otherwise a new one is appended.
fn set_attribute(
    tree: &mut Document,
    id: NodeId,
    prefix: Option<&str>,
    name: &str,
    namespace: Option<&str>,
    value: &str,
) {
    let NodeKind::Element { attributes, .. } = tree.kind_mut(id) else {
        return;
    };
    let existing = attributes.iter_mut().find(|a| {
        a.name == name && a.namespace.as_deref() == namespace && namespace::declaration(a).is_none()
    });
    match existing {
        Some(attribute) => {
            attribute.value = value.to_owned();
            attribute.raw_value = None;
        }
        None => attributes.push(Attribute {
            name: name.to_owned(),
            value: value.to_owned(),
            prefix: prefix.map(str::to_owned),
            namespace: namespace.map(str::to_owned),
            raw_value: None,
        }),
    }
}

/// `Node#set(name, value)` (private; `Node#[]=`). A `prefix:name` whose
/// prefix is declared in scope names an attribute in that namespace;
/// otherwise the name is taken literally.
fn set(ruby: &Ruby, rb_self: &NodeObj, name: String, value: Value) -> Result<Value, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let Some(id) = rcv.tree_id() else {
        return Ok(ruby.qnil().as_value());
    };
    if !rcv.tree().is_element(id) {
        return Ok(ruby.qnil().as_value());
    }
    let text = string(value)?;
    let mut tree = rcv.tree_mut(ruby)?;
    let resolved = match namespace::split_qname(&name) {
        (Some(prefix), local) => {
            namespace::lookup(&tree, id, Some(prefix)).map(|href| (prefix, local, href))
        }
        (None, _) => None,
    };
    match resolved {
        Some((prefix, local, href)) => {
            set_attribute(&mut tree, id, Some(prefix), local, Some(&href), &text);
        }
        None => {
            // An xmlns attribute set this way is an attribute, not a
            // namespace declaration (see namespace::is_plain_xmlns).
            let namespace = (name == "xmlns").then_some(namespace::XMLNS_NAMESPACE);
            set_attribute(&mut tree, id, None, &name, namespace, &text);
        }
    }
    Ok(value)
}

/// `Attr#value=(content)`: nil empties the value.
fn set_attr_value(ruby: &Ruby, rb_self: &NodeObj, content: Value) -> Result<Value, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let value = if content.is_nil() {
        String::new()
    } else {
        string(content)?
    };
    set_attribute_value(ruby, &rcv, value)?;
    Ok(content)
}

fn set_attribute_value(ruby: &Ruby, rcv: &Receiver, value: String) -> Result<(), Error> {
    edit_attribute(ruby, rcv.state(), &rcv.key, |attribute| {
        attribute.value = value;
        attribute.raw_value = None;
    })
}

/// `Node#lang=(lang)`: sets `xml:lang` on an element.
fn set_lang(ruby: &Ruby, rb_self: &NodeObj, lang: String) -> Result<String, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    if let Some(id) = rcv.tree_id() {
        let mut tree = rcv.tree_mut(ruby)?;
        if tree.is_element(id) {
            set_attribute(
                &mut tree,
                id,
                Some("xml"),
                "lang",
                Some(namespace::XML_NAMESPACE),
                &lang,
            );
        }
    }
    Ok(lang)
}

// --- Content and names ------------------------------------------------------------

/// `Node#encode_special_chars(string)`.
fn encode_special_chars(_rb_self: Value, string: String) -> String {
    escape_special_chars(&string)
}

/// libxml2's `xmlEncodeSpecialChars`.
fn escape_special_chars(string: &str) -> String {
    let mut out = String::with_capacity(string.len());
    for c in string.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

/// The nodes `xmlNodeSetContent` makes of element content: text, with
/// character references and the predefined entities decoded, and an entity
/// reference node for any other `&name;`. A `&` that starts no reference
/// stays as it is.
fn content_nodes(content: &str) -> Vec<NodeKind> {
    let mut nodes = Vec::new();
    let mut text = String::new();
    let mut rest = content;
    while let Some(amp) = rest.find('&') {
        text.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let reference = rest[1..]
            .find(';')
            .map(|end| &rest[1..=end])
            .filter(|name| !name.is_empty() && !name.contains(['&', '<', ' ']));
        let Some(name) = reference else {
            text.push('&');
            rest = &rest[1..];
            continue;
        };
        rest = &rest[name.len() + 2..];
        match decode_reference(name) {
            Some(c) => text.push(c),
            None => {
                if !text.is_empty() {
                    nodes.push(NodeKind::Text {
                        content: std::mem::take(&mut text),
                    });
                }
                nodes.push(NodeKind::EntityRef {
                    name: name.to_owned(),
                    value: None,
                });
            }
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        nodes.push(NodeKind::Text { content: text });
    }
    nodes
}

/// The character a predefined entity or character reference stands for.
fn decode_reference(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// `Node#native_content=(content)`: unlinks the children, then sets the
/// content the way `xmlNodeSetContent` does.
fn set_native_content(ruby: &Ruby, rb_self: &NodeObj, content: String) -> Result<String, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let Some(id) = rcv.tree_id() else {
        let decoded: String = content_nodes(&content)
            .into_iter()
            .map(|kind| match kind {
                NodeKind::Text { content } => content,
                NodeKind::EntityRef { name, .. } => format!("&{name};"),
                _ => String::new(),
            })
            .collect();
        set_attribute_value(ruby, &rcv, decoded)?;
        return Ok(content);
    };
    let mut tree = rcv.tree_mut(ruby)?;
    let children: Vec<NodeId> = tree.children(id).collect();
    for child in children {
        tree.detach(child);
    }
    match tree.kind_mut(id) {
        NodeKind::Text { content: c }
        | NodeKind::CData { content: c }
        | NodeKind::Comment { content: c } => c.clone_from(&content),
        NodeKind::ProcessingInstruction { data, .. } => *data = Some(content.clone()),
        NodeKind::Element { .. } => {
            for kind in content_nodes(&content) {
                let child = tree.create_node(kind);
                tree.append_child(id, child);
            }
        }
        _ => {}
    }
    Ok(content)
}

/// `Node#node_name=(name)`: renames an element, attribute, or processing
/// instruction, keeping its namespace, as `xmlNodeSetName` does.
fn set_node_name(ruby: &Ruby, rb_self: &NodeObj, new_name: String) -> Result<String, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    match &rcv.key {
        NodeKey::Tree(id) => {
            let mut tree = rcv.tree_mut(ruby)?;
            match tree.kind_mut(*id) {
                NodeKind::Element { name, .. } => name.clone_from(&new_name),
                NodeKind::ProcessingInstruction { target, .. } => target.clone_from(&new_name),
                _ => {}
            }
        }
        key @ NodeKey::Attr { .. } => edit_attribute(ruby, rcv.state(), key, |attribute| {
            attribute.name.clone_from(&new_name);
        })?,
    }
    Ok(new_name)
}

#[cfg(test)]
mod tests {
    use super::{content_nodes, escape_special_chars};
    use xmloxide::tree::NodeKind;

    fn describe(content: &str) -> Vec<String> {
        content_nodes(content)
            .into_iter()
            .map(|kind| match kind {
                NodeKind::Text { content } => format!("text:{content}"),
                NodeKind::EntityRef { name, .. } => format!("ref:{name}"),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn content_decodes_references() {
        assert_eq!(describe("a &amp; b"), ["text:a & b"]);
        assert_eq!(describe("&lt;&#65;&#x42;&gt;"), ["text:<AB>"]);
        assert_eq!(describe("x&foo;y"), ["text:x", "ref:foo", "text:y"]);
        assert_eq!(describe("fish & chips"), ["text:fish & chips"]);
        assert_eq!(describe(""), Vec::<String>::new());
    }

    #[test]
    fn special_chars_are_escaped() {
        assert_eq!(
            escape_special_chars("<a href=\"x\">&\r"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#13;"
        );
    }

    #[test]
    fn content_round_trips_through_encoding() {
        let original = "<b> & &amp; \"q\"";
        let encoded = escape_special_chars(original);
        assert_eq!(describe(&encoded), [format!("text:{original}")]);
    }
}
