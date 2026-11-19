//! Node wrappers: one Rust type, [`NodeObj`], behind every
//! `Nokogiri::XML::Node` object, including documents. The design, and why it
//! can't produce Nokogiri's use-after-free bugs, is in
//! `notes/M2-node-design.md`.
//!
//! Re-entrancy rule: never call into Ruby (wrapping, `decorate`, IO) while a
//! `RefCell` borrow of a document's tree or cache is live. Collect `NodeKey`s
//! first, drop the borrow, then wrap.

use std::cell::{OnceCell, Ref, RefCell};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU32, Ordering};

use magnus::{
    DataTypeFunctions, Error, Module, RArray, RClass, Ruby, TypedData, Value, gc, method,
    prelude::*, typed_data::Obj, value::Opaque,
};
use xmloxide::serial::{SerializeOptions, serialize_node};
use xmloxide::tree::{Attribute, NodeKind};
use xmloxide::{Document, NodeId};

use crate::classes;
use crate::node_set;

/// libxml2's `xmlElementType` values, which Nokogiri exposes as `node_type`.
mod node_type {
    pub const ELEMENT: i64 = 1;
    pub const ATTRIBUTE: i64 = 2;
    pub const TEXT: i64 = 3;
    pub const CDATA_SECTION: i64 = 4;
    pub const ENTITY_REF: i64 = 5;
    pub const PI: i64 = 7;
    pub const COMMENT: i64 = 8;
    pub const DOCUMENT: i64 = 9;
    pub const HTML_DOCUMENT: i64 = 13;
    pub const DTD: i64 = 14;
}

/// Nokogiri's `SaveOptions` bits that the serializer honours so far.
mod save_options {
    pub const FORMAT: i64 = 1;
    pub const NO_DECLARATION: i64 = 2;
}

/// Which node a Ruby object refers to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeKey {
    /// A node in the arena: element, text, comment, PI, DTD, entity ref,
    /// or the document node itself.
    Tree(NodeId),
    /// An attribute. xmloxide stores attributes inside their element, so an
    /// attribute is named by its owner and qualified name. Not by index, so
    /// removing a sibling attribute doesn't change which attribute this is.
    Attr {
        owner: NodeId,
        prefix: Option<String>,
        name: String,
    },
}

struct Handle {
    /// The Ruby `Document` (the object itself, for documents).
    document: Opaque<Value>,
    key: NodeKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocKind {
    Xml,
    #[allow(dead_code)] // HTML documents arrive with HTML4 / HTML5 parsing.
    Html,
}

/// State owned by a Ruby `Document`: the tree and the identity map.
pub struct DocState {
    pub tree: RefCell<Document>,
    /// Every node wrapper handed out, so a node maps to one Ruby object.
    /// Marked by the document, like the C extension's `node_cache`.
    cache: RefCell<HashMap<NodeKey, Opaque<Value>>>,
    /// Distinguishes documents in `pointer_id`.
    serial: u32,
    pub url: Option<String>,
    pub kind: DocKind,
}

static NEXT_DOCUMENT_SERIAL: AtomicU32 = AtomicU32::new(1);

impl DocState {
    pub fn new(tree: Document, url: Option<String>, kind: DocKind) -> Self {
        Self {
            tree: RefCell::new(tree),
            cache: RefCell::new(HashMap::new()),
            serial: NEXT_DOCUMENT_SERIAL.fetch_add(1, Ordering::Relaxed),
            url,
            kind,
        }
    }
}

/// The data behind every node object. `doc` is set only on documents.
///
/// `Default` is the uninitialised state Ruby's allocator produces (for
/// `dup`, before `initialize_copy_with_args` runs). Every method on such an
/// object raises RuntimeError, as the C extension does since
/// GHSA-9cv2-cfxc-v4v2, rather than dereferencing nothing.
#[derive(TypedData, Default)]
#[magnus(class = "Nokogiri::XML::Node", free_immediately, mark)]
pub struct NodeObj {
    /// `None` until initialised.
    handle: RefCell<Option<Handle>>,
    doc: OnceCell<DocState>,
}

impl DataTypeFunctions for NodeObj {
    fn mark(&self, marker: &gc::Marker) {
        if let Ok(handle) = self.handle.try_borrow()
            && let Some(handle) = handle.as_ref()
        {
            marker.mark(handle.document);
        }
        if let Some(doc) = self.doc.get() {
            // The cache is only ever mutably borrowed for a HashMap insert,
            // which doesn't allocate Ruby objects, so GC can't land here then.
            let cache = doc
                .cache
                .try_borrow()
                .expect("node cache borrowed during GC mark");
            for value in cache.values() {
                marker.mark(*value);
            }
        }
    }
}

impl NodeObj {
    /// Data for a new document object. Call [`NodeObj::init_document`] once
    /// it is wrapped, so the document can point at itself.
    pub fn new_document(state: DocState) -> Self {
        Self {
            handle: RefCell::new(None),
            doc: OnceCell::from(state),
        }
    }

