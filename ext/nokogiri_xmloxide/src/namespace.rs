//! `Nokogiri::XML::Namespace`, mirroring `ext/nokogiri/xml_namespace.c`, and
//! the node methods that return namespaces.
//!
//! xmloxide keeps namespace declarations as `xmlns` and `xmlns:p` attributes.
//! libxml2 keeps each one as an `xmlNs` struct on the declaring element, and
//! Nokogiri hands out one Ruby object per struct. Here a declaration is named
//! by its element and prefix ([`NsKey`]), and the document caches one Ruby
//! object per key, so `node.namespace.equal?(node.namespace)` holds.

use magnus::{
    DataTypeFunctions, Error, Module, Object, RArray, Ruby, TypedData, Value, method, prelude::*,
    typed_data::Obj,
};
use xmloxide::tree::{Attribute, NodeKind};
use xmloxide::{Document, NodeId};

use crate::classes;
use crate::node::{NodeKey, NodeObj, Receiver, find_attr, rekey};
use crate::node_set;

pub const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// Which declaration a Namespace object stands for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NsKey {
    /// The declaring element, or the document node for the built-in `xml`
    /// prefix.
    pub owner: NodeId,
    /// `None` for a default namespace declaration.
    pub prefix: Option<String>,
}

/// A namespace's prefix and URI, fixed when the object is made.
#[derive(TypedData)]
#[magnus(class = "Nokogiri::XML::Namespace", free_immediately)]
pub struct NamespaceObj {
    prefix: Option<String>,
    href: String,
}

impl DataTypeFunctions for NamespaceObj {}

// --- Declarations in the tree ---------------------------------------------

/// The prefix (`None` for the default namespace) and URI that `attr`
/// declares, if it is an `xmlns` or `xmlns:p` attribute.
pub fn declaration(attr: &Attribute) -> Option<(Option<&str>, &str)> {
    match attr.prefix.as_deref() {
        None if attr.name == "xmlns" => Some((None, &attr.value)),
        Some("xmlns") => Some((Some(&attr.name), &attr.value)),
        _ => None,
    }
}

/// The declarations made directly on `id`, in document order.
fn declarations(tree: &Document, id: NodeId) -> impl Iterator<Item = (Option<&str>, &str)> {
    tree.attributes(id).iter().filter_map(declaration)
}

/// The element that declares `prefix` in scope at `id` (the nearest one,
/// walking up), and the URI it declares.
fn resolve(tree: &Document, id: NodeId, prefix: Option<&str>) -> Option<(NodeId, String)> {
    let mut current = Some(id);
    while let Some(node) = current {
        if let Some((_, href)) = declarations(tree, node).find(|(p, _)| *p == prefix) {
            return Some((node, href.to_owned()));
        }
        current = tree.parent(node);
    }
    None
}

/// The URI `prefix` stands for at `id`. An empty URI (`xmlns=""`) undeclares
/// the default namespace, so it resolves to `None`.
pub fn lookup(tree: &Document, id: NodeId, prefix: Option<&str>) -> Option<String> {
    if prefix == Some("xml") {
        return Some(XML_NAMESPACE.to_owned());
    }
    resolve(tree, id, prefix).and_then(|(_, href)| (!href.is_empty()).then_some(href))
}

/// libxml2's `xmlGetNsList`: every declaration in scope at `id`, innermost
/// first. An inner declaration hides an outer one with the same prefix.
fn in_scope(tree: &Document, id: NodeId) -> Vec<(NsKey, String)> {
    let mut found: Vec<(NsKey, String)> = Vec::new();
    let mut current = Some(id);
    while let Some(node) = current {
        for (prefix, href) in declarations(tree, node) {
            if !found.iter().any(|(key, _)| key.prefix.as_deref() == prefix) {
                let key = NsKey {
                    owner: node,
                    prefix: prefix.map(str::to_owned),
                };
                found.push((key, href.to_owned()));
            }
        }
        current = tree.parent(node);
    }
    found
}

