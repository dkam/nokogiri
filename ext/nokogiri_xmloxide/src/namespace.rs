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
};
use xmloxide::tree::{Attribute, NodeKind};
use xmloxide::{Document, NodeId};

use crate::classes;
use crate::node::{NodeKey, NodeObj, Receiver, find_attr};
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