    pub fn init_document(this: Obj<Self>) {
        let root = this.doc.get().expect("a document").tree.borrow().root();
        *this.handle.borrow_mut() = Some(Handle {
            document: this.as_value().into(),
            key: NodeKey::Tree(root),
        });
    }
}

/// A method's receiver, resolved to its document and node.
pub struct Receiver {
    pub doc_value: Value,
    doc: Obj<NodeObj>,
    pub key: NodeKey,
}

impl Receiver {
    pub fn of(ruby: &Ruby, node: &NodeObj) -> Result<Self, Error> {
        let (doc_value, key) = {
            let handle = node.handle.borrow();
            let handle = handle.as_ref().ok_or_else(|| uninitialized(ruby))?;
            (ruby.get_inner(handle.document), handle.key.clone())
        };
        let doc = Obj::<NodeObj>::try_convert(doc_value)?;
        if doc.doc.get().is_none() {
            return Err(uninitialized(ruby));
        }
        Ok(Self {
            doc_value,
            doc,
            key,
        })
    }

    pub fn state(&self) -> &DocState {
        self.doc.doc.get().expect("checked in Receiver::of")
    }

    pub fn tree(&self) -> Ref<'_, Document> {
        self.state().tree.borrow()
    }

    pub fn tree_id(&self) -> Option<NodeId> {
        match self.key {
            NodeKey::Tree(id) => Some(id),
            NodeKey::Attr { .. } => None,
        }
    }

    pub fn wrap(&self, ruby: &Ruby, key: NodeKey) -> Result<Value, Error> {
        wrap(ruby, self.doc_value, self.state(), key)
    }

    pub fn wrap_opt(&self, ruby: &Ruby, id: Option<NodeId>) -> Result<Option<Value>, Error> {
        id.map(|id| self.wrap(ruby, NodeKey::Tree(id))).transpose()
    }

    pub fn wrap_all(&self, ruby: &Ruby, keys: Vec<NodeKey>) -> Result<Vec<Value>, Error> {
        keys.into_iter().map(|key| self.wrap(ruby, key)).collect()
    }
}

fn uninitialized(ruby: &Ruby) -> Error {
    Error::new(
        ruby.exception_runtime_error(),
        "uninitialized node: it has no document",
    )
}

/// Returns the Ruby object for `key`, creating and caching it on first use.
pub fn wrap(ruby: &Ruby, doc_value: Value, doc: &DocState, key: NodeKey) -> Result<Value, Error> {
    if let Some(value) = doc.cache.borrow().get(&key) {
        return Ok(ruby.get_inner(*value));
    }
    let class = {
        let tree = doc.tree.borrow();
        match &key {
            NodeKey::Tree(id) if *id == tree.root() => return Ok(doc_value),
            NodeKey::Tree(id) => class_for(ruby, &tree.node(*id).kind),
            NodeKey::Attr { .. } => classes::get(ruby, &classes::ATTR),
        }
    };
    let data = NodeObj {
        handle: RefCell::new(Some(Handle {
            document: doc_value.into(),
            key: key.clone(),
        })),
        doc: OnceCell::new(),
    };
    let value = ruby.obj_wrap_as(data, class).as_value();
    doc.cache.borrow_mut().insert(key, value.into());
    let _: Value = doc_value.funcall("decorate", (value,))?;
    Ok(value)
}

