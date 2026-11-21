//! Node wrappers: one Rust type, [`NodeObj`], behind every
//! `Nokogiri::XML::Node` object, including documents. The design, and why it
//! can't produce Nokogiri's use-after-free bugs, is in
//! `notes/M2-node-design.md`.
//!
//! Re-entrancy rules for calls into Ruby (wrapping, `decorate`, IO, XPath
//! handlers):
//!
//! - Never hold a mutable borrow of a tree, or any borrow of a cache, across
//!   one. Collect `NodeKey`s first, drop the borrow, then wrap.
//! - A shared borrow of the tree may stay live across one: XPath evaluation
//!   holds one while a Ruby handler runs, so the handler can read and query
//!   the document. Code that mutates a tree must therefore use
//!   `try_borrow_mut` and raise a Ruby error if the document is being read.

use std::cell::{OnceCell, Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU32, Ordering};

use magnus::{
    DataTypeFunctions, Error, Module, RArray, RClass, Ruby, TypedData, Value, gc, method,
    prelude::*, rb_sys::AsRawValue, typed_data::Obj, value::Opaque,
};
use xmloxide::serial::{SerializeOptions, serialize_node};
use xmloxide::tree::{Attribute, NodeKind};
use xmloxide::{Document, NodeId};

use crate::classes;
use crate::namespace::{self, NsKey};
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
    pub const DOCUMENT_FRAG: i64 = 11;
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

impl NodeKey {
    /// The tree node this key is at: the node, or an attribute's element.
    pub fn anchor(&self) -> NodeId {
        match self {
            Self::Tree(id) => *id,
            Self::Attr { owner, .. } => *owner,
        }
    }
}

struct Handle {
    /// The Ruby `Document` (the object itself, for documents).
    document: Opaque<Value>,
    key: NodeKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocKind {
    Xml,
    Html,
}

/// State owned by a Ruby `Document`: the tree and the identity map.
pub struct DocState {
    pub tree: RefCell<Document>,
    /// Every node wrapper handed out, so a node maps to one Ruby object.
    /// Marked by the document, like the C extension's `node_cache`.
    cache: RefCell<HashMap<NodeKey, Opaque<Value>>>,
    /// The same for Namespace objects.
    pub(crate) namespaces: RefCell<HashMap<NsKey, Opaque<Value>>>,
    /// Detached elements that hold attributes removed from their element
    /// while a Ruby object still refers to them, as an unlinked `xmlAttr`
    /// lives on in libxml2. Never exposed as nodes.
    pub(crate) holders: RefCell<HashSet<NodeId>>,
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
            namespaces: RefCell::new(HashMap::new()),
            holders: RefCell::new(HashSet::new()),
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
            // The caches are only ever mutably borrowed for a HashMap insert,
            // which doesn't allocate Ruby objects, so GC can't land here then.
            let cache = doc
                .cache
                .try_borrow()
                .expect("node cache borrowed during GC mark");
            let namespaces = doc
                .namespaces
                .try_borrow()
                .expect("namespace cache borrowed during GC mark");
            for value in cache.values().chain(namespaces.values()) {
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

    /// Makes `this` (allocated by `dup`, so uninitialised) the document
    /// `state`. Returns false, changing nothing, if it already is one.
    pub fn init_document_copy(this: Obj<Self>, state: DocState) -> bool {
        if this.doc.set(state).is_err() {
            return false;
        }
        Self::init_document(this);
        true
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

    /// The receiver for a Ruby value that must be a node object.
    pub fn of_value(ruby: &Ruby, value: Value) -> Result<Self, Error> {
        let node = Obj::<NodeObj>::try_convert(value)?;
        Self::of(ruby, &node)
    }

    pub fn state(&self) -> &DocState {
        self.doc.doc.get().expect("checked in Receiver::of")
    }

    pub fn tree(&self) -> Ref<'_, Document> {
        self.state().tree.borrow()
    }

    /// The tree, for modification.
    pub fn tree_mut(&self, ruby: &Ruby) -> Result<RefMut<'_, Document>, Error> {
        tree_mut(ruby, self.state())
    }

