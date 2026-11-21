//! Ruby classes the backend instantiates, looked up once and cached.
//!
//! `lib.rs` defines them all at load time, so the lookups can't fail once
//! `Init_nokogiri` has finished.

use magnus::{RClass, Ruby, value::Lazy};

macro_rules! class {
    ($name:ident, $path:literal) => {
        pub static $name: Lazy<RClass> = Lazy::new(|ruby| {
            ruby.eval($path)
                .expect(concat!("class ", $path, " is defined at load"))
        });
    };
}

class!(NODE, "Nokogiri::XML::Node");
class!(ELEMENT, "Nokogiri::XML::Element");
class!(ATTR, "Nokogiri::XML::Attr");
class!(TEXT, "Nokogiri::XML::Text");
class!(CDATA, "Nokogiri::XML::CDATA");
class!(COMMENT, "Nokogiri::XML::Comment");
class!(
    PROCESSING_INSTRUCTION,
    "Nokogiri::XML::ProcessingInstruction"
);
class!(ENTITY_REFERENCE, "Nokogiri::XML::EntityReference");
class!(DTD, "Nokogiri::XML::DTD");
class!(XML_DOCUMENT, "Nokogiri::XML::Document");
class!(HTML4_DOCUMENT, "Nokogiri::HTML4::Document");
class!(DOCUMENT_FRAGMENT, "Nokogiri::XML::DocumentFragment");
class!(NODE_SET, "Nokogiri::XML::NodeSet");
class!(NAMESPACE, "Nokogiri::XML::Namespace");
class!(XML_SYNTAX_ERROR, "Nokogiri::XML::SyntaxError");
class!(XPATH_SYNTAX_ERROR, "Nokogiri::XML::XPath::SyntaxError");
class!(XPATH_CONTEXT, "Nokogiri::XML::XPathContext");

/// Shorthand for `ruby.get_inner(&CLASS)`.
pub fn get(ruby: &Ruby, class: &'static Lazy<RClass>) -> RClass {
    ruby.get_inner(class)
}
