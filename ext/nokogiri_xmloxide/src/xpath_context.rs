//! `Nokogiri::XML::XPathContext`, mirroring `ext/nokogiri/xml_xpath_context.c`.
//!
//! Queries run on xmloxide's evaluator with strict XPath 1.0 names, as in
//! libxml2: `//item` matches only un-namespaced elements. Nokogiri's two
//! built-in functions and a query's Ruby handler are supplied through
//! xmloxide's `FunctionResolver`.
//!
//! The document's tree stays immutably borrowed for the whole evaluation,
//! including while a handler runs. A handler may read the document, and even
//! query it again, but can't modify it mid-query.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use magnus::{
    DataTypeFunctions, Error, Float, Integer, IntoValue, Module, Object, RArray, RClass, RString,
    Ruby, TypedData, Value, gc, method, prelude::*, rb_sys::AsRawValue, typed_data::Obj,
    value::Opaque,
};
use xmloxide::Document;
use xmloxide::xpath::eval::{ExtensionCall, FunctionResolver, XPathContext};
use xmloxide::xpath::parser::parse;
use xmloxide::xpath::{XPathError, XPathNode, XPathValue};

use crate::classes;
use crate::node::{NodeKey, NodeObj, Receiver, attr_index, attr_key, is_xml_blank};
use crate::node_set::{self, NodeSetObj};
use crate::syntax_error::{self, XPathCode};
use crate::warnings;

const NOKOGIRI_PREFIX: &str = "nokogiri";
const NOKOGIRI_URI: &str = "http://www.nokogiri.org/default_ns/ruby/extensions_functions";
const NOKOGIRI_BUILTIN_PREFIX: &str = "nokogiri-builtin";
const NOKOGIRI_BUILTIN_URI: &str = "https://www.nokogiri.org/default_ns/ruby/builtins";

#[derive(TypedData)]
#[magnus(class = "Nokogiri::XML::XPathContext", free_immediately, mark)]
pub struct XPathContextObj {
    node: Cell<Opaque<Value>>,
    namespaces: RefCell<HashMap<String, String>>,
    variables: RefCell<HashMap<String, String>>,
}

impl DataTypeFunctions for XPathContextObj {
    fn mark(&self, marker: &gc::Marker) {
        marker.mark(self.node.get());
    }
}

pub fn init(ruby: &Ruby) -> Result<(), Error> {
    let class = classes::get(ruby, &classes::XPATH_CONTEXT);
    // Instances come only from `new`, as in C (`rb_undef_alloc_func`).
    class.undef_default_alloc_func();
    class.define_singleton_method("new", method!(new, 1))?;
    class.define_method("evaluate", method!(evaluate, -1))?;
    class.define_method("register_ns", method!(register_ns, 2))?;
    class.define_method("register_variable", method!(register_variable, 2))?;
    class.define_method("node=", method!(set_node, 1))?;
    Ok(())
}

/// The node, if `value` is an initialised `Nokogiri::XML::Node`; a TypeError
/// otherwise, as in the C extension.
fn check_node(ruby: &Ruby, value: Value) -> Result<(), Error> {
    if !value.is_kind_of(classes::get(ruby, &classes::NODE)) {
        return Err(Error::new(
            ruby.exception_type_error(),
            format!(
                "expected a Nokogiri::XML::Node, received {}",
                value.class().inspect()
            ),
        ));
    }
    let node = Obj::<NodeObj>::try_convert(value)?;
    Receiver::of(ruby, &node)?;
    Ok(())
}

fn new(ruby: &Ruby, class: RClass, node: Value) -> Result<Value, Error> {
    check_node(ruby, node)?;
    let namespaces = HashMap::from([
        (NOKOGIRI_PREFIX.to_owned(), NOKOGIRI_URI.to_owned()),
        (
            NOKOGIRI_BUILTIN_PREFIX.to_owned(),
            NOKOGIRI_BUILTIN_URI.to_owned(),
        ),
    ]);
    let data = XPathContextObj {
        node: Cell::new(node.into()),
        namespaces: RefCell::new(namespaces),
        variables: RefCell::new(HashMap::new()),
    };
    Ok(ruby.obj_wrap_as(data, class).as_value())
}

fn set_node(ruby: &Ruby, rb_self: &XPathContextObj, node: Value) -> Result<Value, Error> {
    check_node(ruby, node)?;
    rb_self.node.set(node.into());
    Ok(node)
}