    /// The document object.
    pub fn document_obj(&self) -> Obj<NodeObj> {
        self.doc
    }

    /// The node's parent: for an attribute, its element, unless the
    /// attribute has been detached.
    pub fn parent_id(&self) -> Option<NodeId> {
        match &self.key {
            NodeKey::Tree(id) => self.tree().parent(*id),
            NodeKey::Attr { owner, .. } => {
                (!self.state().holders.borrow().contains(owner)).then_some(*owner)
            }
        }
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

/// A document's tree, for modification. Raises rather than panicking while
/// the tree is being read, as when an XPath handler tries to modify the
/// document it is querying.
pub fn tree_mut<'a>(ruby: &Ruby, state: &'a DocState) -> Result<RefMut<'a, Document>, Error> {
    state.tree.try_borrow_mut().map_err(|_| {
        Error::new(
            ruby.exception_runtime_error(),
            "the document can't be modified while it is being read (by an XPath query, for instance)",
        )
    })
}

/// The document data of `value`, if it is an initialised document object.
pub fn document_receiver(ruby: &Ruby, value: Value) -> Result<Receiver, Error> {
    let not_a_document = || {
        Error::new(
            ruby.exception_type_error(),
            format!(
                "wrong argument type {} (expected Nokogiri::XML::Document)",
                value.class().inspect()
            ),
        )
    };
    let obj = Obj::<NodeObj>::try_convert(value).map_err(|_| not_a_document())?;
    if obj.doc.get().is_none() {
        return Err(not_a_document());
    }
    Receiver::of(ruby, &obj)
}

fn uninitialized(ruby: &Ruby) -> Error {
    Error::new(
        ruby.exception_runtime_error(),
        "uninitialized node: it has no document",
    )
}

/// Returns the Ruby object for `key`, creating and caching it on first use.
pub fn wrap(ruby: &Ruby, doc_value: Value, doc: &DocState, key: NodeKey) -> Result<Value, Error> {
    wrap_as(ruby, doc_value, doc, key, None)
}

/// Like [`wrap`], but a new object is of `class` when given (a constructor's
/// receiver, say) rather than the class for the node's kind.
pub fn wrap_as(
    ruby: &Ruby,
    doc_value: Value,
    doc: &DocState,
    key: NodeKey,
    class: Option<RClass>,
) -> Result<Value, Error> {
    if let Some(value) = doc.cache.borrow().get(&key) {
        return Ok(ruby.get_inner(*value));
    }
    let class = {
        let tree = doc.tree.borrow();
        match (&key, class) {
            (NodeKey::Tree(id), _) if *id == tree.root() => return Ok(doc_value),
            (_, Some(class)) => class,
            (NodeKey::Tree(id), None) => class_for(ruby, &tree.node(*id).kind),
            (NodeKey::Attr { .. }, None) => classes::get(ruby, &classes::ATTR),
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

/// Makes `obj` (an object allocated but not initialised, as by `dup`) the
/// Ruby object for `key`, caching it unless the node already has one.
pub fn adopt(
    ruby: &Ruby,
    obj: Obj<NodeObj>,
    doc_value: Value,
    doc: &DocState,
    key: NodeKey,
) -> Result<(), Error> {
    if obj.handle.borrow().is_some() || obj.doc.get().is_some() {
        return Err(Error::new(
            ruby.exception_runtime_error(),
            "node is already initialized",
        ));
    }
    *obj.handle.borrow_mut() = Some(Handle {
        document: doc_value.into(),
        key: key.clone(),
    });
    doc.cache
        .borrow_mut()
        .entry(key)
        .or_insert_with(|| obj.as_value().into());
    Ok(())
}

/// Moves the Ruby object for `old`, if there is one, to `new` within the
/// same document. Doesn't call into Ruby.
pub fn rekey(ruby: &Ruby, doc: &DocState, old: &NodeKey, new: NodeKey) -> Result<(), Error> {
    let Some(value) = doc.cache.borrow_mut().remove(old) else {
        return Ok(());
    };
    let obj = Obj::<NodeObj>::try_convert(ruby.get_inner(value))?;
    if let Some(handle) = obj.handle.borrow_mut().as_mut() {
        handle.key = new.clone();
    }
    doc.cache.borrow_mut().insert(new, value);
    Ok(())
}

/// Points `obj` at `key` in `doc_value` without changing any cache: the
/// object of a text node that libxml2 merged into a neighbour now refers to
/// the neighbour, which may have an object of its own.
pub fn repoint(obj: Obj<NodeObj>, doc_value: Value, key: NodeKey) {
    *obj.handle.borrow_mut() = Some(Handle {
        document: doc_value.into(),
        key,
    });
}

/// After a subtree was copied from one document into another, moves the
/// Ruby objects of the original's nodes and attributes to the copy's, so
/// they keep their identity in their new document. The copy has the
/// original's shape, so a walk of each pairs them up.
pub fn retarget_subtree(
    ruby: &Ruby,
    from: &Receiver,
    original: NodeId,
    to: &Receiver,
    copy: NodeId,
) -> Result<(), Error> {
    if from.state().cache.borrow().is_empty() {
        return Ok(());
    }
    let pairs: Vec<(NodeKey, NodeKey)> = {
        let source = from.tree();
        let target = to.tree();
        let originals = std::iter::once(original).chain(source.descendants(original));
        let copies = std::iter::once(copy).chain(target.descendants(copy));
        originals
            .zip(copies)
            .flat_map(|(old, new)| {
                let attributes = source
                    .attributes(old)
                    .iter()
                    .map(move |a| (attr_key(old, a), attr_key(new, a)));
                std::iter::once((NodeKey::Tree(old), NodeKey::Tree(new))).chain(attributes)
            })
            .collect()
    };
    for (old, new) in pairs {
        let Some(value) = from.state().cache.borrow_mut().remove(&old) else {
            continue;
        };
        let obj = Obj::<NodeObj>::try_convert(ruby.get_inner(value))?;
        repoint(obj, to.doc_value, new.clone());
        to.state().cache.borrow_mut().insert(new, value);
    }
    Ok(())
}

/// Removes an attribute from its element. If a Ruby object refers to it,
/// the attribute moves to a detached holder element, so the object keeps
/// its name and value, as an unlinked `xmlAttr` does in libxml2.
pub fn remove_attribute(ruby: &Ruby, doc: &DocState, key: &NodeKey) -> Result<(), Error> {
    if doc.holders.borrow().contains(&key.anchor()) {
        return Ok(());
    }
    let wrapped = doc.cache.borrow().contains_key(key);
    let holder = {
        let mut tree = tree_mut(ruby, doc)?;
        let Some((owner, index)) = attr_index(&tree, key) else {
            return Ok(());
        };
        let NodeKind::Element { attributes, .. } = tree.kind_mut(owner) else {
            return Ok(());
        };
        let attribute = attributes.remove(index);
        if !wrapped {
            return Ok(());
        }
        new_holder(&mut tree, doc, attribute)
    };
    let mut moved = key.clone();
    if let NodeKey::Attr { owner, .. } = &mut moved {
        *owner = holder;
    }
    rekey(ruby, doc, key, moved)
}

/// A detached holder element for `attribute`, an attribute on no element.
pub fn new_holder(tree: &mut Document, doc: &DocState, attribute: Attribute) -> NodeId {
    let holder = tree.create_node(NodeKind::Element {
        name: String::new(),
        prefix: None,
        namespace: None,
        attributes: vec![attribute],
    });
    doc.holders.borrow_mut().insert(holder);
    holder
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
        NodeKind::DocumentFragment => &classes::DOCUMENT_FRAGMENT,
    };
    classes::get(ruby, class)
}

// --- Attributes ---------------------------------------------------------

/// The element's attributes as Nokogiri sees them: no namespace declarations.
fn attributes_of(tree: &Document, id: NodeId) -> impl Iterator<Item = &Attribute> {
    tree.attributes(id)
        .iter()
        .filter(|a| namespace::declaration(a).is_none())
}

pub fn attr_key(owner: NodeId, attr: &Attribute) -> NodeKey {
    NodeKey::Attr {
        owner,
        prefix: attr.prefix.clone(),
        name: attr.name.clone(),
    }
}

/// The attribute an `Attr` key names, if it is still on its element.
pub fn find_attr<'t>(tree: &'t Document, key: &NodeKey) -> Option<&'t Attribute> {
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

/// Where the attribute an `Attr` key names is: its element, and its index
/// among the element's attributes.
pub fn attr_index(tree: &Document, key: &NodeKey) -> Option<(NodeId, usize)> {
    let NodeKey::Attr {
        owner,
        prefix,
        name,
    } = key
    else {
        return None;
    };
    let index = tree
        .attributes(*owner)
        .iter()
        .position(|a| a.prefix == *prefix && a.name == *name)?;
    Some((*owner, index))
}

/// The attribute an `Attr` key names, for modification.
fn attr_mut<'t>(tree: &'t mut Document, key: &NodeKey) -> Option<&'t mut Attribute> {
    let (owner, index) = attr_index(tree, key)?;
    match tree.kind_mut(owner) {
        NodeKind::Element { attributes, .. } => attributes.get_mut(index),
        _ => None,
    }
}

