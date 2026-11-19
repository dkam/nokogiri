//! Moving nodes: the private `add_child_node`, `add_previous_sibling_node`,
//! `add_next_sibling_node` and `replace_node`, which `add_child`,
//! `add_previous_sibling`, `add_next_sibling` and `replace` call, mirroring
//! `reparent_node_with` and `relink_namespace` in `ext/nokogiri/xml_node.c`.
//!
//! A node moved from another document is copied into this one, and the Ruby
//! objects of the original subtree move to the copy, so they keep their
//! identity. Text nodes merge as libxml2 2.14 merges them: a text node added
//! as the last child joins a text node already there, and a replacement joins
//! its text neighbours; sibling insertions never merge.

use magnus::{
    Error, Module, Object, Ruby, Value, method, prelude::*, rb_sys::AsRawValue, typed_data::Obj,
};
use xmloxide::tree::NodeKind;
use xmloxide::{Document, NodeId};

use crate::classes;
use crate::namespace;
use crate::node::{NodeKey, NodeObj, Receiver, attr_key, rekey, repoint, retarget_subtree};

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    let node = classes::get(ruby, &classes::NODE);
    node.define_private_method("add_child_node", method!(add_child, 1))?;
    node.define_private_method(
        "add_previous_sibling_node",
        method!(add_previous_sibling, 1),
    )?;
    node.define_private_method("add_next_sibling_node", method!(add_next_sibling, 1))?;
    node.define_private_method("replace_node", method!(replace, 1))?;
    Ok(())
}

fn add_child(ruby: &Ruby, rb_self: &NodeObj, node: Value) -> Result<Value, Error> {
    reparent(ruby, rb_self, node, Op::AddChild)
}

fn add_previous_sibling(ruby: &Ruby, rb_self: &NodeObj, node: Value) -> Result<Value, Error> {
    reparent(ruby, rb_self, node, Op::AddPreviousSibling)
}

fn add_next_sibling(ruby: &Ruby, rb_self: &NodeObj, node: Value) -> Result<Value, Error> {
    reparent(ruby, rb_self, node, Op::AddNextSibling)
}

fn replace(ruby: &Ruby, rb_self: &NodeObj, node: Value) -> Result<Value, Error> {
    reparent(ruby, rb_self, node, Op::Replace)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    AddChild,
    AddPreviousSibling,
    AddNextSibling,
    Replace,
}

/// libxml2's node types, as far as reparenting distinguishes them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Type {
    Document,
    Element,
    Text,
    CData,
    Comment,
    Pi,
    EntityRef,
    Dtd,
    Attribute,
    Fragment,
}

fn type_of(tree: &Document, key: &NodeKey) -> Type {
    let NodeKey::Tree(id) = key else {
        return Type::Attribute;
    };
    match tree.node(*id).kind {
        NodeKind::Document => Type::Document,
        NodeKind::Element { .. } => Type::Element,
        NodeKind::Text { .. } => Type::Text,
        NodeKind::CData { .. } => Type::CData,
        NodeKind::Comment { .. } => Type::Comment,
        NodeKind::ProcessingInstruction { .. } => Type::Pi,
        NodeKind::EntityRef { .. } => Type::EntityRef,
        NodeKind::DocumentType { .. } => Type::Dtd,
        NodeKind::DocumentFragment => Type::Fragment,
    }
}

/// The DOM's parent–child rules, as the C extension applies them (with text
/// allowed directly under a document, for compatibility).
fn allowed(parent: Type, child: Type) -> bool {
    use Type::{Attribute, CData, Comment, Document, Dtd, Element, EntityRef, Fragment, Pi, Text};
    match parent {
        Document => matches!(
            child,
            Element | Pi | Comment | Dtd | Text | CData | EntityRef
        ),
        Fragment | Element | EntityRef => {
            matches!(child, Element | Pi | Comment | Text | CData | EntityRef)
        }
        Attribute => matches!(child, Text | EntityRef),
        _ => false,
    }
}

fn text_of(tree: &Document, id: NodeId) -> Option<&str> {
    match &tree.node(id).kind {
        NodeKind::Text { content } => Some(content),
        _ => None,
    }
}