fn class_for(ruby: &Ruby, kind: &NodeKind) -> RClass {
    let class = match kind {
        NodeKind::Element { .. } => &classes::ELEMENT,
        NodeKind::Text { .. } => &classes::TEXT,
        NodeKind::CData { .. } => &classes::CDATA,
        NodeKind::Comment { .. } => &classes::COMMENT,
        NodeKind::ProcessingInstruction { .. } => &classes::PROCESSING_INSTRUCTION,
        NodeKind::EntityRef { .. } => &classes::ENTITY_REFERENCE,
        NodeKind::DocumentType { .. } => &classes::DTD,
        NodeKind::Document => &classes::NODE,
    };
    classes::get(ruby, class)
}

// --- Attributes ---------------------------------------------------------

/// `xmlns` and `xmlns:p` declarations. xmloxide stores them as attributes;
/// libxml2 (and so Nokogiri) treats them as namespace definitions instead.
fn is_namespace_declaration(attr: &Attribute) -> bool {
    (attr.name == "xmlns" && attr.prefix.is_none()) || attr.prefix.as_deref() == Some("xmlns")
}

/// The element's attributes as Nokogiri sees them: no namespace declarations.
fn attributes_of(tree: &Document, id: NodeId) -> impl Iterator<Item = &Attribute> {
    tree.attributes(id)
        .iter()
        .filter(|a| !is_namespace_declaration(a))
}

fn attr_key(owner: NodeId, attr: &Attribute) -> NodeKey {
    NodeKey::Attr {
        owner,
        prefix: attr.prefix.clone(),
        name: attr.name.clone(),
    }
}

/// The attribute an `Attr` key names, if it is still on its element.
fn find_attr<'t>(tree: &'t Document, key: &NodeKey) -> Option<&'t Attribute> {
    let NodeKey::Attr {
        owner,
        prefix,
        name,
    } = key
    else {
        return None;
    };
    attributes_of(tree, *owner).find(|a| a.prefix == *prefix && a.name == *name)
}

/// Resolves a namespace prefix (`None` for the default namespace) in scope at
/// `id`, by walking up through the `xmlns` declarations.
fn lookup_namespace(tree: &Document, id: NodeId, prefix: Option<&str>) -> Option<String> {
    if prefix == Some("xml") {
        return Some("http://www.w3.org/XML/1998/namespace".to_owned());
    }
    let mut current = Some(id);
    while let Some(node) = current {
        if tree.is_element(node) {
            let found = tree.attributes(node).iter().find(|a| match prefix {
                None => a.name == "xmlns" && a.prefix.is_none(),
                Some(p) => a.prefix.as_deref() == Some("xmlns") && a.name == p,
            });
            if let Some(decl) = found {
                return (!decl.value.is_empty()).then(|| decl.value.clone());
            }
        }
        current = tree.parent(node);
    }
    None
}

// --- Methods --------------------------------------------------------------

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    let node = classes::get(ruby, &classes::NODE);
    // magnus removes the default allocator from wrapped classes; Nokogiri
    // needs one (Object#dup allocates before initialize_copy_with_args).
    // Subclasses (Element, Document, ...) inherit Node's.
    node.define_alloc_func::<NodeObj>();
    node.define_method("node_name", method!(node_name, 0))?;
    node.define_method("node_type", method!(node_type_of, 0))?;
    node.define_method("document", method!(document, 0))?;
    node.define_method("pointer_id", method!(pointer_id, 0))?;
    node.define_method("parent", method!(parent, 0))?;
    node.define_method("children", method!(children, 0))?;
    node.define_method("child", method!(child, 0))?;
    node.define_method("element_children", method!(element_children, 0))?;
    node.define_method("first_element_child", method!(first_element_child, 0))?;
    node.define_method("last_element_child", method!(last_element_child, 0))?;
    node.define_method("next_sibling", method!(next_sibling, 0))?;
    node.define_method("previous_sibling", method!(previous_sibling, 0))?;
    node.define_method("next_element", method!(next_element, 0))?;
    node.define_method("previous_element", method!(previous_element, 0))?;
    node.define_method("content", method!(content, 0))?;
    node.define_method("blank?", method!(blank, 0))?;
    node.define_method("key?", method!(has_key, 1))?;
    node.define_method("namespaced_key?", method!(has_namespaced_key, 2))?;
    node.define_method("attribute", method!(attribute, 1))?;
    node.define_method("attribute_with_ns", method!(attribute_with_ns, 2))?;
    node.define_method("attribute_nodes", method!(attribute_nodes, 0))?;
    node.define_private_method("get", method!(get, 1))?;
    node.define_private_method("native_write_to", method!(native_write_to, 4))?;
    Ok(())
}

