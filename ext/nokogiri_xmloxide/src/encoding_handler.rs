//! `Nokogiri::EncodingHandler`, mirroring `ext/nokogiri/xml_encoding_handler.c`.
//!
//! libxml2 keeps a process-wide table of encoding aliases; this keeps the same
//! table in Rust. A handler exists for a name when xmloxide can decode that
//! encoding (after resolving aliases), so lookups reflect what the parser can
//! actually do rather than a separate list.

use std::sync::Mutex;

use magnus::{Error, Module, Object, RClass, RModule, Ruby, function, method};

/// Alias table: (alias in ASCII uppercase, real encoding name). libxml2
/// uppercases aliases on insert and lookup, so matching is case-insensitive.
static ALIASES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

#[magnus::wrap(
    class = "Nokogiri::EncodingHandler",
    free_immediately,
    frozen_shareable
)]
struct EncodingHandler {
    name: String,
}

impl EncodingHandler {
    /// `EncodingHandler#name`: the resolved encoding name.
    fn name(&self) -> String {
        self.name.clone()
    }
}

pub fn init(ruby: &Ruby, nokogiri: RModule) -> Result<(), Error> {
    let class = nokogiri.define_class("EncodingHandler", ruby.class_object())?;
    class.define_singleton_method("[]", function!(get, 1))?;
    class.define_singleton_method("delete", function!(delete, 1))?;
    class.define_singleton_method("alias", function!(add_alias, 2))?;
    class.define_singleton_method("clear_aliases!", method!(clear_aliases, 0))?;
    class.define_method("name", method!(EncodingHandler::name, 0))?;
    Ok(())
}

/// `EncodingHandler.[](name)`: the handler for `name`, or nil.
fn get(name: String) -> Option<EncodingHandler> {
    let resolved = resolve_alias(&name).unwrap_or(name);
    is_decodable(&resolved).then_some(EncodingHandler { name: resolved })
}

/// `EncodingHandler.alias(real_name, alias_name)`: returns `alias_name`.
fn add_alias(real_name: String, alias_name: String) -> String {
    let key = alias_name.to_ascii_uppercase();
    let mut aliases = lock_aliases();
    match aliases.iter_mut().find(|(a, _)| *a == key) {
        Some(entry) => entry.1 = real_name,
        None => aliases.push((key, real_name)),
    }
    alias_name
}

/// `EncodingHandler.delete(name)`: true if an alias was removed, else nil.
fn delete(alias_name: String) -> Option<bool> {
    let key = alias_name.to_ascii_uppercase();
    let mut aliases = lock_aliases();
    let before = aliases.len();
    aliases.retain(|(a, _)| *a != key);
    (aliases.len() < before).then_some(true)
}

/// `EncodingHandler.clear_aliases!`: removes every alias; returns the class.
fn clear_aliases(class: RClass) -> RClass {
    lock_aliases().clear();
    class
}

/// The encoding `name` is an alias for, if it is one.
pub fn resolve_alias(name: &str) -> Option<String> {
    let key = name.to_ascii_uppercase();
    lock_aliases()
        .iter()
        .find(|(a, _)| *a == key)
        .map(|(_, real)| real.clone())
}

/// True when xmloxide can transcode from `name`. Decoding nothing succeeds
/// exactly when the encoding label is recognised.
fn is_decodable(name: &str) -> bool {
    xmloxide::encoding::transcode(b"", name).is_ok()
}

/// The table holds plain strings, so a panic mid-update can't leave it
/// inconsistent; recover from poisoning rather than propagate it.
fn lock_aliases() -> std::sync::MutexGuard<'static, Vec<(String, String)>> {
    ALIASES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
