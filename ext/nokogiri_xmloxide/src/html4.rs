//! `Nokogiri::HTML4::ElementDescription` and `EntityLookup`, mirroring
//! `ext/nokogiri/html4_element_description.c` and `html4_entity_lookup.c`,
//! over libxml2's tables in `html4_tables`.

use magnus::{Error, Module, RArray, RClass, RModule, Ruby, Value, method, prelude::*};

use crate::html4_tables::{ElementInfo, entity_lookup, lookup};

/// An element's description: a pointer into the static table, as in C.
#[magnus::wrap(
    class = "Nokogiri::HTML4::ElementDescription",
    free_immediately,
    frozen_shareable
)]
struct ElementDescription(&'static ElementInfo);

impl ElementDescription {
    fn name(&self) -> &'static str {
        self.0.name
    }

    fn implied_start_tag(&self) -> bool {
        self.0.start_tag != 0
    }

    fn implied_end_tag(&self) -> bool {
        self.0.end_tag != 0
    }

    fn save_end_tag(&self) -> bool {
        self.0.save_end_tag != 0
    }

    fn empty(&self) -> bool {
        self.0.empty != 0
    }

    fn deprecated(&self) -> bool {
        self.0.deprecated != 0
    }

    fn inline(&self) -> bool {
        self.0.inline != 0
    }

    fn description(&self) -> &'static str {
        self.0.description
    }

    /// libxml2 2.14 no longer lists an element's sub-elements or
    /// attributes, so these are always empty, and there is no default
    /// sub-element.
    fn empty_list(ruby: &Ruby, _rb_self: &Self) -> RArray {
        ruby.ary_new()
    }

    fn default_sub_element(&self) -> Option<String> {
        None
    }
}

pub fn init(ruby: &Ruby, html4: RModule) -> Result<(), Error> {
    let class = html4.define_class("ElementDescription", ruby.class_object())?;
    class.undef_default_alloc_func();
    class.define_singleton_method("[]", method!(get_description, 1))?;
    class.define_method("name", method!(ElementDescription::name, 0))?;
    class.define_method(
        "implied_start_tag?",
        method!(ElementDescription::implied_start_tag, 0),
    )?;
    class.define_method(
        "implied_end_tag?",
        method!(ElementDescription::implied_end_tag, 0),
    )?;
    class.define_method(
        "save_end_tag?",
        method!(ElementDescription::save_end_tag, 0),
    )?;
    class.define_method("empty?", method!(ElementDescription::empty, 0))?;
    class.define_method("deprecated?", method!(ElementDescription::deprecated, 0))?;
    class.define_method("inline?", method!(ElementDescription::inline, 0))?;
    class.define_method("description", method!(ElementDescription::description, 0))?;
    for name in [
        "sub_elements",
        "optional_attributes",
        "deprecated_attributes",
        "required_attributes",
    ] {
        class.define_method(name, method!(ElementDescription::empty_list, 0))?;
    }
    class.define_method(
        "default_sub_element",
        method!(ElementDescription::default_sub_element, 0),
    )?;

    let entity_lookup = html4.define_class("EntityLookup", ruby.class_object())?;
    entity_lookup.define_method("get", method!(get_entity, 1))?;
    Ok(())
}

/// `ElementDescription.[](tag_name)`: the description of the element
/// called `tag_name`, ignoring case, or nil. It is an instance of the
/// receiver, which may be a subclass.
fn get_description(ruby: &Ruby, class: RClass, tag_name: String) -> Option<Value> {
    lookup(&tag_name).map(|info| ruby.obj_wrap_as(ElementDescription(info), class).as_value())
}

/// `EntityLookup#get(name)`: an `EntityDescription` of the entity's code
/// point, name and description, or nil.
fn get_entity(ruby: &Ruby, _rb_self: Value, name: String) -> Result<Option<Value>, Error> {
    let Some(&(value, name, description)) = entity_lookup(&name) else {
        return Ok(None);
    };
    let html4: RModule = ruby.class_object().const_get("Nokogiri")?;
    let html4: RModule = html4.const_get("HTML4")?;
    let class: RClass = html4.const_get("EntityDescription")?;
    class.new_instance((value, name, description)).map(Some)
}