/// Binds `key` to `value` in `map`, or unbinds it if `value` is nil.
fn bind(map: &RefCell<HashMap<String, String>>, key: String, value: Option<String>) {
    let mut map = map.borrow_mut();
    match value {
        Some(value) => map.insert(key, value),
        None => map.remove(&key),
    };
}

/// `register_ns(prefix, uri)`; a nil `uri` unregisters the prefix.
fn register_ns(
    rb_self: Obj<XPathContextObj>,
    prefix: String,
    uri: Option<String>,
) -> Obj<XPathContextObj> {
    bind(&rb_self.namespaces, prefix, uri);
    rb_self
}

/// `register_variable(name, value)`; a nil `value` unregisters the name.
/// Values are strings, as in the C extension.
fn register_variable(
    rb_self: Obj<XPathContextObj>,
    name: String,
    value: Option<String>,
) -> Obj<XPathContextObj> {
    bind(&rb_self.variables, name, value);
    rb_self
}

/// `evaluate(search_path, handler = nil)`: a NodeSet, String, Float, or
/// boolean.
fn evaluate(ruby: &Ruby, rb_self: &XPathContextObj, args: &[Value]) -> Result<Value, Error> {
    ruby.check_arity(args.len(), 1..=2)?;
    let expression = RString::try_convert(args[0])?.to_string()?;
    let handler = args.get(1).copied().filter(|h| !h.is_nil());
    let parsed = parse(&expression)
        .map_err(|err| syntax_error::xpath_error(ruby, &err.into(), &expression))?;

    let node = Obj::<NodeObj>::try_convert(ruby.get_inner(rb_self.node.get()))?;
    let rcv = Receiver::of(ruby, &node)?;
    let functions = Functions {
        ruby,
        rcv: &rcv,
        handler,
        expression: &expression,
        failure: RefCell::new(None),
        known: RefCell::new(HashMap::new()),
    };
    let namespaces = rb_self.namespaces.borrow().clone();
    let variables = rb_self.variables.borrow().clone();

    let outcome = {
        let tree = rcv.tree();
        let context_node = xpath_node(&tree, &rcv.key).ok_or_else(|| gone(ruby))?;
        let mut ctx = XPathContext::new_at(&tree, context_node);
        ctx.set_strict_names(true);
        for (prefix, uri) in &namespaces {
            ctx.set_namespace(prefix, uri);
        }
        for (name, value) in variables {
            ctx.set_variable(&name, XPathValue::String(value));
        }
        ctx.set_function_resolver(&functions);
        ctx.evaluate(&parsed).map(|value| Outcome::of(&tree, value))
    };

    if let Some(error) = functions.failure.take() {
        return Err(error);
    }
    match outcome {
        Ok(outcome) => outcome.into_ruby(ruby, &rcv),
        Err(err) => Err(syntax_error::xpath_error(ruby, &err, &expression)),
    }
}

fn gone(ruby: &Ruby) -> Error {
    Error::new(
        ruby.exception_runtime_error(),
        "the attribute is no longer on its element",
    )
}

/// An evaluation's result, with nodes named by [`NodeKey`] so it outlives the
/// tree borrow.
enum Outcome {
    Nodes(Vec<NodeKey>),
    String(String),
    Number(f64),
    Boolean(bool),
}

impl Outcome {
    fn of(tree: &Document, value: XPathValue) -> Self {
        match value {
            XPathValue::NodeSet(nodes) => {
                Self::Nodes(nodes.into_iter().map(|n| node_key(tree, n)).collect())
            }
            XPathValue::String(s) => Self::String(s),
            XPathValue::Number(n) => Self::Number(n),
            XPathValue::Boolean(b) => Self::Boolean(b),
        }
    }

    fn into_ruby(self, ruby: &Ruby, rcv: &Receiver) -> Result<Value, Error> {
        Ok(match self {
            Self::Nodes(keys) => {
                let nodes = rcv.wrap_all(ruby, keys)?;
                node_set::new(ruby, rcv.doc_value, nodes)?
            }
            Self::String(s) => ruby.str_new(&s).as_value(),
            Self::Number(n) => ruby.float_from_f64(n).as_value(),
            Self::Boolean(b) => b.into_value_with(ruby),
        })
    }
}

/// The xmloxide node a key names.
fn xpath_node(tree: &Document, key: &NodeKey) -> Option<XPathNode> {
    match key {
        NodeKey::Tree(id) => Some(XPathNode::Node(*id)),
        NodeKey::Attr { .. } => {
            let (owner, index) = attr_index(tree, key)?;
            Some(XPathNode::Attribute {
                owner,
                index: u32::try_from(index).ok()?,
            })
        }
    }
}