/// Appends `extra` to text node `id`.
fn append_text(tree: &mut Document, id: NodeId, extra: &str) {
    if let NodeKind::Text { content } = tree.kind_mut(id) {
        content.push_str(extra);
    }
}

fn is_ancestor_or_self(tree: &Document, ancestor: NodeId, node: NodeId) -> bool {
    let mut current = Some(node);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = tree.parent(id);
    }
    false
}

fn cannot_reparent(ruby: &Ruby, node: Value) -> Error {
    Error::new(
        ruby.exception_arg_error(),
        format!("cannot reparent {} there", node.class().inspect()),
    )
}

fn reparent(ruby: &Ruby, rb_self: &NodeObj, node: Value, op: Op) -> Result<Value, Error> {
    let not_a_node = || {
        Error::new(
            ruby.exception_arg_error(),
            "node must be a Nokogiri::XML::Node",
        )
    };
    if !node.is_kind_of(classes::get(ruby, &classes::NODE))
        || node.is_kind_of(classes::get(ruby, &classes::XML_DOCUMENT))
    {
        return Err(not_a_node());
    }
    let pivot = Receiver::of(ruby, rb_self)?;
    let node_obj = Obj::<NodeObj>::try_convert(node)?;
    let moving = Receiver::of(ruby, &node_obj)?;

    // The parent the node will have, and whether it may have it.
    let moving_type = type_of(&moving.tree(), &moving.key);
    let parent = match (op, &pivot.key) {
        (Op::AddChild, _) => Some(pivot.key.clone()),
        (_, NodeKey::Tree(id)) => pivot.tree().parent(*id).map(NodeKey::Tree),
        (_, NodeKey::Attr { .. }) => return Err(cannot_reparent(ruby, node)),
    };
    if let Some(parent) = &parent
        && !allowed(type_of(&pivot.tree(), parent), moving_type)
    {
        return Err(cannot_reparent(ruby, node));
    }
    if let NodeKey::Attr { .. } = pivot.key {
        return add_to_attribute(ruby, &pivot, &moving, node);
    }
    let NodeKey::Tree(pivot_id) = pivot.key else {
        unreachable!("attributes handled above")
    };
    let NodeKey::Tree(original) = moving.key else {
        unreachable!("attributes can't be reparented")
    };
    let same_document = moving.doc_value.as_raw() == pivot.doc_value.as_raw();

    // A node can't become its own descendant. libxml2 notices after the
    // fact; an arena would loop, so check first.
    if same_document
        && let Some(NodeKey::Tree(parent)) = parent
        && is_ancestor_or_self(&pivot.tree(), original, parent)
    {
        let name = pivot
            .tree()
            .node_name(original)
            .unwrap_or_default()
            .to_owned();
        return Err(Error::new(
            ruby.exception_runtime_error(),
            format!("cycle detected: node '{name}' is an ancestor of itself"),
        ));
    }
    let sibling_op = matches!(
        op,
        Op::AddPreviousSibling | Op::AddNextSibling | Op::Replace
    );
    if sibling_op && same_document && original == pivot_id {
        // Unlinking the node unlinks the pivot too; libxml2 then fails.
        return Err(Error::new(
            ruby.exception_runtime_error(),
            "Could not reparent node",
        ));
    }
    if sibling_op && pivot.tree().parent(pivot_id).is_none() {
        return Err(Error::new(
            ruby.exception_runtime_error(),
            "xmloxide backend: can't add a sibling to a node without a parent",
        ));
    }

    // A node from another document is copied in, and the original's objects
    // move to the copy. Either way the node leaves its old place.
    let id = if same_document {
        pivot.tree_mut(ruby)?.detach(original);
        original
    } else {
        let copy = {
            let source = moving.tree();
            let mut target = pivot.tree_mut(ruby)?;
            let copy = target.import_node(&source, original);
            namespace::declare_missing(&mut target, copy);
            copy
        };
        retarget_subtree(ruby, &moving, original, &pivot, copy)?;
        moving.tree_mut(ruby)?.detach(original);
        copy
    };

    let reparented = {
        let mut tree = pivot.tree_mut(ruby)?;
        insert(&mut tree, op, pivot_id, id)
    };
    if reparented != id {
        // Merged into a neighbouring text node.
        repoint(node_obj, pivot.doc_value, NodeKey::Tree(reparented));
    }
    let reparented_obj = pivot.wrap(ruby, NodeKey::Tree(reparented))?;
    let _: Value = reparented_obj.funcall("decorate!", ())?;

    let inherit = {
        let flag: Value = pivot.document_obj().ivar_get("@namespace_inheritance")?;
        flag.as_raw() == ruby.qtrue().as_raw()
    };
    let rekeys = {
        let mut tree = pivot.tree_mut(ruby)?;
        let mut rekeys = Vec::new();
        relink(&mut tree, reparented, inherit, &mut rekeys);
        rekeys
    };
    for (old, new) in rekeys {
        rekey(ruby, pivot.state(), &old, new)?;
    }
    Ok(reparented_obj)
}