fn node_name(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let tree = rcv.tree();
    let Some(id) = rcv.tree_id() else {
        return Ok(find_attr(&tree, &rcv.key)
            .map(|a| a.name.clone())
            .or_else(|| match &rcv.key {
                NodeKey::Attr { name, .. } => Some(name.clone()),
                NodeKey::Tree(_) => None,
            }));
    };
    Ok(match &tree.node(id).kind {
        NodeKind::Element { name, .. } => Some(name.clone()),
        NodeKind::Text { .. } => Some("text".to_owned()),
        NodeKind::CData { .. } => Some("#cdata-section".to_owned()),
        NodeKind::Comment { .. } => Some("comment".to_owned()),
        NodeKind::ProcessingInstruction { target, .. } => Some(target.clone()),
        NodeKind::EntityRef { name, .. } => Some(name.clone()),
        NodeKind::DocumentType { name, .. } => Some(name.clone()),
        // Like libxml2: documents have no name (Document#name is Ruby).
        NodeKind::Document => None,
    })
}

fn node_type_of(ruby: &Ruby, rb_self: &NodeObj) -> Result<i64, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let Some(id) = rcv.tree_id() else {
        return Ok(node_type::ATTRIBUTE);
    };
    let tree = rcv.tree();
    Ok(match &tree.node(id).kind {
        NodeKind::Element { .. } => node_type::ELEMENT,
        NodeKind::Text { .. } => node_type::TEXT,
        NodeKind::CData { .. } => node_type::CDATA_SECTION,
        NodeKind::Comment { .. } => node_type::COMMENT,
        NodeKind::ProcessingInstruction { .. } => node_type::PI,
        NodeKind::EntityRef { .. } => node_type::ENTITY_REF,
        NodeKind::DocumentType { .. } => node_type::DTD,
        NodeKind::Document => match rcv.state().kind {
            DocKind::Xml => node_type::DOCUMENT,
            DocKind::Html => node_type::HTML_DOCUMENT,
        },
    })
}

fn document(ruby: &Ruby, rb_self: &NodeObj) -> Result<Value, Error> {
    Ok(Receiver::of(ruby, rb_self)?.doc_value)
}

/// Unique per node across documents; `Node#==` compares these.
fn pointer_id(ruby: &Ruby, rb_self: &NodeObj) -> Result<u64, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let mut hasher = DefaultHasher::new();
    rcv.state().serial.hash(&mut hasher);
    rcv.key.hash(&mut hasher);
    // Keep it a Fixnum.
    Ok(hasher.finish() >> 2)
}

fn parent(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let parent = match &rcv.key {
        NodeKey::Tree(id) => rcv.tree().parent(*id),
        NodeKey::Attr { owner, .. } => Some(*owner),
    };
    rcv.wrap_opt(ruby, parent)
}

fn child_ids(rcv: &Receiver, elements_only: bool) -> Vec<NodeKey> {
    let Some(id) = rcv.tree_id() else {
        return Vec::new();
    };
    let tree = rcv.tree();
    tree.children(id)
        .filter(|c| !elements_only || tree.is_element(*c))
        .map(NodeKey::Tree)
        .collect()
}

