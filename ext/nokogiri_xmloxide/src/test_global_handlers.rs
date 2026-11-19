//! `Nokogiri::Test`, mirroring `ext/nokogiri/test_global_handlers.c`.
//!
//! The C version installs the block as libxml2's process-wide structured error
//! handler, so the test suite can check that Nokogiri never lets an error leak
//! to a global handler. xmloxide has no global error state: every error stays
//! with the document or parse that produced it. So there is nothing to
//! install, and the block can never be called, which is what the tests assert.

use magnus::{Error, Module, Object, RModule, Ruby, function};

pub fn init(nokogiri: RModule) -> Result<(), Error> {
    let test = nokogiri.define_module("Test")?;
    test.define_singleton_method(
        "__foreign_error_handler",
        function!(foreign_error_handler, 0),
    )?;
    Ok(())
}

/// `Nokogiri::Test.__foreign_error_handler { ... } -> nil`. Like the C
/// version (`rb_need_block`), raises LocalJumpError without a block.
fn foreign_error_handler(ruby: &Ruby) -> Result<(), Error> {
    ruby.block_proc()?;
    Ok(())
}
