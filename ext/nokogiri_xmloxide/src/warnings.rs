//! Ruby warnings issued the way the C extension issues them.

use magnus::{Error, Ruby, Value, kwargs, prelude::*, rb_sys::FromRawValue};

/// Whether `$VERBOSE` is true, as `rb_category_warning` requires.
fn verbose() -> bool {
    // SAFETY: reading a global variable can't raise, and $VERBOSE always
    // exists; the value is used immediately.
    let value = unsafe { Value::from_raw(rb_sys::rb_gv_get(c"$VERBOSE".as_ptr())) };
    value.to_bool()
}

/// A deprecation warning, as `NOKO_WARN_DEPRECATION` (C's
/// `rb_category_warning`) issues it: only when `$VERBOSE` is true and
/// `Warning[:deprecated]` is enabled. An exception raised by a
/// `Warning.warn` hook propagates.
pub fn deprecated(ruby: &Ruby, message: &str) -> Result<(), Error> {
    if !verbose() {
        return Ok(());
    }
    let options = kwargs!("uplevel" => 0, "category" => ruby.to_symbol("deprecated"));
    let _: Value = ruby.module_kernel().funcall("warn", (message, options))?;
    Ok(())
}