/// Changes the attribute an `Attr` key names. If the change renames it (its
/// prefix or name), its Ruby object moves to the new key.
pub fn edit_attribute(
    ruby: &Ruby,
    doc: &DocState,
    key: &NodeKey,
    change: impl FnOnce(&mut Attribute),
) -> Result<(), Error> {
    let renamed = {
        let mut tree = tree_mut(ruby, doc)?;
        let Some(attribute) = attr_mut(&mut tree, key) else {
            return Ok(());
        };
        change(attribute);
        attr_key(key.anchor(), attribute)
    };
    if renamed != *key {
        rekey(ruby, doc, key, renamed)?;
    }
    Ok(())
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
    node.define_method("path", method!(path, 0))?;
    node.define_method("lang", method!(lang, 0))?;
    node.define_method("data_ptr?", method!(has_data, 0))?;
    node.define_private_method("compare", method!(compare, 1))?;
    node.define_private_method("get", method!(get, 1))?;
    node.define_private_method("native_write_to", method!(native_write_to, 4))?;
    Ok(())
}

fn node_name(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let id = match &rcv.key {
        NodeKey::Tree(id) => *id,
        NodeKey::Attr { name, .. } => return Ok(Some(name.clone())),
    };
    let tree = rcv.tree();
    Ok(match &tree.node(id).kind {
        NodeKind::Element { name, .. } => Some(name.clone()),
        NodeKind::Text { .. } => Some("text".to_owned()),
        NodeKind::CData { .. } => Some("#cdata-section".to_owned()),
        NodeKind::Comment { .. } => Some("comment".to_owned()),
        NodeKind::ProcessingInstruction { target, .. } => Some(target.clone()),
        NodeKind::EntityRef { name, .. } => Some(name.clone()),
        NodeKind::DocumentType { name, .. } => Some(name.clone()),
        // Like libxml2: documents and fragments have no name (their #name
        // is Ruby).
        NodeKind::Document | NodeKind::DocumentFragment => None,
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
        NodeKind::DocumentFragment => node_type::DOCUMENT_FRAG,
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
    rcv.wrap_opt(ruby, rcv.parent_id())
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
        NodeKind::Element { .. }
        | NodeKind::EntityRef { .. }
        | NodeKind::Document
        | NodeKind::DocumentFragment => Some(tree.text_content(id)),
    })
}