fn node_key(tree: &Document, node: XPathNode) -> NodeKey {
    match node {
        XPathNode::Node(id) => NodeKey::Tree(id),
        XPathNode::Attribute { owner, index } => {
            attr_key(owner, &tree.attributes(owner)[index as usize])
        }
    }
}

// --- Functions --------------------------------------------------------------

/// Nokogiri's built-in functions plus the query's handler, if any.
///
/// When the handler raises, the exception is kept in `failure` and every
/// later call fails at once, so evaluation unwinds and `evaluate` re-raises
/// it, as the C extension does with `rb_protect`.
struct Functions<'a> {
    ruby: &'a Ruby,
    rcv: &'a Receiver,
    handler: Option<Value>,
    expression: &'a str,
    failure: RefCell<Option<Error>>,
    /// Whether the handler has each function (by name, and whether it was
    /// called without a namespace), asked once per query, as libxml2 looks a
    /// function up once per call site.
    known: RefCell<HashMap<(String, bool), bool>>,
}

impl FunctionResolver for Functions<'_> {
    fn call(
        &self,
        name: &str,
        namespace: Option<&str>,
        call: &ExtensionCall<'_, '_>,
    ) -> Option<Result<XPathValue, XPathError>> {
        if self.failure.borrow().is_some() {
            return Some(Err(aborted()));
        }
        if namespace == Some(NOKOGIRI_BUILTIN_URI) {
            match name {
                "css-class" => return Some(css_class(call)),
                "local-name-is" => return Some(local_name_is(call)),
                _ => {}
            }
        }
        let handler = self.handler?;
        match self.handles(handler, name, namespace.is_none()) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => return Some(Err(self.fail(error))),
        }
        Some(call.args().and_then(|args| {
            self.invoke(handler, name, call.document(), args)
                .map_err(|error| self.fail(error))
        }))
    }
}

fn aborted() -> XPathError {
    XPathError::InternalError {
        message: "aborted by a Ruby exception".to_owned(),
    }
}

impl Functions<'_> {
    /// Whether the handler has function `name`. Like the C extension, any
    /// namespace reaches the handler; a name without one is deprecated.
    fn handles(&self, handler: Value, name: &str, unprefixed: bool) -> Result<bool, Error> {
        let key = (name.to_owned(), unprefixed);
        let cached = self.known.borrow().get(&key).copied();
        if let Some(known) = cached {
            return Ok(known);
        }
        let responds = handler.respond_to(name, false)?;
        if responds && unprefixed {
            warnings::deprecated(
                self.ruby,
                &format!(
                    "A custom XPath or CSS handler function named '{name}' is being invoked \
                     without a namespace. Please update your query to reference this function \
                     as 'nokogiri:{name}'. Invoking custom handler functions without a \
                     namespace is deprecated and will become an error in Nokogiri v1.17.0."
                ),
            )?;
        }
        self.known.borrow_mut().insert(key, responds);
        Ok(responds)
    }

    fn fail(&self, error: Error) -> XPathError {
        self.failure.borrow_mut().get_or_insert(error);
        aborted()
    }

    /// Calls the handler method with the evaluated arguments, marshalling
    /// them and the result the way
    /// `Nokogiri_marshal_xpath_funcall_and_return_values` does.
    fn invoke(
        &self,
        handler: Value,
        name: &str,
        tree: &Document,
        values: Vec<XPathValue>,
    ) -> Result<XPathValue, Error> {
        // A Ruby Array, not a Vec, so the arguments stay visible to the GC.
        let args = self.ruby.ary_new_capa(values.len());
        for value in values {
            args.push(Outcome::of(tree, value).into_ruby(self.ruby, self.rcv)?)?;
        }
        // SAFETY: nothing touches `args` between borrowing the slice and
        // the call copying it onto the Ruby stack.
        let returned: Value = handler.funcall(name, unsafe { args.as_slice() })?;
        self.returned_value(tree, returned)
    }

    fn returned_value(&self, tree: &Document, value: Value) -> Result<XPathValue, Error> {
        let ruby = self.ruby;
        if Float::from_value(value).is_some() || Integer::from_value(value).is_some() {
            return f64::try_convert(value).map(XPathValue::Number);
        }
        if let Some(s) = RString::from_value(value) {
            return s.to_string().map(XPathValue::String);
        }
        if value.is_nil() {
            // The function left nothing on libxml2's value stack.
            return Err(syntax_error::xpath(
                ruby,
                XPathCode::StackError,
                self.expression,
            ));
        }
        if value.as_raw() == ruby.qtrue().as_raw() {
            return Ok(XPathValue::Boolean(true));
        }
        if value.as_raw() == ruby.qfalse().as_raw() {
            return Ok(XPathValue::Boolean(false));
        }
        let set = if RArray::from_value(value).is_some() {
            let class = classes::get(ruby, &classes::NODE_SET);
            class.new_instance((self.rcv.doc_value, value))?
        } else if Obj::<NodeSetObj>::try_convert(value).is_ok() {
            value
        } else {
            return Err(Error::new(
                ruby.exception_runtime_error(),
                "Invalid return type",
            ));
        };
        let nodes = node_set::members(set)?
            .into_iter()
            .map(|member| self.member_node(tree, member))
            .collect::<Result<_, _>>()?;
        Ok(XPathValue::NodeSet(nodes))
    }

    /// A NodeSet member returned by a handler, as an xmloxide node. It must
    /// be a node of the queried document.
    fn member_node(&self, tree: &Document, member: Value) -> Result<XPathNode, Error> {
        let ruby = self.ruby;
        let foreign = || {
            Error::new(
                ruby.exception_runtime_error(),
                "xmloxide backend: an XPath function may only return nodes of the queried document",
            )
        };
        let node = Obj::<NodeObj>::try_convert(member).map_err(|_| foreign())?;
        let rcv = Receiver::of(ruby, &node)?;
        if rcv.doc_value.as_raw() != self.rcv.doc_value.as_raw() {
            return Err(foreign());
        }
        xpath_node(tree, &rcv.key).ok_or_else(|| gone(ruby))
    }
}