/// The declaration that puts a node named `prefix:…` into namespace `href`.
/// A node built without a matching declaration in scope gets a key of its
/// own, as libxml2 would give it an `xmlNs` of its own.
fn declaration_for(tree: &Document, anchor: NodeId, prefix: Option<&str>, href: &str) -> NsKey {
    if prefix == Some("xml") {
        return NsKey {
            owner: tree.root(),
            prefix: Some("xml".to_owned()),
        };
    }
    let owner = match resolve(tree, anchor, prefix) {
        Some((owner, declared)) if declared == href => owner,
        _ => anchor,
    };
    NsKey {
        owner,
        prefix: prefix.map(str::to_owned),
    }
}

/// An `xmlns` or `xmlns:prefix` attribute declaring `href`.
fn declaration_attribute(prefix: Option<&str>, href: &str) -> Attribute {
    let (name, prefix) = match prefix {
        Some(p) => (p.to_owned(), Some("xmlns".to_owned())),
        None => ("xmlns".to_owned(), None),
    };
    Attribute {
        name,
        value: href.to_owned(),
        prefix,
        namespace: None,
        raw_value: None,
    }
}

/// Declares `prefix` as `href` on element `id`, after its other
/// declarations and before its attributes, the order libxml2 writes them in.
fn declare(tree: &mut Document, id: NodeId, prefix: Option<&str>, href: &str) {
    if let NodeKind::Element { attributes, .. } = tree.kind_mut(id) {
        let position = attributes
            .iter()
            .rposition(|a| declaration(a).is_some())
            .map_or(0, |last| last + 1);
        attributes.insert(position, declaration_attribute(prefix, href));
    }
}

/// Declares, on `copy` (the top of a detached copy of a subtree), each
/// namespace the copy's elements and attributes use but the copy itself
/// doesn't declare, as libxml2's `xmlStaticCopyNode` does, so the copy keeps
/// its namespaces and serialises with its prefixes bound.
pub fn declare_missing(tree: &mut Document, copy: NodeId) {
    let mut missing: Vec<(Option<String>, String)> = Vec::new();
    for id in std::iter::once(copy).chain(tree.descendants(copy)) {
        let NodeKind::Element {
            prefix,
            namespace,
            attributes,
            ..
        } = &tree.node(id).kind
        else {
            continue;
        };
        let element_use = namespace.as_deref().map(|href| (prefix.as_deref(), href));
        let attribute_uses = attributes.iter().filter_map(|a| {
            let prefix = a.prefix.as_deref()?;
            (prefix != "xmlns").then_some((Some(prefix), a.namespace.as_deref()?))
        });
        for (prefix, href) in element_use.into_iter().chain(attribute_uses) {
            let bound = prefix == Some("xml")
                || resolve(tree, id, prefix).is_some_and(|(_, declared)| declared == href);
            if !bound && !missing.iter().any(|(p, _)| p.as_deref() == prefix) {
                missing.push((prefix.map(str::to_owned), href.to_owned()));
            }
        }
    }
    for (prefix, href) in missing {
        declare(tree, copy, prefix.as_deref(), &href);
    }
}

/// Puts a node in namespace `href` with `prefix`, or in no namespace.
/// Renaming an attribute's prefix changes its key, so its Ruby object moves.
fn set_node_namespace(
    ruby: &Ruby,
    rcv: &Receiver,
    namespace: Option<(Option<&str>, &str)>,
) -> Result<(), Error> {
    let new_prefix = namespace.and_then(|(p, _)| p).map(str::to_owned);
    let new_href = namespace.map(|(_, h)| h.to_owned());
    match &rcv.key {
        NodeKey::Tree(id) => {
            if let NodeKind::Element {
                prefix, namespace, ..
            } = rcv.tree_mut(ruby)?.kind_mut(*id)
            {
                *prefix = new_prefix;
                *namespace = new_href;
            }
        }
        key @ NodeKey::Attr {
            owner,
            prefix,
            name,
        } => {
            {
                let mut tree = rcv.tree_mut(ruby)?;
                if let NodeKind::Element { attributes, .. } = tree.kind_mut(*owner)
                    && let Some(attribute) = attributes
                        .iter_mut()
                        .find(|a| a.prefix == *prefix && a.name == *name)
                {
                    attribute.prefix.clone_from(&new_prefix);
                    attribute.namespace = new_href;
                }
            }
            let moved = NodeKey::Attr {
                owner: *owner,
                prefix: new_prefix,
                name: name.clone(),
            };
            rekey(ruby, rcv.state(), key, moved)?;
        }
    }
    Ok(())
}