/// Performs the insertion, merging text as libxml2 does, and returns the
/// node that ends up holding the inserted content: `id`, or the text node it
/// merged into.
fn insert(tree: &mut Document, op: Op, pivot: NodeId, id: NodeId) -> NodeId {
    let is_text = text_of(tree, id).is_some();
    match op {
        Op::AddChild => {
            // xmlAddChild coalesces a text node into a text last child.
            if is_text
                && let Some(last) = tree.last_child(pivot)
                && text_of(tree, last).is_some()
            {
                let content = text_of(tree, id).unwrap_or_default().to_owned();
                append_text(tree, last, &content);
                return last;
            }
            tree.append_child(pivot, id);
            id
        }
        Op::AddPreviousSibling => {
            tree.insert_before(pivot, id);
            id
        }
        Op::AddNextSibling => {
            tree.insert_after(pivot, id);
            id
        }
        Op::Replace => {
            // The C extension's xmlReplaceNodeWrapper merges a text
            // replacement with a text node on either side. The node after
            // is detached first, keeping its content and any object, as C
            // pins it before merging a copy.
            let trailing = (is_text)
                .then(|| tree.next_sibling(pivot))
                .flatten()
                .filter(|next| text_of(tree, *next).is_some());
            let trailing_content = trailing.map(|next| {
                let content = text_of(tree, next).unwrap_or_default().to_owned();
                tree.detach(next);
                content
            });
            tree.insert_before(pivot, id);
            tree.detach(pivot);
            let mut result = id;
            if is_text
                && let Some(previous) = tree.prev_sibling(id)
                && text_of(tree, previous).is_some()
            {
                let content = text_of(tree, id).unwrap_or_default().to_owned();
                append_text(tree, previous, &content);
                tree.detach(id);
                result = previous;
            }
            if let Some(content) = trailing_content {
                append_text(tree, result, &content);
            }
            result
        }
    }
}

/// `attr.add_child(text)`: the text becomes part of the attribute's value.
fn add_to_attribute(
    ruby: &Ruby,
    pivot: &Receiver,
    moving: &Receiver,
    node: Value,
) -> Result<Value, Error> {
    let NodeKey::Tree(id) = moving.key else {
        return Err(cannot_reparent(ruby, node));
    };
    let text = match &moving.tree().node(id).kind {
        NodeKind::Text { content } => content.clone(),
        NodeKind::EntityRef { name, .. } => format!("&{name};"),
        _ => return Err(cannot_reparent(ruby, node)),
    };
    moving.tree_mut(ruby)?.detach(id);
    let NodeKey::Attr {
        owner,
        prefix,
        name,
    } = &pivot.key
    else {
        unreachable!("the pivot is an attribute")
    };
    if let NodeKind::Element { attributes, .. } = pivot.tree_mut(ruby)?.kind_mut(*owner)
        && let Some(attribute) = attributes
            .iter_mut()
            .find(|a| a.prefix == *prefix && a.name == *name)
    {
        attribute.value.push_str(&text);
        attribute.raw_value = None;
    }
    Ok(node)
}

/// Splits `prefix:local`.
fn split_qname(name: &str) -> (Option<&str>, &str) {
    match name.split_once(':') {
        Some((prefix, local)) if !prefix.is_empty() && !local.is_empty() => (Some(prefix), local),
        _ => (None, name),
    }
}

