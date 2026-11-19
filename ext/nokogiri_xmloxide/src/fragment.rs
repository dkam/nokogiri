//! `Nokogiri::XML::DocumentFragment.native_new` (`xml_document_fragment.c`)
//! and `Node#in_context` (`xml_node.c`), which parses markup as content of a
//! node, for fragments and `add_child("<markup/>")`.
//!
//! libxml2's `xmlParseInNodeContext` parses with the context node's
//! namespaces in scope. xmloxide has no such entry point, so the markup is
//! parsed inside a synthetic root element that declares every namespace in
//! scope at the context node, behind the document's internal DTD subset so
//! that declared entities resolve. The root's children are then copied into
//! the context node's document, detached, as libxml2 leaves them.

use std::fmt::Write as _;

use magnus::{Error, Module, RArray, RClass, RString, Ruby, Value, method, prelude::*};
use xmloxide::Document;
use xmloxide::error::SourceLocation;
use xmloxide::parser::parse_str_with_options;
use xmloxide::tree::NodeKind;

use crate::classes;
use crate::document::parser_options;
use crate::namespace;
use crate::node::{NodeKey, NodeObj, Receiver, document_receiver, escape_attribute, wrap_as};
use crate::node_set;
use crate::syntax_error;

/// The synthetic root's name; it never escapes into a result.
const CONTEXT_ROOT: &str = "nokogiri-xmloxide-context";

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    classes::get(ruby, &classes::DOCUMENT_FRAGMENT)
        .define_singleton_method("native_new", method!(native_new, 1))?;
    classes::get(ruby, &classes::NODE)
        .define_private_method("in_context", method!(in_context, 2))?;
    Ok(())
}

/// `DocumentFragment.native_new(document)`: an empty fragment in
/// `document`, of the receiving class. Ruby's `new` then calls `initialize`.
fn native_new(ruby: &Ruby, class: RClass, document: Value) -> Result<Value, Error> {
    let doc = document_receiver(ruby, document)?;
    let id = doc.tree_mut(ruby)?.create_node(NodeKind::DocumentFragment);
    wrap_as(
        ruby,
        doc.doc_value,
        doc.state(),
        NodeKey::Tree(id),
        Some(class),
    )
}

/// The wrapper document: `markup` after the wrapper's `prefix`, closed.
fn wrap_markup(prefix: &str, markup: &str) -> String {
    format!("{prefix}{markup}</{CONTEXT_ROOT}>")
}

/// Everything the wrapper puts before the markup: the internal `subset`, if
/// any, and a root that declares `namespaces`.
fn wrapper_prefix(namespaces: &[(Option<String>, String)], subset: Option<&str>) -> String {
    let mut text = String::with_capacity(128);
    if let Some(subset) = subset {
        let _ = write!(text, "<!DOCTYPE {CONTEXT_ROOT} [{subset}]>");
    }
    text.push('<');
    text.push_str(CONTEXT_ROOT);
    for (prefix, href) in namespaces {
        match prefix {
            Some(prefix) => {
                let _ = write!(text, " xmlns:{prefix}=\"{}\"", escape_attribute(href));
            }
            None => {
                let _ = write!(text, " xmlns=\"{}\"", escape_attribute(href));
            }
        }
    }
    text.push('>');
    text
}

/// Moves a diagnostic's position from the wrapper document to the markup:
/// back by the lines the wrapper's prefix spans and, on the markup's first
/// line, by the prefix's last line.
fn unshift(location: &mut SourceLocation, prefix: &str) {
    let prefix_lines = u32::try_from(prefix.matches('\n').count()).unwrap_or(u32::MAX);
    let last_line = prefix.rsplit('\n').next().unwrap_or(prefix);
    let prefix_columns = u32::try_from(last_line.chars().count()).unwrap_or(u32::MAX);
    if location.line > prefix_lines {
        location.line -= prefix_lines;
        if location.line == 1 {
            location.column = location.column.saturating_sub(prefix_columns).max(1);
        }
    }
    location.byte_offset = location.byte_offset.saturating_sub(prefix.len());
}