fn children(ruby: &Ruby, rb_self: &NodeObj) -> Result<Value, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let keys = child_ids(&rcv, false);
    let nodes = rcv.wrap_all(ruby, keys)?;
    node_set::new(ruby, rcv.doc_value, nodes)
}

fn element_children(ruby: &Ruby, rb_self: &NodeObj) -> Result<Value, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let keys = child_ids(&rcv, true);
    let nodes = rcv.wrap_all(ruby, keys)?;
    node_set::new(ruby, rcv.doc_value, nodes)
}

/// Shared shape of the single-node navigation methods.
fn navigate(
    ruby: &Ruby,
    rb_self: &NodeObj,
    step: impl Fn(&Document, NodeId) -> Option<NodeId>,
) -> Result<Option<Value>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let target = rcv.tree_id().and_then(|id| step(&rcv.tree(), id));
    rcv.wrap_opt(ruby, target)
}

fn element_from(
    tree: &Document,
    start: Option<NodeId>,
    step: fn(&Document, NodeId) -> Option<NodeId>,
) -> Option<NodeId> {
    let mut current = start;
    while let Some(id) = current {
        if tree.is_element(id) {
            return Some(id);
        }
        current = step(tree, id);
    }
    None
}

fn child(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    navigate(ruby, rb_self, |t, id| t.first_child(id))
}

fn first_element_child(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    navigate(ruby, rb_self, |t, id| {
        element_from(t, t.first_child(id), Document::next_sibling)
    })
}

fn last_element_child(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    navigate(ruby, rb_self, |t, id| {
        element_from(t, t.last_child(id), Document::prev_sibling)
    })
}

fn next_sibling(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    navigate(ruby, rb_self, |t, id| t.next_sibling(id))
}

fn previous_sibling(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    navigate(ruby, rb_self, |t, id| t.prev_sibling(id))
}

fn next_element(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    navigate(ruby, rb_self, |t, id| {
        element_from(t, t.next_sibling(id), Document::next_sibling)
    })
}

fn previous_element(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    navigate(ruby, rb_self, |t, id| {
        element_from(t, t.prev_sibling(id), Document::prev_sibling)
    })
}

fn content(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let tree = rcv.tree();
    let Some(id) = rcv.tree_id() else {
        return Ok(Some(
            find_attr(&tree, &rcv.key)
                .map(|a| a.value.clone())
                .unwrap_or_default(),
        ));
    };
    Ok(match &tree.node(id).kind {
        NodeKind::Text { content }
        | NodeKind::CData { content }
        | NodeKind::Comment { content } => Some(content.clone()),
        NodeKind::ProcessingInstruction { data, .. } => Some(data.clone().unwrap_or_default()),
        NodeKind::DocumentType { .. } => None,
        NodeKind::Element { .. } | NodeKind::EntityRef { .. } | NodeKind::Document => {
            Some(tree.text_content(id))
        }
    })
}

/// libxml2's `xmlIsBlankNode`: a text or CDATA node of only blanks.
fn blank(ruby: &Ruby, rb_self: &NodeObj) -> Result<bool, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let Some(id) = rcv.tree_id() else {
        return Ok(false);
    };
    Ok(match &rcv.tree().node(id).kind {
        NodeKind::Text { content } | NodeKind::CData { content } => content
            .bytes()
            .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r')),
        _ => false,
    })
}

/// `Node#[]` (private `get`), like the C version: `prefix:name` looks the
/// prefix up in scope; a bare name matches only attributes with no namespace.
fn get(ruby: &Ruby, rb_self: &NodeObj, name: Option<String>) -> Result<Option<String>, Error> {
    let Some(name) = name else { return Ok(None) };
    let rcv = Receiver::of(ruby, rb_self)?;
    let Some(id) = rcv.tree_id() else {
        return Ok(None);
    };
    let tree = rcv.tree();
    if !tree.is_element(id) {
        return Ok(None);
    }
    let found = match name.split_once(':') {
        Some((prefix, local)) => match lookup_namespace(&tree, id, Some(prefix)) {
            Some(href) => attributes_of(&tree, id)
                .find(|a| a.name == local && a.namespace.as_deref() == Some(href.as_str())),
            None => attributes_of(&tree, id).find(|a| a.name == name),
        },
        None => attributes_of(&tree, id).find(|a| a.name == name && a.namespace.is_none()),
    };
    Ok(found.map(|a| a.value.clone()))
}