/// libxml2's `xmlIsBlankNode`: a text or CDATA node of only blanks.
fn blank(ruby: &Ruby, rb_self: &NodeObj) -> Result<bool, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let Some(id) = rcv.tree_id() else {
        return Ok(false);
    };
    Ok(match &rcv.tree().node(id).kind {
        NodeKind::Text { content } | NodeKind::CData { content } => {
            content.bytes().all(is_xml_blank)
        }
        _ => false,
    })
}

/// libxml2's `IS_BLANK_CH`: XML whitespace.
pub fn is_xml_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
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
    let found = match namespace::split_qname(&name) {
        (Some(prefix), local) => match namespace::lookup(&tree, id, Some(prefix)) {
            Some(href) => attributes_of(&tree, id)
                .find(|a| a.name == local && a.namespace.as_deref() == Some(href.as_str())),
            None => attributes_of(&tree, id).find(|a| a.name == name),
        },
        (None, _) => attributes_of(&tree, id).find(|a| a.name == name && a.namespace.is_none()),
    };
    Ok(found.map(|a| a.value.clone()))
}

/// The first of the element's attributes that `matches`.
fn find_attribute(rcv: &Receiver, matches: impl Fn(&Attribute) -> bool) -> Option<NodeKey> {
    let id = rcv.tree_id()?;
    let tree = rcv.tree();
    attributes_of(&tree, id)
        .find(|a| matches(a))
        .map(|a| attr_key(id, a))
}

