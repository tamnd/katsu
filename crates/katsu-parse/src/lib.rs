//! The frontend: the oxc adapter, scope analysis, and lowering to bytecode.
//!
//! We do not write our own parser. oxc parses TypeScript and JavaScript, passes every
//! test262 stage 4 test, and is maintained under VoidZero with Rolldown and Vite depending
//! on it. Whether it stays the right choice is open question Q9. See `spec/04-frontend.md`.

use katsu_ir::FunctionBlueprint;
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;

/// A syntax error, already formatted for a user rather than for a debugger.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The source did not parse.
    #[error("{path}: {message}")]
    Syntax {
        /// The file the error came from.
        path: String,
        /// The first diagnostic, rendered.
        message: String,
    },
    /// Lowering is not implemented for this construct yet.
    ///
    /// This variant exists so that M0 fails loudly and specifically rather than producing
    /// bytecode that is quietly wrong. It shrinks to nothing over M1.
    #[error("{path}: {construct} is not lowered yet")]
    NotLowered {
        /// The file being lowered.
        path: String,
        /// What we hit.
        construct: &'static str,
    },
}

/// The result of parsing one source file.
#[derive(Debug)]
pub struct ParsedModule {
    /// The path the source came from, kept for diagnostics and stack traces.
    pub path: String,
    /// How many top level statements the program has.
    ///
    /// A placeholder for the real lowered output while M0 is in progress. It is here so
    /// that the seam between parsing and lowering is exercised by a test rather than
    /// being an empty function nobody calls.
    pub statement_count: usize,
    /// The lowered top level code, empty until lowering lands in M0.
    pub top_level: FunctionBlueprint,
}

/// Parse one source file and report what came out.
///
/// The source type is inferred from the path, so a `.ts` file is parsed as TypeScript and
/// a `.mts` file as an ES module, matching what `spec/04-frontend.md` specifies.
pub fn parse(path: &str, source: &str) -> Result<ParsedModule, ParseError> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    // oxc's own documentation is explicit that an empty `diagnostics` is the check for a
    // valid AST, and that the list is not comprehensive because the expensive checks are
    // offloaded to semantic analysis. Scope analysis in M0 runs that second pass.
    if let Some(first) = parsed.diagnostics.first() {
        return Err(ParseError::Syntax {
            path: path.to_owned(),
            message: first.to_string(),
        });
    }

    Ok(ParsedModule {
        path: path.to_owned(),
        statement_count: parsed.program.body.len(),
        top_level: FunctionBlueprint::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::{ParseError, parse};

    #[test]
    fn plain_javascript_parses() {
        let module = parse("hello.js", "const x = 1; console.log(x);").expect("should parse");
        assert_eq!(module.statement_count, 2);
    }

    #[test]
    fn typescript_annotations_parse_because_the_path_says_typescript() {
        let module = parse("hello.ts", "const x: number = 1;").expect("should parse");
        assert_eq!(module.statement_count, 1);
    }

    #[test]
    fn a_syntax_error_names_the_file() {
        let error = parse("broken.js", "const = ;").expect_err("should not parse");
        let ParseError::Syntax { path, .. } = error else {
            panic!("expected a syntax error, got {error:?}");
        };
        assert_eq!(path, "broken.js");
    }
}