/// The attribute named `name` in any namespace (libxml2's `xmlHasProp`).
fn attr_by_name(rcv: &Receiver, name: &str) -> Option<NodeKey> {
    let id = rcv.tree_id()?;
    let tree = rcv.tree();
    if !tree.is_element(id) {
        return None;
    }
    attributes_of(&tree, id)
        .find(|a| a.name == name)
        .map(|a| attr_key(id, a))
}

/// The attribute `name` in namespace `ns` (`None`: no namespace), like
/// libxml2's `xmlHasNsProp`.
fn attr_by_ns(rcv: &Receiver, name: &str, ns: Option<&str>) -> Option<NodeKey> {
    let id = rcv.tree_id()?;
    let tree = rcv.tree();
    if !tree.is_element(id) {
        return None;
    }
    attributes_of(&tree, id)
        .find(|a| a.name == name && a.namespace.as_deref() == ns)
        .map(|a| attr_key(id, a))
}

fn has_key(ruby: &Ruby, rb_self: &NodeObj, name: String) -> Result<bool, Error> {
    Ok(attr_by_name(&Receiver::of(ruby, rb_self)?, &name).is_some())
}

fn has_namespaced_key(
    ruby: &Ruby,
    rb_self: &NodeObj,
    name: String,
    ns: Option<String>,
) -> Result<bool, Error> {
    Ok(attr_by_ns(&Receiver::of(ruby, rb_self)?, &name, ns.as_deref()).is_some())
}

fn attribute(ruby: &Ruby, rb_self: &NodeObj, name: String) -> Result<Option<Value>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    attr_by_name(&rcv, &name)
        .map(|key| rcv.wrap(ruby, key))
        .transpose()
}

fn attribute_with_ns(
    ruby: &Ruby,
    rb_self: &NodeObj,
    name: String,
    ns: Option<String>,
) -> Result<Option<Value>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    attr_by_ns(&rcv, &name, ns.as_deref())
        .map(|key| rcv.wrap(ruby, key))
        .transpose()
}

fn attribute_nodes(ruby: &Ruby, rb_self: &NodeObj) -> Result<RArray, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let keys: Vec<NodeKey> = match rcv.tree_id() {
        Some(id) if rcv.tree().is_element(id) => {
            let tree = rcv.tree();
            attributes_of(&tree, id).map(|a| attr_key(id, a)).collect()
        }
        _ => Vec::new(),
    };
    node_set::array_of(ruby, rcv.wrap_all(ruby, keys)?)
}

/// Serialises the node and writes it to `io`. Not yet honoured: `encoding`
/// (output is UTF-8), `NO_EMPTY_TAGS`, and the HTML save modes.
fn native_write_to(
    ruby: &Ruby,
    rb_self: &NodeObj,
    io: Value,
    _encoding: Option<String>,
    indent_string: String,
    options: i64,
) -> Result<Value, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let markup = {
        let tree = rcv.tree();
        match &rcv.key {
            NodeKey::Tree(id) => {
                let opts = SerializeOptions::default()
                    .indent(options & save_options::FORMAT != 0)
                    .indent_str(&indent_string)
                    .xml_declaration(options & save_options::NO_DECLARATION == 0);
                serialize_node(&tree, *id, &opts)
            }
            key @ NodeKey::Attr { .. } => match find_attr(&tree, key) {
                Some(attr) => {
                    let qname = match &attr.prefix {
                        Some(p) => format!("{p}:{}", attr.name),
                        None => attr.name.clone(),
                    };
                    format!(" {qname}=\"{}\"", escape_attribute(&attr.value))
                }
                None => String::new(),
            },
        }
    };
    let _: Value = io.funcall("write", (ruby.str_new(&markup),))?;
    Ok(io)
}

fn escape_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out
}