/// The attribute named `name` in any namespace (libxml2's `xmlHasProp`).
fn attr_by_name(rcv: &Receiver, name: &str) -> Option<NodeKey> {
    find_attribute(rcv, |a| a.name == name)
}

/// The attribute `name` in namespace `ns` (`None`: no namespace), like
/// libxml2's `xmlHasNsProp`.
fn attr_by_ns(rcv: &Receiver, name: &str, ns: Option<&str>) -> Option<NodeKey> {
    find_attribute(rcv, |a| a.name == name && a.namespace.as_deref() == ns)
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

pub fn escape_attribute(value: &str) -> String {
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

/// `Node#data_ptr?`: whether the object refers to a node yet.
fn has_data(rb_self: &NodeObj) -> bool {
    rb_self.handle.borrow().is_some()
}

/// `Node#compare(other)` (private; `Node#<=>`): 1 if this node follows
/// `other` in document order, -1 if it precedes it, 0 if they are the same
/// node, and -2 if they are in different trees, as libxml2's
/// `xmlXPathCmpNodes(other, self)` reports. An element's attributes follow
/// it, in order, before its children.
fn compare(ruby: &Ruby, rb_self: &NodeObj, other: Value) -> Result<i64, Error> {
    let this = Receiver::of(ruby, rb_self)?;
    let other = Receiver::of_value(ruby, other)?;
    if this.doc_value.as_raw() != other.doc_value.as_raw() {
        return Ok(-2);
    }
    let tree = this.tree();
    let attribute_position = |key: &NodeKey| attr_index(&tree, key).map(|(_, index)| index);
    let order = if this.key.anchor() == other.key.anchor() {
        attribute_position(&this.key).cmp(&attribute_position(&other.key))
    } else {
        match tree.compare_document_order(this.key.anchor(), other.key.anchor()) {
            Some(order) => order,
            None => return Ok(-2),
        }
    };
    Ok(order as i64)
}

/// `Node#lang`: the nearest `xml:lang` on the node or an ancestor.
fn lang(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<String>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let tree = rcv.tree();
    let lang = tree.ancestors(rcv.key.anchor()).find_map(|id| {
        tree.attributes(id)
            .iter()
            .find(|a| a.name == "lang" && a.namespace.as_deref() == Some(namespace::XML_NAMESPACE))
            .map(|a| a.value.clone())
    });
    Ok(lang)
}

/// Recognises the siblings a path step counts for its `[n]`.
type SameKind<'a> = Box<dyn Fn(&NodeKind) -> bool + 'a>;

/// A node's step in its path, and how to recognise its same-kind siblings,
/// following libxml2's `xmlGetNodePath`.
fn path_step(tree: &Document, id: NodeId) -> Option<(String, SameKind<'_>)> {
    let kind = &tree.node(id).kind;
    Some(match kind {
        NodeKind::Element {
            name,
            prefix,
            namespace,
            ..
        } => {
            // A default-namespace name can't be written in a path, so libxml2
            // writes `*` and counts every element sibling.
            let generic = namespace.is_some() && prefix.is_none();
            let step = match (namespace, prefix) {
                (Some(_), Some(p)) => format!("{p}:{name}"),
                (Some(_), None) => "*".to_owned(),
                (None, _) => name.clone(),
            };
            let same = move |other: &NodeKind| match other {
                NodeKind::Element {
                    name: n,
                    prefix: p,
                    namespace: ns,
                    ..
                } => {
                    generic
                        || (n == name
                            && ((ns == namespace && p == prefix)
                                || (ns.is_some() && namespace.is_some() && p == prefix)))
                }
                _ => false,
            };
            (step, Box::new(same))
        }
        NodeKind::Comment { .. } => (
            "comment()".to_owned(),
            Box::new(|other: &NodeKind| matches!(other, NodeKind::Comment { .. })),
        ),
        NodeKind::Text { .. } | NodeKind::CData { .. } => (
            "text()".to_owned(),
            Box::new(|other: &NodeKind| {
                matches!(other, NodeKind::Text { .. } | NodeKind::CData { .. })
            }),
        ),
        NodeKind::ProcessingInstruction { target, .. } => (
            format!("processing-instruction('{target}')"),
            Box::new(
                move |other: &NodeKind| matches!(other, NodeKind::ProcessingInstruction { target: t, .. } if t == target),
            ),
        ),
        _ => return None,
    })
}

/// The `[n]` a step needs: its position among same-kind siblings, or none
/// if it has no such siblings.
fn occurrence(tree: &Document, id: NodeId, same: &dyn Fn(&NodeKind) -> bool) -> usize {
    let mut before = 0;
    let mut sibling = tree.prev_sibling(id);
    while let Some(s) = sibling {
        before += usize::from(same(&tree.node(s).kind));
        sibling = tree.prev_sibling(s);
    }
    if before > 0 {
        return before + 1;
    }
    let mut sibling = tree.next_sibling(id);
    while let Some(s) = sibling {
        if same(&tree.node(s).kind) {
            return 1;
        }
        sibling = tree.next_sibling(s);
    }
    0
}

/// `Node#path`: an XPath to the node, as libxml2's `xmlGetNodePath` writes
/// it, or `"?"` where it can't, as the C extension returns.
fn path(ruby: &Ruby, rb_self: &NodeObj) -> Result<String, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let tree = rcv.tree();
    let mut path = String::new();
    let mut current = match &rcv.key {
        NodeKey::Tree(id) => Some(*id),
        NodeKey::Attr { prefix, name, .. } => {
            let attribute = find_attr(&tree, &rcv.key);
            path = match (attribute.and_then(|a| a.namespace.as_ref()), prefix) {
                (Some(_), Some(p)) => format!("/@{p}:{name}"),
                _ => format!("/@{name}"),
            };
            rcv.parent_id()
        }
    };
    while let Some(id) = current {
        if let NodeKind::Document = tree.node(id).kind {
            if !path.starts_with('/') {
                path.insert(0, '/');
            }
            break;
        }
        let Some((step, same)) = path_step(&tree, id) else {
            return Ok("?".to_owned());
        };
        let segment = match occurrence(&tree, id, &*same) {
            0 => format!("/{step}"),
            n => format!("/{step}[{n}]"),
        };
        path.insert_str(0, &segment);
        current = tree.parent(id);
    }
    Ok(path)
}