/// The document's internal DTD subset, if it has one.
fn internal_subset(tree: &Document) -> Option<String> {
    tree.children(tree.root())
        .find_map(|id| match &tree.node(id).kind {
            NodeKind::DocumentType {
                internal_subset, ..
            } => internal_subset.clone(),
            _ => None,
        })
}

/// `Node#in_context(markup, options)` (private; `Node#parse`): the nodes
/// `markup` parses to as content of this node, detached in its document.
/// Parse errors go to the document's `errors`; `Node#parse` decides whether
/// to raise them.
fn in_context(
    ruby: &Ruby,
    rb_self: &NodeObj,
    markup: RString,
    options: i64,
) -> Result<Value, Error> {
    let rcv = Receiver::of(ruby, rb_self)?;
    let prefix = {
        let tree = rcv.tree();
        let namespaces: Vec<(Option<String>, String)> =
            namespace::in_scope(&tree, rcv.key.anchor())
                .into_iter()
                .map(|(key, href)| (key.prefix, href))
                .collect();
        wrapper_prefix(&namespaces, internal_subset(&tree).as_deref())
    };
    // The markup is borrowed from the Ruby string only until it is copied
    // into the wrapper, before any further Ruby call.
    let wrapper = wrap_markup(
        &prefix,
        &String::from_utf8_lossy(unsafe { markup.as_slice() }),
    );
    let mut parsed = parse_str_with_options(&wrapper, &parser_options(options));
    let diagnostics = match &mut parsed {
        Ok(tree) => &mut tree.diagnostics,
        Err(failure) => {
            unshift(&mut failure.location, &prefix);
            &mut failure.diagnostics
        }
    };
    for diagnostic in diagnostics.iter_mut() {
        unshift(&mut diagnostic.location, &prefix);
    }

    let errors = match &parsed {
        Ok(tree) => syntax_error::array(ruby, &tree.diagnostics)?,
        Err(failure) => syntax_error::array_for_failure(ruby, failure)?,
    };
    let document_errors: Value = rcv.document_obj().ivar_get("@errors")?;
    if let Some(document_errors) = RArray::from_value(document_errors) {
        for error in errors.into_iter() {
            document_errors.push(error)?;
        }
    }

    let ids = match &parsed {
        Ok(source) => match source.root_element() {
            Some(root) => {
                let children: Vec<_> = source.children(root).collect();
                let mut tree = rcv.tree_mut(ruby)?;
                children
                    .into_iter()
                    .map(|child| tree.import_node(source, child))
                    .collect()
            }
            None => Vec::new(),
        },
        // xmloxide gives up where libxml2 would recover (notes/PLAN.md).
        Err(_) => Vec::new(),
    };
    let nodes = rcv.wrap_all(ruby, ids.into_iter().map(NodeKey::Tree).collect())?;
    node_set::new(ruby, rcv.doc_value, nodes)
}

#[cfg(test)]
mod tests {
    use super::{unshift, wrap_markup, wrapper_prefix};
    use xmloxide::error::SourceLocation;

    #[test]
    fn positions_refer_to_the_markup() {
        let prefix = wrapper_prefix(&[], Some("\n<!ENTITY e 'x'>\n"));
        let mut location = SourceLocation {
            line: 3,
            column: 40,
            byte_offset: prefix.len() + 2,
        };
        // The prefix spans two newlines and ends with "]><root>" on line 3.
        unshift(&mut location, &prefix);
        assert_eq!((location.line, location.byte_offset), (1, 2));
        assert_eq!(
            location.column,
            40 - "]><nokogiri-xmloxide-context>".len() as u32
        );
    }

    #[test]
    fn wrapper_declares_namespaces_and_keeps_the_subset() {
        let namespaces = vec![
            (Some("p".to_owned()), "urn:p".to_owned()),
            (None, "urn:\"d\"".to_owned()),
        ];
        let prefix = wrapper_prefix(&namespaces, Some("<!ENTITY e 'x'>"));
        assert_eq!(
            wrap_markup(&prefix, "<p:a/>"),
            "<!DOCTYPE nokogiri-xmloxide-context [<!ENTITY e 'x'>]>\
             <nokogiri-xmloxide-context xmlns:p=\"urn:p\" xmlns=\"urn:&quot;d&quot;\">\
             <p:a/></nokogiri-xmloxide-context>"
        );
    }
}