// --- Ruby objects -----------------------------------------------------------

/// The Ruby Namespace for `key`, created and cached on first use, with
/// `@document` set as `noko_xml_namespace_wrap` does.
fn wrap(ruby: &Ruby, rcv: &Receiver, key: NsKey, href: String) -> Result<Value, Error> {
    let state = rcv.state();
    if let Some(value) = state.namespaces.borrow().get(&key) {
        return Ok(ruby.get_inner(*value));
    }
    let data = NamespaceObj {
        prefix: key.prefix.clone(),
        href,
    };
    let namespace = ruby.obj_wrap(data);
    namespace.ivar_set("@document", rcv.doc_value)?;
    let value = namespace.as_value();
    state.namespaces.borrow_mut().insert(key, value.into());
    Ok(value)
}

fn wrap_all(ruby: &Ruby, rcv: &Receiver, found: Vec<(NsKey, String)>) -> Result<RArray, Error> {
    let values = found
        .into_iter()
        .map(|(key, href)| wrap(ruby, rcv, key, href))
        .collect::<Result<Vec<_>, _>>()?;
    node_set::array_of(ruby, values)
}

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    let class = classes::get(ruby, &classes::NAMESPACE);
    class.define_method("prefix", method!(prefix, 0))?;
    class.define_method("href", method!(href, 0))?;

    let node = classes::get(ruby, &classes::NODE);
    node.define_method("namespace", method!(namespace, 0))?;
    node.define_method("namespace_definitions", method!(namespace_definitions, 0))?;
    node.define_method("namespace_scopes", method!(namespace_scopes, 0))?;
    node.define_method(
        "add_namespace_definition",
        method!(add_namespace_definition, 2),
    )?;
    node.define_private_method("set_namespace", method!(set_namespace, 1))?;
    Ok(())
}

fn prefix(rb_self: &NamespaceObj) -> Option<String> {
    rb_self.prefix.clone()
}

fn href(rb_self: &NamespaceObj) -> String {
    rb_self.href.clone()
}

/// `Node#namespace`: the namespace the element or attribute is in, or nil.
fn namespace(ruby: &Ruby, rb_self: &NodeObj) -> Result<Option<Value>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let found = {
        let tree = rcv.tree();
        let named = match &rcv.key {
            NodeKey::Tree(id) => match &tree.node(*id).kind {
                NodeKind::Element {
                    prefix, namespace, ..
                } => namespace
                    .as_deref()
                    .map(|href| (*id, prefix.as_deref(), href)),
                _ => None,
            },
            key @ NodeKey::Attr { owner, .. } => find_attr(&tree, key).and_then(|a| {
                a.namespace
                    .as_deref()
                    .map(|href| (*owner, a.prefix.as_deref(), href))
            }),
        };
        named.map(|(anchor, prefix, href)| {
            (
                declaration_for(&tree, anchor, prefix, href),
                href.to_owned(),
            )
        })
    };
    found
        .map(|(key, href)| wrap(ruby, &rcv, key, href))
        .transpose()
}

/// `Node#namespace_definitions`: the declarations made on this node.
fn namespace_definitions(ruby: &Ruby, rb_self: &NodeObj) -> Result<RArray, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let found = match rcv.tree_id() {
        Some(id) => declarations(&rcv.tree(), id)
            .map(|(prefix, href)| {
                let key = NsKey {
                    owner: id,
                    prefix: prefix.map(str::to_owned),
                };
                (key, href.to_owned())
            })
            .collect(),
        None => Vec::new(),
    };
    wrap_all(ruby, &rcv, found)
}

