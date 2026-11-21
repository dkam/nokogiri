//! xmloxide diagnostics as `Nokogiri::XML::SyntaxError` objects, mirroring
//! `noko_xml_syntax_error__wrap` in `ext/nokogiri/xml_syntax_error.c`.

use magnus::{Error, Exception, RArray, RClass, RObject, Ruby, Value, prelude::*};
use xmloxide::error::{ErrorSeverity, ParseDiagnostic, ParseError};
use xmloxide::xpath::XPathError;

use crate::classes;

/// libxml2's `XML_FROM_PARSER` error domain.
const DOMAIN_PARSER: i64 = 1;
/// A parse error as `add_errors` in `gumbo.c` reports it: the `code` in
/// `str1`, the URL (or `"#fragment"`) in `file`.
pub fn gumbo(
    ruby: &Ruby,
    message: &str,
    code: &str,
    line: u32,
    column: u32,
    file: Value,
) -> Result<Value, Error> {
    let error = build_fields(
        ruby,
        &Fields {
            class: classes::get(ruby, &classes::XML_SYNTAX_ERROR),
            domain: DOMAIN_PARSER,
            // XML_ERR_INTERNAL_ERROR, as gumbo.c sets it.
            code: 1,
            message,
            level: LEVEL_ERROR,
            line,
            column,
            str1: Some(code),
        },
    )?;
    RObject::try_convert(error)?.ivar_set("@file", file)?;
    Ok(error)
}

/// libxml2's `XML_FROM_XPATH` error domain.
const DOMAIN_XPATH: i64 = 12;
/// libxml2's `XML_ERR_ERROR` level.
const LEVEL_ERROR: i64 = 2;

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
    build_fields(
        ruby,
        &Fields {
            class: classes::get(ruby, &classes::XML_SYNTAX_ERROR),
            domain: DOMAIN_PARSER,
            // xmloxide has no libxml2 error codes; 1 is XML_ERR_INTERNAL_ERROR.
            code: 1,
            message,
            level,
            line,
            column,
            str1: None,
        },
    )
}

/// What `noko_xml_syntax_error__wrap` copies out of an `xmlError`.
struct Fields<'a> {
    class: RClass,
    domain: i64,
    code: i64,
    message: &'a str,
    level: i64,
    line: u32,
    column: u32,
    str1: Option<&'a str>,
}

fn build_fields(ruby: &Ruby, fields: &Fields) -> Result<Value, Error> {
    let error: Value = fields.class.new_instance((ruby.str_new(fields.message),))?;
    let object = RObject::try_convert(error)?;
    object.ivar_set("@domain", fields.domain)?;
    object.ivar_set("@code", fields.code)?;
    object.ivar_set("@level", fields.level)?;
    object.ivar_set("@file", ruby.qnil())?;
    object.ivar_set("@line", fields.line)?;
    object.ivar_set("@path", ruby.qnil())?;
    object.ivar_set("@str1", fields.str1)?;
    object.ivar_set("@str2", ruby.qnil())?;
    object.ivar_set("@str3", ruby.qnil())?;
    object.ivar_set("@int1", 0)?;
    object.ivar_set("@column", fields.column)?;
    Ok(error)
}

/// libxml2's `xmlXPathError` codes that the backend reports, with the
/// messages from libxml2's `xmlXPathErrorMessages`.
#[derive(Clone, Copy)]
pub enum XPathCode {
    UndefVariable = 5,
    Expr = 7,
    UnknownFunc = 9,
    InvalidType = 11,
    InvalidArity = 12,
    UndefPrefix = 19,
    StackError = 23,
}

impl XPathCode {
    fn message(self) -> &'static str {
        match self {
            Self::UndefVariable => "Undefined variable\n",
            Self::Expr => "Invalid expression\n",
            Self::UnknownFunc => "Unregistered function\n",
            Self::InvalidType => "Invalid type\n",
            Self::InvalidArity => "Invalid number of arguments\n",
            Self::UndefPrefix => "Undefined namespace prefix\n",
            Self::StackError => "Stack usage error\n",
        }
    }

    fn of(error: &XPathError) -> Self {
        match error {
            XPathError::UndefinedVariable { .. } => Self::UndefVariable,
            XPathError::UndefinedFunction { .. } => Self::UnknownFunc,
            XPathError::TypeError { .. } => Self::InvalidType,
            XPathError::InvalidArgCount { .. } => Self::InvalidArity,
            XPathError::UndefinedNamespacePrefix { .. } => Self::UndefPrefix,
            _ => Self::Expr,
        }
    }
}

/// A `Nokogiri::XML::XPath::SyntaxError` as `xmlXPathErr` reports it: the
/// expression in `str1`, no line or column.
pub fn xpath(ruby: &Ruby, code: XPathCode, expression: &str) -> Error {
    let fields = Fields {
        class: classes::get(ruby, &classes::XPATH_SYNTAX_ERROR),
        domain: DOMAIN_XPATH,
        // libxml2's XML_XPATH_EXPRESSION_OK is 1200.
        code: 1200 + code as i64,
        message: code.message(),
        level: LEVEL_ERROR,
        line: 0,
        column: 0,
        str1: Some(expression),
    };
    match build_fields(ruby, &fields).map(Exception::from_value) {
        Ok(Some(exception)) => exception.into(),
        Ok(None) => Error::new(ruby.exception_runtime_error(), code.message().trim_end()),
        Err(error) => error,
    }
}

/// The XPath::SyntaxError for an xmloxide XPath error.
pub fn xpath_error(ruby: &Ruby, error: &XPathError, expression: &str) -> Error {
    xpath(ruby, XPathCode::of(error), expression)
}

/// libxml2's `XML_FROM_IO` error domain.
const DOMAIN_IO: i64 = 8;
/// libxml2's `XML_IO_UNKNOWN`.
const CODE_IO_UNKNOWN: i64 = 1500;

/// The fatal error libxml2 reports when an `xmlReadIO` callback fails.
pub fn io_error(ruby: &Ruby) -> Result<Value, Error> {
    build_fields(
        ruby,
        &Fields {
            class: classes::get(ruby, &classes::XML_SYNTAX_ERROR),
            domain: DOMAIN_IO,
            code: CODE_IO_UNKNOWN,
            message: "Unknown IO error\n",
            level: 3,
            line: 0,
            column: 0,
            str1: None,
        },
    )
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