/// Nokogiri's `relink_namespace`: after a move, an element (or attribute)
/// without a namespace, or with a default one, binds to whatever its prefix
/// (or the default namespace) means in its new place; with
/// `@namespace_inheritance`, an element still without one takes its
/// parent's; declarations the new parent already makes are dropped; and, if
/// the element ended up in a namespace, its children and attributes are
/// relinked too. Attributes whose key changes are reported in `rekeys`.
fn relink(tree: &mut Document, id: NodeId, inherit: bool, rekeys: &mut Vec<(NodeKey, NodeKey)>) {
    let NodeKind::Element {
        name,
        prefix,
        namespace,
        ..
    } = &tree.node(id).kind
    else {
        return;
    };
    if namespace.is_none() || prefix.is_none() {
        let (qprefix, local) = split_qname(name);
        if let Some(href) = namespace::lookup(tree, id, qprefix) {
            let qprefix = qprefix.map(str::to_owned);
            let local = local.to_owned();
            if let NodeKind::Element {
                name,
                prefix,
                namespace,
                ..
            } = tree.kind_mut(id)
            {
                if qprefix.is_some() {
                    *name = local;
                }
                *prefix = qprefix;
                *namespace = Some(href);
            }
        }
    }
    let Some(parent) = tree.parent(id) else {
        return;
    };
    let parent_is_element = tree.is_element(parent);
    if inherit
        && parent_is_element
        && tree.node_namespace(id).is_none()
        && let NodeKind::Element {
            prefix: parent_prefix,
            namespace: parent_namespace,
            ..
        } = tree.node(parent).kind.clone()
        && let NodeKind::Element {
            prefix, namespace, ..
        } = tree.kind_mut(id)
    {
        *prefix = parent_prefix;
        *namespace = parent_namespace;
    }
    // Drop declarations the new scope already makes.
    let redundant: Vec<(Option<String>, String)> = tree
        .attributes(id)
        .iter()
        .filter_map(namespace::declaration)
        .filter(|(prefix, href)| namespace::lookup(tree, parent, *prefix).as_deref() == Some(*href))
        .map(|(prefix, href)| (prefix.map(str::to_owned), href.to_owned()))
        .collect();
    if !redundant.is_empty()
        && let NodeKind::Element { attributes, .. } = tree.kind_mut(id)
    {
        attributes.retain(|a| match namespace::declaration(a) {
            Some((prefix, href)) => !redundant
                .iter()
                .any(|(p, h)| p.as_deref() == prefix && h == href),
            None => true,
        });
    }
    if tree.node_namespace(id).is_none() {
        return;
    }
    let children: Vec<NodeId> = tree.children(id).collect();
    for child in children {
        relink(tree, child, inherit, rekeys);
    }
    relink_attributes(tree, id, rekeys);
}

/// The attribute half of `relink_namespace`: a literal `prefix:name`
/// attribute binds to the prefix's namespace once it is declared in scope.
fn relink_attributes(tree: &mut Document, id: NodeId, rekeys: &mut Vec<(NodeKey, NodeKey)>) {
    let bindings: Vec<(usize, String, String, String)> = tree
        .attributes(id)
        .iter()
        .enumerate()
        .filter(|(_, a)| a.prefix.is_none() && namespace::declaration(a).is_none())
        .filter_map(|(index, a)| {
            let (prefix, local) = split_qname(&a.name);
            let prefix = prefix.filter(|p| *p != "xmlns")?;
            let href = namespace::lookup(tree, id, Some(prefix))?;
            Some((index, prefix.to_owned(), local.to_owned(), href))
        })
        .collect();
    if bindings.is_empty() {
        return;
    }
    let NodeKind::Element { attributes, .. } = tree.kind_mut(id) else {
        return;
    };
    for (index, prefix, local, href) in bindings {
        let attribute = &mut attributes[index];
        let old = attr_key(id, attribute);
        attribute.name = local;
        attribute.prefix = Some(prefix);
        attribute.namespace = Some(href);
        rekeys.push((old, attr_key(id, attribute)));
    }
}
