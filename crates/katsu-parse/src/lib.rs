//! The frontend: the oxc adapter, scope analysis, and lowering to bytecode.
//!
//! We do not write our own parser. oxc parses TypeScript and JavaScript, passes every
//! test262 stage 4 test, and is maintained under VoidZero with Rolldown and Vite depending
//! on it. Whether it stays the right choice is open question Q9. See `spec/04-frontend.md`.

mod adapter;
pub mod ast;
pub mod lower;
pub mod scope;

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
    /// The source parsed, but it uses a construct the adapter does not translate yet.
    ///
    /// Distinct from `NotLowered` because it comes from a different phase and means a different
    /// thing. This one says our tree has no shape for the syntax. `NotLowered` says the tree has
    /// the shape and there is no bytecode behind it. Collapsing the two would make the M1 work
    /// list harder to read, and the work list is what these variants are for.
    #[error("{path}:{line}:{column}: {construct} is not supported yet")]
    Unsupported {
        /// The file the construct was written in.
        path: String,
        /// One based line.
        line: u32,
        /// One based column, counted in characters.
        column: u32,
        /// What we hit, named the way a JavaScript programmer would name it.
        construct: &'static str,
    },
    /// The source parsed, and then broke a rule the language checks before running anything.
    ///
    /// A redeclared name and a `const` with no initialiser are both errors that a program has to
    /// be refused for even if the line they are on never runs, which is why they are found here
    /// rather than by the interpreter. The message is the one the other engines use, because these
    /// end up in somebody's test expectations.
    #[error("{path}:{line}:{column}: {message}")]
    EarlyError {
        /// The file the rule was broken in.
        path: String,
        /// One based line.
        line: u32,
        /// One based column, counted in characters.
        column: u32,
        /// What is wrong.
        message: String,
    },
    /// Lowering is not implemented for this construct yet.
    ///
    /// This variant exists so that M0 fails loudly and specifically rather than producing
    /// bytecode that is quietly wrong. It shrinks to nothing over M1.
    #[error("{path}:{line}:{column}: {construct} is not lowered yet")]
    NotLowered {
        /// The file being lowered.
        path: String,
        /// One based line.
        line: u32,
        /// One based column, counted in characters.
        column: u32,
        /// What we hit.
        construct: &'static str,
    },
}

impl ParseError {
    /// Whether this is our gap rather than the program's mistake.
    ///
    /// The difference matters more than it looks. A conformance suite is full of files that are
    /// supposed to be rejected, and a runner that cannot tell "this is invalid JavaScript" from
    /// "this is valid JavaScript we have not built yet" will count every one of our own gaps as a
    /// pass on a negative test. That inflates the pass rate by exactly the amount of work left to
    /// do, which is the most misleading direction an error could possibly be wrong in.
    ///
    /// The two variants that mean our gap are the two that name a construct: one says the tree has
    /// no shape for the syntax and the other says the shape exists and there is no bytecode behind
    /// it. Both shrink to nothing over M1 and this function goes with them.
    #[must_use]
    pub const fn is_not_implemented(&self) -> bool {
        matches!(
            self,
            ParseError::Unsupported { .. } | ParseError::NotLowered { .. }
        )
    }
}

/// The result of parsing one source file.
#[derive(Debug)]
pub struct ParsedModule {
    /// The path the source came from, kept for diagnostics and stack traces.
    pub path: String,
    /// The source, adapted into our own tree.
    ///
    /// The oxc allocator that held the original tree is dropped before `parse` returns, so
    /// nothing in here borrows from it. That is the cost of owning our own tree and it is the
    /// price of the parser staying swappable.
    pub ast: ast::Module,
    /// Every name in the tree, resolved to a slot.
    pub scopes: scope::Scopes,
    /// The lowered top level code, with every function written inside it nested under it.
    pub top_level: FunctionBlueprint,
}

impl ParsedModule {
    /// How many top level statements survived adaptation.
    ///
    /// Not the same as the number written, because TypeScript that erases to nothing leaves no
    /// statement behind.
    pub fn statement_count(&self) -> usize {
        self.ast.body.len()
    }
}

/// Parse one source file and report what came out.
///
/// The source type is inferred from the path, so a `.ts` file is parsed as TypeScript and
/// a `.mts` file as an ES module, matching what `spec/04-frontend.md` specifies.
pub fn parse(path: &str, source: &str) -> Result<ParsedModule, ParseError> {
    let (ast, scopes) = frontend(path, source)?;

    let top_level = lower::lower(&ast, &scopes).map_err(|error| {
        let (line, column) = adapter::line_and_column(source, error.span.start);
        ParseError::NotLowered {
            path: path.to_owned(),
            line,
            column,
            construct: error.construct,
        }
    })?;

    Ok(ParsedModule {
        path: path.to_owned(),
        ast,
        scopes,
        top_level,
    })
}

/// Everything before lowering: parse, adapt the tree, and resolve every name in it.
///
/// Split out from `parse` so that scope analysis can be exercised on sources lowering has no
/// bytecode for yet. A test about where a name lives should not fail because of a missing opcode.
fn frontend(path: &str, source: &str) -> Result<(ast::Module, scope::Scopes), ParseError> {
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

    let ast = adapter::adapt(path, &parsed.program)?;
    let scopes = scope::analyse(&ast).map_err(|error| {
        let (line, column) = adapter::line_and_column(source, error.span.start);
        ParseError::EarlyError {
            path: path.to_owned(),
            line,
            column,
            message: error.message,
        }
    })?;

    Ok((ast, scopes))
}

#[cfg(test)]
mod tests {
    use super::ast::{DeclKind, ExprKind, StmtKind};
    use super::{ParseError, parse};

    #[test]
    fn plain_javascript_parses() {
        let module = parse("hello.js", "const x = 1; console.log(x);").expect("should parse");
        assert_eq!(module.statement_count(), 2);
    }

    #[test]
    fn typescript_annotations_parse_because_the_path_says_typescript() {
        let module = parse("hello.ts", "const x: number = 1;").expect("should parse");
        assert_eq!(module.statement_count(), 1);
    }

    #[test]
    fn a_syntax_error_names_the_file() {
        let error = parse("broken.js", "const = ;").expect_err("should not parse");
        let ParseError::Syntax { path, .. } = error else {
            panic!("expected a syntax error, got {error:?}");
        };
        assert_eq!(path, "broken.js");
    }

    #[test]
    fn the_tree_that_comes_back_is_ours_and_not_the_parsers() {
        // The oxc allocator is dropped inside `parse`, so if any of this still borrowed from the
        // arena it would not compile. That is the property this test is really checking, and the
        // assertions below are just there to make it a test rather than a comment.
        let module = parse("hello.js", "const answer = 42;").expect("should parse");

        let StmtKind::Declare { kind, bindings } = &module.ast.body[0].kind else {
            panic!("expected a declaration, got {:?}", module.ast.body[0]);
        };
        assert_eq!(*kind, DeclKind::Const);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].name.name, "answer");

        let Some(init) = &bindings[0].init else {
            panic!("expected an initialiser");
        };
        assert_eq!(init.kind, ExprKind::Number(42.0));
    }

    #[test]
    fn an_unsupported_construct_says_where_it_is() {
        let error =
            parse("loop.js", "let i = 0;\nfor (i of xs) {}").expect_err("should be refused");
        let ParseError::Unsupported {
            path,
            line,
            column,
            construct,
        } = error
        else {
            panic!("expected an unsupported construct, got {error:?}");
        };
        assert_eq!(path, "loop.js");
        assert_eq!(construct, "a for of loop");
        assert_eq!((line, column), (2, 1));
    }
}
