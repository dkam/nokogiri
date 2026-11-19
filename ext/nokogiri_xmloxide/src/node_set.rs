//! `Nokogiri::XML::NodeSet`, mirroring `ext/nokogiri/xml_node_set.c`.
//!
//! A list of node (or namespace) objects, held as Ruby values and marked.
//! Like libxml2's `xmlXPathNodeSetAdd`, it never holds the same object twice.

use std::cell::RefCell;
use std::collections::HashSet;

use magnus::{
    DataTypeFunctions, Error, Module, Object, RArray, Ruby, TypedData, Value, gc, method,
    prelude::*, rb_sys::AsRawValue, typed_data::Obj, value::Opaque,
};

use crate::classes;
use crate::node::NodeObj;

#[derive(TypedData, Default)]
#[magnus(class = "Nokogiri::XML::NodeSet", free_immediately, mark)]
pub struct NodeSetObj {
    items: RefCell<Vec<Opaque<Value>>>,
}

impl DataTypeFunctions for NodeSetObj {
    fn mark(&self, marker: &gc::Marker) {
        if let Ok(items) = self.items.try_borrow() {
            for item in items.iter() {
                marker.mark(*item);
            }
        }
    }
}

fn same(a: Value, b: Value) -> bool {
    a.as_raw() == b.as_raw()
}

/// A Ruby Array of `values`. magnus won't build one from a `Vec<Value>`
/// directly because a Rust Vec isn't visible to the GC; the values passed
/// here are node and namespace objects kept alive by their document.
pub fn array_of(ruby: &Ruby, values: Vec<Value>) -> Result<RArray, Error> {
    let array = ruby.ary_new_capa(values.len());
    for value in values {
        array.push(value)?;
    }
    Ok(array)
}

fn values_of(array: RArray) -> Result<Vec<Value>, Error> {
    (0..array.len())
        .map(|i| array.entry::<Value>(i as isize))
        .collect()
}

impl NodeSetObj {
    fn values(&self, ruby: &Ruby) -> Vec<Value> {
        self.items
            .borrow()
            .iter()
            .map(|v| ruby.get_inner(*v))
            .collect()
    }

    fn contains(&self, ruby: &Ruby, value: Value) -> bool {
        self.items
            .borrow()
            .iter()
            .any(|v| same(ruby.get_inner(*v), value))
    }
}

/// The members of a NodeSet object.
pub fn members(set: Value) -> Result<Vec<Value>, Error> {
    let ruby = Ruby::get_with(set);
    Ok(Obj::<NodeSetObj>::try_convert(set)?.values(&ruby))
}

/// The objects' identities, for set operations in linear time.
fn identities(values: &[Value]) -> HashSet<usize> {
    values.iter().map(|v| v.as_raw() as usize).collect()
}

/// A new NodeSet holding `nodes` (deduplicated, keeping first occurrences),
/// with `@document` set and the document's decorators applied, as
/// `noko_xml_node_set_wrap` does.
pub fn new(ruby: &Ruby, document: Value, nodes: Vec<Value>) -> Result<Value, Error> {
    let mut seen = HashSet::with_capacity(nodes.len());
    let items: Vec<Opaque<Value>> = nodes
        .into_iter()
        .filter(|node| seen.insert(node.as_raw() as usize))
        .map(Into::into)
        .collect();
    let set = ruby.obj_wrap(NodeSetObj {
        items: RefCell::new(items),
    });
    set.ivar_set("@document", document)?;
    if !document.is_nil() {
        let _: Value = document.funcall("decorate", (set,))?;
    }
    Ok(set.as_value())
}

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    let class = classes::get(ruby, &classes::NODE_SET);
    class.define_alloc_func::<NodeSetObj>();
    class.define_method("length", method!(length, 0))?;
    class.define_method("push", method!(push, 1))?;
    class.define_method("[]", method!(slice, -1))?;
    class.define_method("slice", method!(slice, -1))?;
    class.define_method("to_a", method!(to_a, 0))?;
    class.define_method("include?", method!(include, 1))?;
    class.define_method("delete", method!(delete, 1))?;
    class.define_method("|", method!(union, 1))?;
    class.define_method("&", method!(intersection, 1))?;
    class.define_method("-", method!(difference, 1))?;
    class.define_private_method("initialize_copy", method!(initialize_copy, 1))?;
    Ok(())
}

