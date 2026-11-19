//! Ruby warnings issued the way the C extension issues them.

use magnus::{Error, Ruby, Value, kwargs, prelude::*};

/// A deprecation warning, as `NOKO_WARN_DEPRECATION` (C's
/// `rb_category_warning`) issues it: only when `$VERBOSE` is true and
/// `Warning[:deprecated]` is enabled. An exception raised by a
/// `Warning.warn` hook propagates.
pub fn deprecated(ruby: &Ruby, message: &str) -> Result<(), Error> {
    if !ruby.eval::<Value>("$VERBOSE")?.to_bool() {
        return Ok(());
    }
    let options = kwargs!("uplevel" => 0, "category" => ruby.to_symbol("deprecated"));
    let _: Value = ruby.module_kernel().funcall("warn", (message, options))?;
    Ok(())
}
