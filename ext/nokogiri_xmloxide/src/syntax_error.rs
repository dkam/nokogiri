//! xmloxide diagnostics as `Nokogiri::XML::SyntaxError` objects, mirroring
//! `noko_xml_syntax_error__wrap` in `ext/nokogiri/xml_syntax_error.c`.

use magnus::{Error, Exception, RArray, RObject, Ruby, Value, prelude::*};
use xmloxide::error::{ErrorSeverity, ParseDiagnostic, ParseError};

use crate::classes;

/// libxml2's `XML_FROM_PARSER` error domain.
const DOMAIN_PARSER: i64 = 1;

pub fn from_diagnostic(ruby: &Ruby, diagnostic: &ParseDiagnostic) -> Result<Value, Error> {
    let level = match diagnostic.severity {
        ErrorSeverity::Warning => 1,
        ErrorSeverity::Error => 2,
        ErrorSeverity::Fatal => 3,
    };
    build(
        ruby,
        &diagnostic.message,
        level,
        diagnostic.location.line,
        diagnostic.location.column,
    )
}

fn build(ruby: &Ruby, message: &str, level: i64, line: u32, column: u32) -> Result<Value, Error> {
    let class = classes::get(ruby, &classes::XML_SYNTAX_ERROR);
    let error: Value = class.new_instance((ruby.str_new(message),))?;
    let object = RObject::try_convert(error)?;
    object.ivar_set("@domain", DOMAIN_PARSER)?;
    // xmloxide has no libxml2 error codes; 1 is XML_ERR_INTERNAL_ERROR.
    object.ivar_set("@code", 1)?;
    object.ivar_set("@level", level)?;
    object.ivar_set("@file", ruby.qnil())?;
    object.ivar_set("@line", line)?;
    object.ivar_set("@path", ruby.qnil())?;
    object.ivar_set("@str1", ruby.qnil())?;
    object.ivar_set("@str2", ruby.qnil())?;
    object.ivar_set("@str3", ruby.qnil())?;
    object.ivar_set("@int1", 0)?;
    object.ivar_set("@column", column)?;
    Ok(error)
}

/// All the diagnostics as a Ruby Array of SyntaxErrors.
pub fn array(ruby: &Ruby, diagnostics: &[ParseDiagnostic]) -> Result<RArray, Error> {
    let errors = ruby.ary_new();
    for diagnostic in diagnostics {
        errors.push(from_diagnostic(ruby, diagnostic)?)?;
    }
    Ok(errors)
}

/// The errors from a failed parse: its diagnostics, then the fatal error
/// itself unless a diagnostic already reported it.
pub fn array_for_failure(ruby: &Ruby, failure: &ParseError) -> Result<RArray, Error> {
    let errors = array(ruby, &failure.diagnostics)?;
    if !failure
        .diagnostics
        .iter()
        .any(|d| d.message == failure.message)
    {
        errors.push(build(
            ruby,
            &failure.message,
            3,
            failure.location.line,
            failure.location.column,
        )?)?;
    }
    Ok(errors)
}

/// Raises what the C extension raises when parsing fails outright:
/// `SyntaxError.aggregate(errors)`, or a RuntimeError if there are none.
pub fn raise_aggregate(ruby: &Ruby, errors: RArray) -> Error {
    let class = classes::get(ruby, &classes::XML_SYNTAX_ERROR);
    match class.funcall::<_, _, Value>("aggregate", (errors,)) {
        Ok(exception) if exception.is_nil() => {
            Error::new(ruby.exception_runtime_error(), "Could not parse document")
        }
        Ok(exception) => match Exception::from_value(exception) {
            Some(exception) => exception.into(),
            None => Error::new(ruby.exception_runtime_error(), "Could not parse document"),
        },
        Err(error) => error,
    }
}