/// `Node#namespace_scopes`: every declaration in scope, innermost first.
/// An attribute sees its element's scope, as in libxml2.
fn namespace_scopes(ruby: &Ruby, rb_self: &NodeObj) -> Result<RArray, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let anchor = match &rcv.key {
        NodeKey::Tree(id) => *id,
        NodeKey::Attr { owner, .. } => *owner,
    };
    let found = in_scope(&rcv.tree(), anchor);
    wrap_all(ruby, &rcv, found)
}

/// The element a node's namespace lookups start from: itself, an
/// attribute's element, or another node's parent.
fn anchor(rcv: &Receiver) -> NodeId {
    match &rcv.key {
        NodeKey::Tree(id) => *id,
        NodeKey::Attr { owner, .. } => *owner,
    }
}

/// `Node#add_namespace_definition(prefix, href)`, as the C extension does
/// it: a prefix already declared in scope is reused whatever its URI;
/// otherwise the prefix is declared on the element (or a non-element's
/// parent). A default namespace, or one declared for a non-element, also
/// becomes the node's namespace.
fn add_namespace_definition(
    ruby: &Ruby,
    rb_self: &NodeObj,
    prefix: Option<String>,
    href: String,
) -> Result<Option<Value>, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let start = anchor(&rcv);
    let prefix = prefix.as_deref();
    let in_scope = if prefix == Some("xml") {
        Some((rcv.tree().root(), XML_NAMESPACE.to_owned()))
    } else {
        resolve(&rcv.tree(), start, prefix)
    };
    let in_scope_was_none = in_scope.is_none();
    let is_element = rcv.tree_id().is_some_and(|id| rcv.tree().is_element(id));
    let (key, href) = match in_scope {
        Some((owner, declared)) => (
            NsKey {
                owner,
                prefix: prefix.map(str::to_owned),
            },
            declared,
        ),
        None => {
            // A non-element declares on its parent element (an attribute's
            // parent is its element).
            let element = if is_element {
                Some(start)
            } else {
                let tree = rcv.tree();
                match &rcv.key {
                    NodeKey::Attr { .. } => Some(start),
                    NodeKey::Tree(id) => tree.parent(*id).filter(|p| tree.is_element(*p)),
                }
            };
            let Some(element) = element else {
                return Ok(None);
            };
            declare(&mut *rcv.tree_mut(ruby)?, element, prefix, &href);
            let key = NsKey {
                owner: element,
                prefix: prefix.map(str::to_owned),
            };
            (key, href)
        }
    };
    let declared_on_parent = in_scope_was_none && !is_element;
    if prefix.is_none() || declared_on_parent {
        set_node_namespace(ruby, &rcv, Some((prefix, &href)))?;
    }
    wrap(ruby, &rcv, key, href).map(Some)
}

/// `Node#set_namespace(namespace)` (private; `Node#namespace=`). An element
/// put into a namespace whose prefix isn't declared in scope gets a
/// declaration.
fn set_namespace(
    ruby: &Ruby,
    rb_self: Obj<NodeObj>,
    namespace: Value,
) -> Result<Obj<NodeObj>, Error> {
    let rcv = Receiver::of(ruby, &rb_self)?;
    if namespace.is_nil() {
        set_node_namespace(ruby, &rcv, None)?;
        return Ok(rb_self);
    }
    let namespace = Obj::<NamespaceObj>::try_convert(namespace)?;
    let prefix = namespace.prefix.as_deref();
    set_node_namespace(ruby, &rcv, Some((prefix, &namespace.href)))?;
    if let Some(id) = rcv.tree_id() {
        let undeclared = {
            let tree = rcv.tree();
            tree.is_element(id) && prefix != Some("xml") && resolve(&tree, id, prefix).is_none()
        };
        if undeclared {
            declare(&mut *rcv.tree_mut(ruby)?, id, prefix, &namespace.href);
        }
    }
    Ok(rb_self)
}