/// `nokogiri-builtin:css-class(hay, needle)`: whether `needle` is one of
/// the whitespace-separated words of `hay`.
fn css_class(call: &ExtensionCall<'_, '_>) -> Result<XPathValue, XPathError> {
    call.check_arg_count("css-class", 2)?;
    let args = call.args()?;
    let hay = call.string(&args[0]);
    let needle = call.string(&args[1]);
    Ok(XPathValue::Boolean(contains_class(
        hay.as_bytes(),
        needle.as_bytes(),
    )))
}

/// A port of `_noko_xml_xpath_context__css_class`: does a word of `hay`
/// start with `needle`, followed by a blank or the end?
fn contains_class(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            let next = i + needle.len();
            if next == hay.len() || is_xml_blank(hay[next]) {
                return true;
            }
        }
        while i < hay.len() && !is_xml_blank(hay[i]) {
            i += 1;
        }
        while i < hay.len() && is_xml_blank(hay[i]) {
            i += 1;
        }
    }
    false
}

/// `nokogiri-builtin:local-name-is(name)`: whether the context node's name
/// is `name`, ignoring namespaces (for HTML5 CSS queries).
fn local_name_is(call: &ExtensionCall<'_, '_>) -> Result<XPathValue, XPathError> {
    use xmloxide::tree::NodeKind;
    call.check_arg_count("local-name-is", 1)?;
    let args = call.args()?;
    let wanted = call.string(&args[0]);
    let doc = call.document();
    let name = match call.context_node() {
        XPathNode::Node(id) => match &doc.node(id).kind {
            NodeKind::Element { name, .. } => Some(name.as_str()),
            NodeKind::ProcessingInstruction { target, .. } => Some(target.as_str()),
            NodeKind::Text { .. } => Some("text"),
            NodeKind::Comment { .. } => Some("comment"),
            _ => None,
        },
        XPathNode::Attribute { owner, index } => doc
            .attributes(owner)
            .get(index as usize)
            .map(|a| a.name.as_str()),
    };
    Ok(XPathValue::Boolean(name == Some(wanted.as_str())))
}

#[cfg(test)]
mod tests {
    use super::contains_class;

    #[test]
    fn css_class_matches_whole_words() {
        assert!(contains_class(b"anything", b""));
        assert!(contains_class(b"asdf", b"asdf"));
        assert!(!contains_class(b"asdf", b"asd"));
        assert!(!contains_class(b"asdf", b"sdf"));
        assert!(!contains_class(b"asdf", b"xasdf"));
        assert!(!contains_class(b"asdf", b"asdfx"));
        assert!(contains_class(b"a \tqwer\nb", b"qwer"));
        assert!(!contains_class(b"a qwer b", b"wer"));
        assert!(contains_class(b"  lead", b"lead"));
    }
}