fn length(rb_self: &NodeSetObj) -> usize {
    rb_self.items.borrow().len()
}

/// Nodes and namespaces may go in a NodeSet; anything else is an error.
fn check_member(ruby: &Ruby, value: Value) -> Result<(), Error> {
    let is_node = Obj::<NodeObj>::try_convert(value).is_ok();
    let is_namespace = value.is_kind_of(classes::get(ruby, &classes::NAMESPACE));
    if is_node || is_namespace {
        Ok(())
    } else {
        Err(Error::new(
            ruby.exception_arg_error(),
            "node must be a Nokogiri::XML::Node or Nokogiri::XML::Namespace",
        ))
    }
}

fn push(ruby: &Ruby, rb_self: Obj<NodeSetObj>, node: Value) -> Result<Obj<NodeSetObj>, Error> {
    check_member(ruby, node)?;
    if !rb_self.contains(ruby, node) {
        rb_self.items.borrow_mut().push(node.into());
    }
    Ok(rb_self)
}

fn to_a(ruby: &Ruby, rb_self: &NodeSetObj) -> Result<RArray, Error> {
    array_of(ruby, rb_self.values(ruby))
}

/// `[]` and `slice`: Array's indexing rules exactly (index, start+length,
/// range), returning a NodeSet wherever Array would return an Array.
fn slice(ruby: &Ruby, rb_self: Obj<NodeSetObj>, args: &[Value]) -> Result<Value, Error> {
    let array = to_a(ruby, &rb_self)?;
    let result: Value = array.funcall("[]", args)?;
    match RArray::from_value(result) {
        Some(subset) => new(ruby, document_of(rb_self)?, values_of(subset)?),
        None => Ok(result),
    }
}

fn include(ruby: &Ruby, rb_self: &NodeSetObj, node: Value) -> bool {
    rb_self.contains(ruby, node)
}

fn delete(ruby: &Ruby, rb_self: &NodeSetObj, node: Value) -> Result<Option<Value>, Error> {
    check_member(ruby, node)?;
    let mut items = rb_self.items.borrow_mut();
    let before = items.len();
    items.retain(|v| !same(ruby.get_inner(*v), node));
    Ok((items.len() < before).then_some(node))
}

fn document_of(set: Obj<NodeSetObj>) -> Result<Value, Error> {
    set.ivar_get("@document")
}

fn other_set(ruby: &Ruby, other: Value) -> Result<Obj<NodeSetObj>, Error> {
    Obj::<NodeSetObj>::try_convert(other).map_err(|_| {
        Error::new(
            ruby.exception_arg_error(),
            "node_set must be a Nokogiri::XML::NodeSet",
        )
    })
}

fn union(ruby: &Ruby, rb_self: Obj<NodeSetObj>, other: Value) -> Result<Value, Error> {
    let other = other_set(ruby, other)?;
    let mut nodes = rb_self.values(ruby);
    nodes.extend(other.values(ruby));
    new(ruby, document_of(rb_self)?, nodes)
}

fn intersection(ruby: &Ruby, rb_self: Obj<NodeSetObj>, other: Value) -> Result<Value, Error> {
    let other = identities(&other_set(ruby, other)?.values(ruby));
    let nodes = rb_self
        .values(ruby)
        .into_iter()
        .filter(|v| other.contains(&(v.as_raw() as usize)))
        .collect();
    new(ruby, document_of(rb_self)?, nodes)
}

fn difference(ruby: &Ruby, rb_self: Obj<NodeSetObj>, other: Value) -> Result<Value, Error> {
    let other = identities(&other_set(ruby, other)?.values(ruby));
    let nodes = rb_self
        .values(ruby)
        .into_iter()
        .filter(|v| !other.contains(&(v.as_raw() as usize)))
        .collect();
    new(ruby, document_of(rb_self)?, nodes)
}

fn initialize_copy(ruby: &Ruby, rb_self: &NodeSetObj, other: Value) -> Result<(), Error> {
    let other = other_set(ruby, other)?;
    let copied: Vec<Opaque<Value>> = other.items.borrow().clone();
    *rb_self.items.borrow_mut() = copied;
    Ok(())
}
