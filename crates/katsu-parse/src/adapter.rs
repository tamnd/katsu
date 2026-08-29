//! The only module in the workspace that is allowed to name an oxc type.
//!
//! `spec/04-frontend.md` says we do not write our own parser and that the risk of leaning on
//! somebody else's is contained by keeping the dependency behind a thin adapter, so that moving to
//! a different parser is a week of work rather than a rewrite. This file is that adapter, and the
//! containment only holds if it stays the boundary. Everything above it sees `crate::ast` and
//! nothing else.
//!
//! There are three jobs here and it is worth naming them separately, because they have different
//! failure modes.
//!
//! The first is translation, turning oxc's tree into ours node by node. Dull, mechanical, and the
//! part that has to be rewritten if the parser is ever swapped.
//!
//! The second is TypeScript erasure. `spec/04-frontend.md` chose to erase types rather than check
//! them, which is what esbuild and swc do and what Node's own type stripping does, so a type
//! annotation, an interface, a type alias and a `declare` are all dropped, and `as`, `satisfies`,
//! a type assertion and a non null assertion are unwrapped to the expression underneath. The
//! TypeScript constructs that are not erasable, because they emit code, are refused by name rather
//! than dropped, since dropping an enum declaration would leave every reference to it broken in a
//! way that looks like a bug in the runtime.
//!
//! The third is refusing what M0 does not cover. Every refusal names the construct and the place
//! it was written. This is the part that shrinks over M1 and M2 until it is empty, and while it is
//! not empty it has to be loud, because a runtime that quietly produces wrong bytecode for syntax
//! it does not understand is worse than one that admits it.

use oxc_ast::ast as oxc;

use crate::ParseError;
use crate::ast::{
    AssignOp, BinaryOp, Binding, Block, Case, Catch, DeclKind, Expr, ExprKind, ForInit, Func,
    Ident, LogicalOp, Module, Property, PropertyKind, Span, Stmt, StmtKind, Target, TargetKind,
    UnaryOp, UpdateOp,
};

/// Turn one parsed program into our tree.
pub(crate) fn adapt(path: &str, program: &oxc::Program<'_>) -> Result<Module, ParseError> {
    let adapter = Adapter {
        path,
        source: program.source_text,
    };

    // A module is strict whatever it says, and a script is strict only if it opens with the
    // directive. Getting this wrong is not a syntax error, it is a silently different program, so
    // it is decided once here where the source type is still in hand.
    let strict = program.source_type.is_module()
        || program
            .directives
            .iter()
            .any(|directive| directive.directive.as_str() == "use strict");

    let body = adapter.statements(&program.body, strict)?;

    Ok(Module {
        path: path.to_owned(),
        body,
        strict,
    })
}

/// The words that stop being identifiers once the code is strict.
///
/// They are reserved for a language that was never written, which is why they are only reserved in
/// the mode that could afford to reserve them. `enum` is missing on purpose: it is reserved in both
/// modes, so the parser rejects it before we get here and with a different message. `await` is
/// missing for the opposite reason, since what makes it special is a module or an async function
/// and not strictness, and neither exists in the subset yet.
const STRICT_RESERVED: [&str; 9] = [
    "implements",
    "interface",
    "let",
    "package",
    "private",
    "protected",
    "public",
    "static",
    "yield",
];

/// Carries what an error message needs: the file it came from and the text to find a line in.
struct Adapter<'a> {
    path: &'a str,
    source: &'a str,
}

impl Adapter<'_> {
    /// Report a construct we do not handle, pointing at where it was written.
    fn refuse<T>(&self, construct: &'static str, span: oxc::Span) -> Result<T, ParseError> {
        let (line, column) = line_and_column(self.source, span.start);
        Err(ParseError::Unsupported {
            path: self.path.to_owned(),
            line,
            column,
            construct,
        })
    }

    /// Report a rule the language checks before anything runs, pointing at where it was broken.
    fn early<T>(&self, message: &str, span: oxc::Span) -> Result<T, ParseError> {
        let (line, column) = line_and_column(self.source, span.start);
        Err(ParseError::EarlyError {
            path: self.path.to_owned(),
            line,
            column,
            message: message.to_owned(),
        })
    }

    /// Check a name being read, which in strict mode cannot be one of the reserved words.
    ///
    /// Reading is the weaker of the two positions and so this is the weaker of the two checks. A
    /// reserved word is not an identifier at all in strict code, so `public;` on its own is a
    /// `SyntaxError` before anything runs, while `eval` and `arguments` are ordinary names to read
    /// and only stop being ordinary when something writes to one or binds one.
    ///
    /// A property is not a name in this sense and is not checked anywhere, which is why `o.public`
    /// and `{ public: 1 }` are both fine in strict code. The two never meet because a property name
    /// is adapted at its own site rather than through here.
    fn reference(&self, name: &str, span: oxc::Span, strict: bool) -> Result<(), ParseError> {
        if strict && STRICT_RESERVED.contains(&name) {
            return self.early("Unexpected strict mode reserved word", span);
        }
        Ok(())
    }

    /// Check a name being bound or written, which is the position with both rules in it.
    ///
    /// `eval` and `arguments` are the two names strict mode will not let a program move, because
    /// the whole point of the mode is that a reader can tell what a name refers to, and rebinding
    /// either of those takes that away. Everything a `var`, a `let`, a `const`, a function name, a
    /// parameter, a catch parameter and an assignment target has in common is that it moves a name,
    /// which is why they all arrive here.
    fn writable(&self, name: &str, span: oxc::Span, strict: bool) -> Result<(), ParseError> {
        self.reference(name, span, strict)?;
        if strict && (name == "eval" || name == "arguments") {
            return self.early("Unexpected eval or arguments in strict mode", span);
        }
        Ok(())
    }

    /// Adapt a list of statements, dropping the ones that erase to nothing.
    ///
    /// The drop is why this returns a `Vec` rather than mapping one to one. An interface
    /// declaration is a statement in the source and no statement at all afterwards, and pretending
    /// otherwise would mean inventing an empty node with a span over TypeScript that no longer
    /// exists.
    fn statements(
        &self,
        body: &[oxc::Statement<'_>],
        strict: bool,
    ) -> Result<Vec<Stmt>, ParseError> {
        let mut out = Vec::with_capacity(body.len());
        for statement in body {
            if let Some(statement) = self.statement(statement, strict)? {
                out.push(statement);
            }
        }
        Ok(out)
    }

    /// Adapt one statement, or `None` if it erases to nothing.
    fn statement(
        &self,
        statement: &oxc::Statement<'_>,
        strict: bool,
    ) -> Result<Option<Stmt>, ParseError> {
        let adapted = match statement {
            oxc::Statement::ExpressionStatement(node) => Stmt::new(
                span(node.span),
                StmtKind::Expr(self.expression(&node.expression, strict)?),
            ),

            oxc::Statement::EmptyStatement(node) => Stmt::new(span(node.span), StmtKind::Empty),

            oxc::Statement::BlockStatement(node) => Stmt::new(
                span(node.span),
                StmtKind::Block(self.statements(&node.body, strict)?),
            ),

            oxc::Statement::ReturnStatement(node) => {
                let argument = match node.argument.as_ref() {
                    Some(argument) => Some(self.expression(argument, strict)?),
                    None => None,
                };
                Stmt::new(span(node.span), StmtKind::Return(argument))
            }

            oxc::Statement::IfStatement(node) => {
                let test = self.expression(&node.test, strict)?;
                let consequent = self.branch(&node.consequent, strict)?;
                let alternate = match node.alternate.as_ref() {
                    Some(alternate) => Some(self.branch(alternate, strict)?),
                    None => None,
                };
                Stmt::new(
                    span(node.span),
                    StmtKind::If {
                        test,
                        consequent,
                        alternate,
                    },
                )
            }

            oxc::Statement::WhileStatement(node) => {
                let test = self.expression(&node.test, strict)?;
                let body = self.branch(&node.body, strict)?;
                Stmt::new(span(node.span), StmtKind::While { test, body })
            }

            oxc::Statement::DoWhileStatement(node) => {
                let body = self.branch(&node.body, strict)?;
                let test = self.expression(&node.test, strict)?;
                Stmt::new(span(node.span), StmtKind::DoWhile { body, test })
            }

            oxc::Statement::ForStatement(node) => self.for_statement(node, strict)?,

            oxc::Statement::SwitchStatement(node) => self.switch_statement(node, strict)?,

            oxc::Statement::ThrowStatement(node) => Stmt::new(
                span(node.span),
                StmtKind::Throw(self.expression(&node.argument, strict)?),
            ),

            oxc::Statement::TryStatement(node) => self.try_statement(node, strict)?,

            // A label is an identifier and it obeys the identifier spelling rules, so `break public`
            // is refused in strict code even though the label is not a binding and could never be
            // read. Whether the label names anything is scope analysis's question and not this one.
            oxc::Statement::LabeledStatement(node) => {
                self.reference(node.label.name.as_str(), node.label.span, strict)?;
                let label = Ident::new(span(node.label.span), node.label.name.as_str());
                let body = self.branch(&node.body, strict)?;
                Stmt::new(span(node.span), StmtKind::Labeled { label, body })
            }

            oxc::Statement::BreakStatement(node) => {
                let label = self.label(node.label.as_ref(), strict)?;
                Stmt::new(span(node.span), StmtKind::Break(label))
            }

            oxc::Statement::ContinueStatement(node) => {
                let label = self.label(node.label.as_ref(), strict)?;
                Stmt::new(span(node.span), StmtKind::Continue(label))
            }

            oxc::Statement::VariableDeclaration(node) => {
                // `declare const x: number` describes something that exists elsewhere and emits no
                // code, so it erases the same way an interface does.
                if node.declare {
                    return Ok(None);
                }
                self.variable_declaration(node, strict)?
            }

            oxc::Statement::FunctionDeclaration(node) => {
                // An overload signature and a `declare function` both have no body. There is
                // nothing to lower and the implementation that follows carries the code.
                let Some(function) = self.function(node, strict)? else {
                    return Ok(None);
                };
                Stmt::new(span(node.span), StmtKind::Function(Box::new(function)))
            }

            // Types erase. This is the whole of TypeScript support in M0 and it is deliberate:
            // `spec/04-frontend.md` chose erasure over checking, so a type alias, an interface and
            // a `declare module` leave nothing behind at run time and we drop them here.
            oxc::Statement::TSTypeAliasDeclaration(_)
            | oxc::Statement::TSInterfaceDeclaration(_)
            | oxc::Statement::TSGlobalDeclaration(_)
            | oxc::Statement::TSExternalModuleDeclaration(_) => return Ok(None),

            // These do not erase, because they emit code. Refusing them by name is the honest
            // answer until there is somewhere to lower them to.
            oxc::Statement::TSEnumDeclaration(node) => {
                return self.refuse("a TypeScript enum", node.span);
            }
            oxc::Statement::TSNamespaceDeclaration(node) => {
                return self.refuse("a TypeScript namespace", node.span);
            }
            oxc::Statement::TSImportEqualsDeclaration(node) => {
                return self.refuse("a TypeScript import assignment", node.span);
            }

            other => return self.refuse(statement_name(other), statement_span(other)),
        };

        Ok(Some(adapted))
    }

    /// Adapt a `switch` and its clauses.
    ///
    /// A `default` keeps its position in the list and is the clause with no test, rather than being
    /// pulled out into a field of its own. It has to be compared after every case and laid out where
    /// it was written, so the list is the only shape that says both without one of them being
    /// reconstructed later.
    fn switch_statement(
        &self,
        node: &oxc::SwitchStatement<'_>,
        strict: bool,
    ) -> Result<Stmt, ParseError> {
        let discriminant = self.expression(&node.discriminant, strict)?;
        let mut cases = Vec::with_capacity(node.cases.len());
        for case in &node.cases {
            let test = match case.test.as_ref() {
                Some(test) => Some(self.expression(test, strict)?),
                None => None,
            };
            cases.push(Case {
                span: span(case.span),
                test,
                body: self.statements(&case.consequent, strict)?,
            });
        }
        Ok(Stmt::new(
            span(node.span),
            StmtKind::Switch {
                discriminant,
                cases,
            },
        ))
    }

    /// Adapt the label a `break` or a `continue` was written with, if it was written with one.
    ///
    /// The spelling rules apply here for the same reason they apply to the label on the statement,
    /// which is that both of them are the same production in the grammar.
    fn label(
        &self,
        label: Option<&oxc::LabelIdentifier<'_>>,
        strict: bool,
    ) -> Result<Option<Ident>, ParseError> {
        match label {
            None => Ok(None),
            Some(label) => {
                self.reference(label.name.as_str(), label.span, strict)?;
                Ok(Some(Ident::new(span(label.span), label.name.as_str())))
            }
        }
    }

    /// Adapt a `for`, the three part one.
    ///
    /// Every part is optional and `for (;;)` is a legal endless loop, so all three arrive as
    /// options and stay options. Filling an absent test in with `true` here would be a small lie
    /// that costs a comparison on every iteration of the loops that leave it out on purpose.
    ///
    /// The head is a declaration or an expression and the grammar allows nothing else, which is why
    /// it becomes a `ForInit` rather than a `Stmt`. Adapting it as a statement would have meant
    /// every reader downstream handling arms the parser can never hand it.
    fn for_statement(
        &self,
        node: &oxc::ForStatement<'_>,
        strict: bool,
    ) -> Result<Stmt, ParseError> {
        let init = match node.init.as_ref() {
            None => None,
            Some(oxc::ForStatementInit::VariableDeclaration(declaration)) => {
                // A `declare` in this position is not valid syntax, so unlike a statement level
                // declaration there is nothing that erases to nothing here.
                let (kind, bindings) = self.declaration_parts(declaration, strict)?;
                Some(ForInit::Declare { kind, bindings })
            }
            Some(other) => {
                let expression = other
                    .as_expression()
                    .expect("a for head is a declaration or an expression");
                Some(ForInit::Expr(self.expression(expression, strict)?))
            }
        };

        let test = match node.test.as_ref() {
            Some(test) => Some(self.expression(test, strict)?),
            None => None,
        };
        let update = match node.update.as_ref() {
            Some(update) => Some(self.expression(update, strict)?),
            None => None,
        };

        Ok(Stmt::new(
            span(node.span),
            StmtKind::For {
                init,
                test,
                update,
                body: self.branch(&node.body, strict)?,
            },
        ))
    }

    /// Adapt a `try` and its two or three parts.
    ///
    /// All three are separate fields rather than two shapes, because the grammar allows a `catch`
    /// alone, a `finally` alone and both, and lowering wants to know which of the three it has
    /// rather than to work it out from a tree that has already picked a nesting.
    fn try_statement(
        &self,
        node: &oxc::TryStatement<'_>,
        strict: bool,
    ) -> Result<Stmt, ParseError> {
        let block = self.block(&node.block, strict)?;
        let catch = match node.handler.as_ref() {
            Some(handler) => Some(self.catch(handler, strict)?),
            None => None,
        };
        let finally = match node.finalizer.as_ref() {
            Some(finalizer) => Some(self.block(finalizer, strict)?),
            None => None,
        };
        Ok(Stmt::new(
            span(node.span),
            StmtKind::Try {
                block,
                catch,
                finally,
            },
        ))
    }

    /// Adapt the `catch` clause of a `try`.
    ///
    /// The parameter is optional in the grammar, because `catch {}` is a way of saying that the
    /// value is not wanted, and it is a binding pattern rather than a name when it is present. A
    /// pattern is refused here for the same reason it is refused in a declaration.
    fn catch(&self, handler: &oxc::CatchClause<'_>, strict: bool) -> Result<Catch, ParseError> {
        let param = match handler.param.as_ref() {
            Some(param) => Some(self.binding_name(&param.pattern, strict)?),
            None => None,
        };
        Ok(Catch {
            span: span(handler.span),
            param,
            body: self.block(&handler.body, strict)?,
        })
    }

    /// Adapt a braced block that is part of a larger statement rather than a statement itself.
    fn block(&self, node: &oxc::BlockStatement<'_>, strict: bool) -> Result<Block, ParseError> {
        Ok(Block {
            span: span(node.span),
            body: self.statements(&node.body, strict)?,
        })
    }

    /// Adapt the statement in the arm of an `if`, the body of a loop or the body of a label.
    ///
    /// The grammar allows a single statement without braces there, and it can be a declaration in
    /// exactly one case that is already a syntax error in strict mode, so an erasing statement in
    /// this position would leave the arm with nothing in it. That is a genuine hole rather than a
    /// thing to paper over, so it becomes an empty statement and keeps its span.
    fn branch(
        &self,
        statement: &oxc::Statement<'_>,
        strict: bool,
    ) -> Result<Box<Stmt>, ParseError> {
        // A function declaration is not a statement, and the only reason one can stand here at all
        // is Annex B, which is legacy web compatibility rather than the language proper. node takes
        // `if (c) function f() {}` and `l: function f() {}` in sloppy code and refuses both in
        // strict code. Refusing it by name in both is the honest answer until Annex B is looked at
        // properly, and it is a strict improvement on what was here before, which was a panic in
        // scope analysis, because nothing declared the name and resolving it found nothing.
        if let oxc::Statement::FunctionDeclaration(node) = statement {
            return self.refuse("a function declaration outside a block", node.span);
        }
        let at = statement_span(statement);
        let adapted = self
            .statement(statement, strict)?
            .unwrap_or_else(|| Stmt::new(span(at), StmtKind::Empty));
        Ok(Box::new(adapted))
    }

    /// Adapt a `var`, `let` or `const` declaration.
    fn variable_declaration(
        &self,
        node: &oxc::VariableDeclaration<'_>,
        strict: bool,
    ) -> Result<Stmt, ParseError> {
        let (kind, bindings) = self.declaration_parts(node, strict)?;
        Ok(Stmt::new(
            span(node.span),
            StmtKind::Declare { kind, bindings },
        ))
    }

    /// Adapt the keyword and the declarators of a declaration, without deciding what holds them.
    ///
    /// A declaration is a statement in most places and the head of a `for` in one, and the two
    /// differ only in what wraps this, so this is the part they share.
    fn declaration_parts(
        &self,
        node: &oxc::VariableDeclaration<'_>,
        strict: bool,
    ) -> Result<(DeclKind, Vec<Binding>), ParseError> {
        let kind = match node.kind {
            oxc::VariableDeclarationKind::Var => DeclKind::Var,
            oxc::VariableDeclarationKind::Let => DeclKind::Let,
            oxc::VariableDeclarationKind::Const => DeclKind::Const,
            // `using` and `await using` run a disposal method at the end of the scope, which needs
            // the same machinery as `try` and `finally`. Neither is in M0.
            oxc::VariableDeclarationKind::Using | oxc::VariableDeclarationKind::AwaitUsing => {
                return self.refuse("a using declaration", node.span);
            }
        };

        let mut bindings = Vec::with_capacity(node.declarations.len());
        for declarator in &node.declarations {
            let name = self.binding_name(&declarator.id, strict)?;
            let init = match declarator.init.as_ref() {
                Some(init) => Some(self.expression(init, strict)?),
                None => None,
            };
            bindings.push(Binding {
                span: span(declarator.span),
                name,
                init,
            });
        }

        Ok((kind, bindings))
    }

    /// Pull the single name out of a binding position, refusing destructuring.
    ///
    /// The type annotation hanging off the pattern is not read anywhere in this function, which is
    /// what erasure looks like in practice.
    fn binding_name(
        &self,
        pattern: &oxc::BindingPattern<'_>,
        strict: bool,
    ) -> Result<Ident, ParseError> {
        match pattern {
            oxc::BindingPattern::BindingIdentifier(node) => {
                self.writable(node.name.as_str(), node.span, strict)?;
                Ok(Ident::new(span(node.span), node.name.as_str()))
            }
            oxc::BindingPattern::ObjectPattern(node) => {
                self.refuse("object destructuring", node.span)
            }
            oxc::BindingPattern::ArrayPattern(node) => {
                self.refuse("array destructuring", node.span)
            }
            oxc::BindingPattern::AssignmentPattern(node) => {
                self.refuse("a default value in a binding", node.span)
            }
        }
    }

    /// Adapt a function, or `None` if it has no body and therefore emits nothing.
    fn function(
        &self,
        node: &oxc::Function<'_>,
        enclosing_strict: bool,
    ) -> Result<Option<Func>, ParseError> {
        if node.generator {
            return self.refuse("a generator function", node.span);
        }
        if node.r#async {
            return self.refuse("an async function", node.span);
        }

        let Some(body) = node.body.as_ref() else {
            return Ok(None);
        };

        // Strictness is inherited and then possibly turned on, never off. A function inside strict
        // code is strict whatever its own directives say, which is why the enclosing value is an
        // argument rather than something recomputed from the directives alone.
        let strict = enclosing_strict
            || body
                .directives
                .iter()
                .any(|directive| directive.directive.as_str() == "use strict");

        if let Some(rest) = node.params.rest.as_ref() {
            return self.refuse("a rest parameter", rest.span);
        }

        let mut params = Vec::with_capacity(node.params.items.len());
        for parameter in &node.params.items {
            if let Some(decorator) = parameter.decorators.first() {
                return self.refuse("a parameter decorator", decorator.span);
            }
            // `constructor(private readonly x: number)` declares a field as a side effect of
            // taking an argument, so it is not erasable and must not be dropped silently.
            if parameter.accessibility.is_some() || parameter.readonly || parameter.r#override {
                return self.refuse("a TypeScript parameter property", parameter.span);
            }
            if let Some(initializer) = parameter.initializer.as_ref() {
                return self.refuse("a default parameter value", expression_span(initializer));
            }
            params.push(self.binding_name(&parameter.pattern, strict)?);
        }

        // The name is checked against the function's own strictness rather than the enclosing
        // strictness, which is why this waits until after the directives have been read. A sloppy
        // `function eval() { "use strict"; }` is a `SyntaxError` even though nothing outside it is
        // strict, because the name is part of what the directive turned strict.
        let name = match node.id.as_ref() {
            Some(id) => {
                self.writable(id.name.as_str(), id.span, strict)?;
                Some(Ident::new(span(id.span), id.name.as_str()))
            }
            None => None,
        };

        Ok(Some(Func {
            span: span(node.span),
            name,
            params,
            body: self.statements(&body.statements, strict)?,
            strict,
        }))
    }

    /// Adapt one expression.
    fn expression(
        &self,
        expression: &oxc::Expression<'_>,
        strict: bool,
    ) -> Result<Expr, ParseError> {
        let expression = peel(expression);
        let adapted = match expression {
            oxc::Expression::NumericLiteral(node) => {
                Expr::new(span(node.span), ExprKind::Number(node.value))
            }
            oxc::Expression::StringLiteral(node) => Expr::new(
                span(node.span),
                ExprKind::String(node.value.as_str().to_owned()),
            ),
            oxc::Expression::BooleanLiteral(node) => {
                Expr::new(span(node.span), ExprKind::Boolean(node.value))
            }
            oxc::Expression::NullLiteral(node) => Expr::new(span(node.span), ExprKind::Null),
            oxc::Expression::ThisExpression(node) => Expr::new(span(node.span), ExprKind::This),

            oxc::Expression::Identifier(node) => {
                self.reference(node.name.as_str(), node.span, strict)?;
                Expr::new(
                    span(node.span),
                    ExprKind::Ident(Ident::new(span(node.span), node.name.as_str())),
                )
            }

            oxc::Expression::UnaryExpression(node) => Expr::new(
                span(node.span),
                ExprKind::Unary {
                    op: unary_op(node.operator),
                    operand: Box::new(self.expression(&node.argument, strict)?),
                },
            ),

            oxc::Expression::UpdateExpression(node) => Expr::new(
                span(node.span),
                ExprKind::Update {
                    op: match node.operator {
                        oxc::UpdateOperator::Increment => UpdateOp::Increment,
                        oxc::UpdateOperator::Decrement => UpdateOp::Decrement,
                    },
                    prefix: node.prefix,
                    target: self.simple_target(&node.argument, strict)?,
                },
            ),

            oxc::Expression::BinaryExpression(node) => Expr::new(
                span(node.span),
                ExprKind::Binary {
                    op: binary_op(node.operator),
                    left: Box::new(self.expression(&node.left, strict)?),
                    right: Box::new(self.expression(&node.right, strict)?),
                },
            ),

            oxc::Expression::LogicalExpression(node) => Expr::new(
                span(node.span),
                ExprKind::Logical {
                    op: logical_op(node.operator),
                    left: Box::new(self.expression(&node.left, strict)?),
                    right: Box::new(self.expression(&node.right, strict)?),
                },
            ),

            oxc::Expression::ConditionalExpression(node) => Expr::new(
                span(node.span),
                ExprKind::Conditional {
                    test: Box::new(self.expression(&node.test, strict)?),
                    consequent: Box::new(self.expression(&node.consequent, strict)?),
                    alternate: Box::new(self.expression(&node.alternate, strict)?),
                },
            ),

            oxc::Expression::AssignmentExpression(node) => Expr::new(
                span(node.span),
                ExprKind::Assign {
                    op: assign_op(node.operator),
                    target: self.target(&node.left, strict)?,
                    value: Box::new(self.expression(&node.right, strict)?),
                },
            ),

            oxc::Expression::StaticMemberExpression(node) => self.field(node, strict)?,

            oxc::Expression::ComputedMemberExpression(node) => self.index(node, strict)?,

            oxc::Expression::CallExpression(node) => self.call(node, strict)?,

            oxc::Expression::ObjectExpression(node) => self.object(node, strict)?,

            oxc::Expression::FunctionExpression(node) => {
                // A function expression without a body is a TypeScript overload signature in a
                // position where there is nothing to fall through to, so unlike the declaration
                // case there is no later implementation to carry the code.
                let Some(function) = self.function(node, strict)? else {
                    return self.refuse("a function expression with no body", node.span);
                };
                Expr::new(span(node.span), ExprKind::Function(Box::new(function)))
            }

            other => return self.refuse(expression_name(other), expression_span(other)),
        };

        Ok(adapted)
    }

    /// Adapt an object literal whose property names are all known at compile time.
    ///
    /// That is the subset the object model can build today. Everything else in the grammar is
    /// refused by name rather than approximated, and each refusal says which construct it was, so a
    /// program that uses spread gets told about spread instead of being told that object literals
    /// are unsupported.
    ///
    /// Shorthand needs no work here. `{x}` arrives with an identifier key and an identifier value,
    /// which is exactly the shape `{x: x}` arrives in, and the two lower identically because they
    /// mean the same thing.
    fn object(&self, node: &oxc::ObjectExpression<'_>, strict: bool) -> Result<Expr, ParseError> {
        use oxc_span::GetSpan;

        let mut properties = Vec::with_capacity(node.properties.len());
        for property in &node.properties {
            let oxc::ObjectPropertyKind::ObjectProperty(property) = property else {
                return self.refuse("spread in an object literal", property.span());
            };
            let kind = match property.kind {
                oxc::PropertyKind::Init => PropertyKind::Value,
                oxc::PropertyKind::Get => PropertyKind::Getter,
                oxc::PropertyKind::Set => PropertyKind::Setter,
            };
            // A getter and a setter are both marked as methods here, because they are: the grammar
            // that produces them is `MethodDefinition`. The refusal is about the third thing that
            // grammar produces, which is `{x() {}}`, so it asks about the kind as well.
            if property.method && kind == PropertyKind::Value {
                return self.refuse("a method in an object literal", property.span);
            }
            if property.computed {
                return self.refuse("a computed property name", property.key.span());
            }
            let name = match &property.key {
                oxc::PropertyKey::StaticIdentifier(key) => {
                    Ident::new(span(key.span), key.name.as_str())
                }
                // A string key is a name known at compile time just as much as an identifier is,
                // and `{'a-b': 1}` is ordinary code rather than an edge case.
                oxc::PropertyKey::StringLiteral(key) => {
                    Ident::new(span(key.span), key.value.as_str())
                }
                // A numeric key is an array index by another name, and an array index is not a
                // string key. Until there are elements this is refused rather than stored under the
                // text of the number, which would put it in the wrong place and enumerate it in the
                // wrong order.
                oxc::PropertyKey::NumericLiteral(key) => {
                    return self.refuse("a numeric property name", key.span);
                }
                other => return self.refuse("a property name of this kind", other.span()),
            };
            properties.push(Property {
                span: span(property.span),
                name,
                kind,
                value: self.expression(&property.value, strict)?,
            });
        }
        Ok(Expr::new(span(node.span), ExprKind::Object { properties }))
    }

    /// Adapt a property read with a name known at compile time, as in `o.x`.
    ///
    /// This is the shape an inline cache can do something with, which is why it is a different
    /// node from the computed one rather than the same node with a string key.
    fn field(
        &self,
        node: &oxc::StaticMemberExpression<'_>,
        strict: bool,
    ) -> Result<Expr, ParseError> {
        if node.optional {
            return self.refuse("optional chaining", node.span);
        }
        Ok(Expr::new(
            span(node.span),
            ExprKind::Field {
                object: Box::new(self.expression(&node.object, strict)?),
                name: Ident::new(span(node.property.span), node.property.name.as_str()),
            },
        ))
    }

    /// Adapt a property read with a computed key, as in `o[k]`.
    fn index(
        &self,
        node: &oxc::ComputedMemberExpression<'_>,
        strict: bool,
    ) -> Result<Expr, ParseError> {
        if node.optional {
            return self.refuse("optional chaining", node.span);
        }
        Ok(Expr::new(
            span(node.span),
            ExprKind::Index {
                object: Box::new(self.expression(&node.object, strict)?),
                index: Box::new(self.expression(&node.expression, strict)?),
            },
        ))
    }

    /// Adapt a call.
    ///
    /// The callee keeps whatever shape it had. A `Field` callee is a method call and the object is
    /// the receiver the callee will see as `this`, so flattening it into a plain function value
    /// here would lose that and there would be no way to get it back at lowering.
    fn call(&self, node: &oxc::CallExpression<'_>, strict: bool) -> Result<Expr, ParseError> {
        if node.optional {
            return self.refuse("an optional call", node.span);
        }

        let mut arguments = Vec::with_capacity(node.arguments.len());
        for argument in &node.arguments {
            // A spread argument makes the argument count a run time value, which changes how the
            // call frame is set up rather than adding one more expression to evaluate.
            if let oxc::Argument::SpreadElement(spread) = argument {
                return self.refuse("a spread argument", spread.span);
            }
            let Some(expression) = argument.as_expression() else {
                return self.refuse("this argument form", node.span);
            };
            arguments.push(self.expression(expression, strict)?);
        }

        Ok(Expr::new(
            span(node.span),
            ExprKind::Call {
                callee: Box::new(self.expression(&node.callee, strict)?),
                arguments,
            },
        ))
    }

    /// Adapt the left side of an assignment.
    fn target(
        &self,
        target: &oxc::AssignmentTarget<'_>,
        strict: bool,
    ) -> Result<Target, ParseError> {
        match target {
            oxc::AssignmentTarget::ArrayAssignmentTarget(node) => {
                self.refuse("array destructuring assignment", node.span)
            }
            oxc::AssignmentTarget::ObjectAssignmentTarget(node) => {
                self.refuse("object destructuring assignment", node.span)
            }
            other => {
                let Some(simple) = other.as_simple_assignment_target() else {
                    return self.refuse("this assignment target", assignment_target_span(other));
                };
                self.simple_target(simple, strict)
            }
        }
    }

    /// Adapt an assignment target that is a name or a property, which is all the M0 subset allows.
    fn simple_target(
        &self,
        target: &oxc::SimpleAssignmentTarget<'_>,
        strict: bool,
    ) -> Result<Target, ParseError> {
        match target {
            oxc::SimpleAssignmentTarget::AssignmentTargetIdentifier(node) => {
                self.writable(node.name.as_str(), node.span, strict)?;
                Ok(Target {
                    span: span(node.span),
                    kind: TargetKind::Ident(Ident::new(span(node.span), node.name.as_str())),
                })
            }

            oxc::SimpleAssignmentTarget::StaticMemberExpression(node) => Ok(Target {
                span: span(node.span),
                kind: TargetKind::Field {
                    object: Box::new(self.expression(&node.object, strict)?),
                    name: Ident::new(span(node.property.span), node.property.name.as_str()),
                },
            }),

            oxc::SimpleAssignmentTarget::ComputedMemberExpression(node) => Ok(Target {
                span: span(node.span),
                kind: TargetKind::Index {
                    object: Box::new(self.expression(&node.object, strict)?),
                    index: Box::new(self.expression(&node.expression, strict)?),
                },
            }),

            // `(x as T) = 1` and `x! = 1` are assignments to `x`, so the wrapper erases here the
            // same way it does in expression position.
            oxc::SimpleAssignmentTarget::TSAsExpression(node) => {
                self.target_from_expression(&node.expression, strict)
            }
            oxc::SimpleAssignmentTarget::TSSatisfiesExpression(node) => {
                self.target_from_expression(&node.expression, strict)
            }
            oxc::SimpleAssignmentTarget::TSTypeAssertion(node) => {
                self.target_from_expression(&node.expression, strict)
            }
            oxc::SimpleAssignmentTarget::TSNonNullExpression(node) => {
                self.target_from_expression(&node.expression, strict)
            }

            oxc::SimpleAssignmentTarget::PrivateFieldExpression(node) => {
                self.refuse("a private field", node.span)
            }
        }
    }

    /// Recover an assignment target from the expression left behind by erasing a TypeScript
    /// wrapper.
    ///
    /// The wrapper is gone, so the grammar's guarantee that the thing underneath is assignable is
    /// gone with it and has to be re-established. Only three shapes can appear here, and anything
    /// else was already a syntax error before erasure.
    fn target_from_expression(
        &self,
        expression: &oxc::Expression<'_>,
        strict: bool,
    ) -> Result<Target, ParseError> {
        let adapted = self.expression(expression, strict)?;
        let kind = match adapted.kind {
            ExprKind::Ident(name) => TargetKind::Ident(name),
            ExprKind::Field { object, name } => TargetKind::Field { object, name },
            ExprKind::Index { object, index } => TargetKind::Index { object, index },
            _ => return self.refuse("this assignment target", expression_span(expression)),
        };
        Ok(Target {
            span: adapted.span,
            kind,
        })
    }
}

/// Convert an oxc span to ours. The representation is the same, the type is not, and that is the
/// point.
const fn span(from: oxc::Span) -> Span {
    Span::new(from.start, from.end)
}

/// Strip the wrappers that evaluate to whatever is inside them.
///
/// `x as T`, `x satisfies T`, `<T>x`, `x!` and `f<T>` all produce `x`, which is the whole of what
/// TypeScript erasure means for an expression and the reason a `.ts` file can reach the
/// interpreter without a type checker having run. Parentheses go here too, since they group rather
/// than compute and oxc only keeps them so that a formatter can.
///
/// A loop rather than recursion, and one list rather than five match arms, because this is exactly
/// the list that grows when TypeScript adds syntax and it should only exist in one place.
fn peel<'e, 'a>(mut expression: &'e oxc::Expression<'a>) -> &'e oxc::Expression<'a> {
    loop {
        expression = match expression {
            oxc::Expression::ParenthesizedExpression(node) => &node.expression,
            oxc::Expression::TSAsExpression(node) => &node.expression,
            oxc::Expression::TSSatisfiesExpression(node) => &node.expression,
            oxc::Expression::TSTypeAssertion(node) => &node.expression,
            oxc::Expression::TSNonNullExpression(node) => &node.expression,
            oxc::Expression::TSInstantiationExpression(node) => &node.expression,
            other => return other,
        };
    }
}

fn unary_op(operator: oxc::UnaryOperator) -> UnaryOp {
    match operator {
        oxc::UnaryOperator::UnaryPlus => UnaryOp::Plus,
        oxc::UnaryOperator::UnaryNegation => UnaryOp::Minus,
        oxc::UnaryOperator::LogicalNot => UnaryOp::Not,
        oxc::UnaryOperator::BitwiseNot => UnaryOp::BitNot,
        oxc::UnaryOperator::Typeof => UnaryOp::Typeof,
        oxc::UnaryOperator::Void => UnaryOp::Void,
        oxc::UnaryOperator::Delete => UnaryOp::Delete,
    }
}

fn binary_op(operator: oxc::BinaryOperator) -> BinaryOp {
    match operator {
        oxc::BinaryOperator::Equality => BinaryOp::Equal,
        oxc::BinaryOperator::Inequality => BinaryOp::NotEqual,
        oxc::BinaryOperator::StrictEquality => BinaryOp::StrictEqual,
        oxc::BinaryOperator::StrictInequality => BinaryOp::StrictNotEqual,
        oxc::BinaryOperator::LessThan => BinaryOp::Less,
        oxc::BinaryOperator::LessEqualThan => BinaryOp::LessEqual,
        oxc::BinaryOperator::GreaterThan => BinaryOp::Greater,
        oxc::BinaryOperator::GreaterEqualThan => BinaryOp::GreaterEqual,
        oxc::BinaryOperator::Addition => BinaryOp::Add,
        oxc::BinaryOperator::Subtraction => BinaryOp::Sub,
        oxc::BinaryOperator::Multiplication => BinaryOp::Mul,
        oxc::BinaryOperator::Division => BinaryOp::Div,
        oxc::BinaryOperator::Remainder => BinaryOp::Rem,
        oxc::BinaryOperator::Exponential => BinaryOp::Pow,
        oxc::BinaryOperator::ShiftLeft => BinaryOp::Shl,
        oxc::BinaryOperator::ShiftRight => BinaryOp::Shr,
        oxc::BinaryOperator::ShiftRightZeroFill => BinaryOp::UnsignedShr,
        oxc::BinaryOperator::BitwiseOR => BinaryOp::BitOr,
        oxc::BinaryOperator::BitwiseXOR => BinaryOp::BitXor,
        oxc::BinaryOperator::BitwiseAnd => BinaryOp::BitAnd,
        oxc::BinaryOperator::In => BinaryOp::In,
        oxc::BinaryOperator::Instanceof => BinaryOp::Instanceof,
    }
}

fn logical_op(operator: oxc::LogicalOperator) -> LogicalOp {
    match operator {
        oxc::LogicalOperator::And => LogicalOp::And,
        oxc::LogicalOperator::Or => LogicalOp::Or,
        oxc::LogicalOperator::Coalesce => LogicalOp::Coalesce,
    }
}

fn assign_op(operator: oxc::AssignmentOperator) -> AssignOp {
    match operator {
        oxc::AssignmentOperator::Assign => AssignOp::Assign,
        oxc::AssignmentOperator::Addition => AssignOp::Binary(BinaryOp::Add),
        oxc::AssignmentOperator::Subtraction => AssignOp::Binary(BinaryOp::Sub),
        oxc::AssignmentOperator::Multiplication => AssignOp::Binary(BinaryOp::Mul),
        oxc::AssignmentOperator::Division => AssignOp::Binary(BinaryOp::Div),
        oxc::AssignmentOperator::Remainder => AssignOp::Binary(BinaryOp::Rem),
        oxc::AssignmentOperator::Exponential => AssignOp::Binary(BinaryOp::Pow),
        oxc::AssignmentOperator::ShiftLeft => AssignOp::Binary(BinaryOp::Shl),
        oxc::AssignmentOperator::ShiftRight => AssignOp::Binary(BinaryOp::Shr),
        oxc::AssignmentOperator::ShiftRightZeroFill => AssignOp::Binary(BinaryOp::UnsignedShr),
        oxc::AssignmentOperator::BitwiseOR => AssignOp::Binary(BinaryOp::BitOr),
        oxc::AssignmentOperator::BitwiseXOR => AssignOp::Binary(BinaryOp::BitXor),
        oxc::AssignmentOperator::BitwiseAnd => AssignOp::Binary(BinaryOp::BitAnd),
        oxc::AssignmentOperator::LogicalOr => AssignOp::Logical(LogicalOp::Or),
        oxc::AssignmentOperator::LogicalAnd => AssignOp::Logical(LogicalOp::And),
        oxc::AssignmentOperator::LogicalNullish => AssignOp::Logical(LogicalOp::Coalesce),
    }
}

/// Name a statement we do not handle, in the words a JavaScript programmer would use.
///
/// These strings end up in front of a user, so they say `a for loop` and not
/// `ForStatement`. The list is the to do list for M1 and M2 read from the other end.
fn statement_name(statement: &oxc::Statement<'_>) -> &'static str {
    match statement {
        oxc::Statement::DebuggerStatement(_) => "debugger",
        oxc::Statement::ForInStatement(_) => "a for in loop",
        oxc::Statement::ForOfStatement(_) => "a for of loop",
        oxc::Statement::ThrowStatement(_) => "throw",
        oxc::Statement::TryStatement(_) => "try",
        oxc::Statement::WithStatement(_) => "with",
        oxc::Statement::ClassDeclaration(_) => "a class",
        oxc::Statement::ImportDeclaration(_) => "an import",
        oxc::Statement::ExportAllDeclaration(_)
        | oxc::Statement::ExportDefaultDeclaration(_)
        | oxc::Statement::ExportDeclaration(_)
        | oxc::Statement::ExportNamedDeclaration(_)
        | oxc::Statement::ExportFromDeclaration(_) => "an export",
        _ => "this statement",
    }
}

/// Name an expression we do not handle.
fn expression_name(expression: &oxc::Expression<'_>) -> &'static str {
    match expression {
        oxc::Expression::ArrayExpression(_) => "an array literal",
        oxc::Expression::ObjectExpression(_) => "an object literal",
        oxc::Expression::ArrowFunctionExpression(_) => "an arrow function",
        oxc::Expression::AwaitExpression(_) => "await",
        oxc::Expression::YieldExpression(_) => "yield",
        oxc::Expression::NewExpression(_) => "new",
        oxc::Expression::ClassExpression(_) => "a class expression",
        oxc::Expression::TemplateLiteral(_) => "a template literal",
        oxc::Expression::TaggedTemplateExpression(_) => "a tagged template",
        oxc::Expression::RegExpLiteral(_) => "a regular expression",
        oxc::Expression::BigIntLiteral(_) => "a bigint literal",
        oxc::Expression::SequenceExpression(_) => "a comma expression",
        oxc::Expression::ChainExpression(_) => "optional chaining",
        oxc::Expression::ImportExpression(_) => "a dynamic import",
        oxc::Expression::ImportMeta(_) => "import.meta",
        oxc::Expression::NewTarget(_) => "new.target",
        oxc::Expression::Super(_) => "super",
        oxc::Expression::PrivateFieldExpression(_) | oxc::Expression::PrivateInExpression(_) => {
            "a private field"
        }
        _ => "this expression",
    }
}

/// The span of a statement, however deeply oxc buries it.
///
/// oxc has a `GetSpan` trait that does this, but reaching for it would put a second oxc name in
/// every call site, and the whole argument for this file is that the oxc surface we depend on is
/// small enough to enumerate.
fn statement_span(statement: &oxc::Statement<'_>) -> oxc::Span {
    use oxc_span::GetSpan;
    statement.span()
}

/// The span of an expression.
fn expression_span(expression: &oxc::Expression<'_>) -> oxc::Span {
    use oxc_span::GetSpan;
    expression.span()
}

/// The span of an assignment target.
fn assignment_target_span(target: &oxc::AssignmentTarget<'_>) -> oxc::Span {
    use oxc_span::GetSpan;
    target.span()
}

/// Turn a byte offset into a one based line and column.
///
/// Only ever called on the error path, so a scan from the start of the file is fine and a line
/// table would be a cache with nothing to serve. The column counts characters rather than bytes,
/// because a column that lands mid character in a file with an accent in it is worse than useless.
pub(crate) fn line_and_column(source: &str, offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(source.len());
    let before = &source[..offset];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let start_of_line = before.rfind('\n').map_or(0, |index| index + 1);
    let column = source[start_of_line..offset].chars().count() + 1;
    #[allow(clippy::cast_possible_truncation)]
    (line as u32, column as u32)
}

#[cfg(test)]
mod tests {
    use crate::ast::{
        AssignOp, BinaryOp, DeclKind, Expr, ExprKind, LogicalOp, PropertyKind, Stmt, StmtKind,
        TargetKind, UnaryOp, UpdateOp,
    };
    use crate::{ParseError, parse};

    /// Parse and adapt, expecting it to work.
    fn tree(path: &str, source: &str) -> Vec<Stmt> {
        parse(path, source)
            .expect("should parse and adapt")
            .ast
            .body
    }

    /// Parse and adapt one statement, expecting it to work.
    fn one(path: &str, source: &str) -> Stmt {
        let mut body = tree(path, source);
        assert_eq!(body.len(), 1, "expected exactly one statement");
        body.pop().expect("checked above")
    }

    /// Parse and adapt, expecting a refusal, and report what was refused.
    fn refused(path: &str, source: &str) -> &'static str {
        let error = parse(path, source).expect_err("should be refused");
        let ParseError::Unsupported { construct, .. } = error else {
            panic!("expected an unsupported construct, got {error:?}");
        };
        construct
    }

    /// Parse and adapt, expecting an early error, and report where it was and what it said.
    fn early(path: &str, source: &str) -> (u32, u32, String) {
        let error = parse(path, source).expect_err("should be refused");
        let ParseError::EarlyError {
            line,
            column,
            message,
            ..
        } = error
        else {
            panic!("expected an early error, got {error:?}");
        };
        (line, column, message)
    }

    /// The initialiser of the first binding of a single declaration.
    fn init(source: &str) -> Expr {
        let StmtKind::Declare { bindings, .. } = one("t.js", source).kind else {
            panic!("expected a declaration");
        };
        bindings
            .into_iter()
            .next()
            .expect("one binding")
            .init
            .expect("an initialiser")
    }

    #[test]
    fn the_three_declaration_keywords_stay_apart() {
        for (source, expected) in [
            ("var x = 1;", DeclKind::Var),
            ("let x = 1;", DeclKind::Let),
            ("const x = 1;", DeclKind::Const),
        ] {
            let StmtKind::Declare { kind, .. } = one("t.js", source).kind else {
                panic!("expected a declaration for {source}");
            };
            assert_eq!(kind, expected, "for {source}");
        }
    }

    #[test]
    fn one_declaration_can_bind_several_names() {
        let StmtKind::Declare { bindings, .. } = one("t.js", "let a = 1, b, c = 3;").kind else {
            panic!("expected a declaration");
        };
        let names: Vec<&str> = bindings.iter().map(|b| b.name.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert!(bindings[1].init.is_none(), "b has no initialiser");
    }

    #[test]
    fn operators_survive_the_crossing() {
        let ExprKind::Binary { op, .. } = init("let x = 1 + 2;").kind else {
            panic!("expected a binary expression");
        };
        assert_eq!(op, BinaryOp::Add);

        let ExprKind::Logical { op, .. } = init("let x = a ?? b;").kind else {
            panic!("expected a logical expression");
        };
        assert_eq!(op, LogicalOp::Coalesce);

        let ExprKind::Unary { op, .. } = init("let x = typeof a;").kind else {
            panic!("expected a unary expression");
        };
        assert_eq!(op, UnaryOp::Typeof);
    }

    #[test]
    fn a_compound_assignment_carries_the_operation_it_applies() {
        let StmtKind::Expr(expression) = one("t.js", "x >>>= 2;").kind else {
            panic!("expected an expression statement");
        };
        let ExprKind::Assign { op, target, .. } = expression.kind else {
            panic!("expected an assignment");
        };
        assert_eq!(op, AssignOp::Binary(BinaryOp::UnsignedShr));
        assert!(matches!(target.kind, TargetKind::Ident(_)));
    }

    #[test]
    fn a_short_circuiting_assignment_is_not_a_binary_one() {
        // `a ||= b` does not always store, so lowering has to emit a branch. If these two ever
        // collapse into one representation that distinction is the thing that gets lost.
        let StmtKind::Expr(expression) = one("t.js", "a ||= b;").kind else {
            panic!("expected an expression statement");
        };
        let ExprKind::Assign { op, .. } = expression.kind else {
            panic!("expected an assignment");
        };
        assert_eq!(op, AssignOp::Logical(LogicalOp::Or));
    }

    #[test]
    fn the_three_assignable_shapes_come_through_as_targets() {
        for (source, check) in [
            ("x = 1;", "ident"),
            ("o.x = 1;", "field"),
            ("o[k] = 1;", "index"),
        ] {
            let StmtKind::Expr(expression) = one("t.js", source).kind else {
                panic!("expected an expression statement for {source}");
            };
            let ExprKind::Assign { target, .. } = expression.kind else {
                panic!("expected an assignment for {source}");
            };
            let actual = match target.kind {
                TargetKind::Ident(_) => "ident",
                TargetKind::Field { .. } => "field",
                TargetKind::Index { .. } => "index",
            };
            assert_eq!(actual, check, "for {source}");
        }
    }

    #[test]
    fn a_postfix_increment_remembers_that_it_is_postfix() {
        let StmtKind::Expr(expression) = one("t.js", "i++;").kind else {
            panic!("expected an expression statement");
        };
        let ExprKind::Update { op, prefix, .. } = expression.kind else {
            panic!("expected an update expression");
        };
        assert_eq!(op, UpdateOp::Increment);
        assert!(!prefix, "i++ is postfix");
    }

    #[test]
    fn a_method_call_keeps_the_receiver_visible() {
        // Flattening `console.log(x)` into a call to a resolved function would lose the object,
        // and the object is the `this` the callee gets.
        let StmtKind::Expr(expression) = one("t.js", "console.log(x);").kind else {
            panic!("expected an expression statement");
        };
        let ExprKind::Call { callee, arguments } = expression.kind else {
            panic!("expected a call");
        };
        assert_eq!(arguments.len(), 1);
        let ExprKind::Field { object, name } = callee.kind else {
            panic!("expected a field callee");
        };
        assert_eq!(name.name, "log");
        assert!(matches!(object.kind, ExprKind::Ident(_)));
    }

    #[test]
    fn spans_point_at_the_source_they_came_from() {
        let source = "const answer = 42;";
        let StmtKind::Declare { bindings, .. } = one("t.js", source).kind else {
            panic!("expected a declaration");
        };
        let name = &bindings[0].name.span;
        assert_eq!(
            &source[name.start as usize..name.end as usize],
            "answer",
            "the name span has to slice the name back out"
        );
        let init = bindings[0].init.as_ref().expect("an initialiser").span;
        assert_eq!(&source[init.start as usize..init.end as usize], "42");
    }

    #[test]
    fn types_erase_and_leave_no_statement_behind() {
        // Four statements written, one statement of running code. Everything else was a type.
        let body = tree(
            "t.ts",
            "interface Shape { area(): number }\ntype Id = string;\ndeclare const version: string;\nlet count: number = 0;",
        );
        assert_eq!(body.len(), 1);
        let StmtKind::Declare { bindings, .. } = &body[0].kind else {
            panic!("expected a declaration");
        };
        assert_eq!(bindings[0].name.name, "count");
    }

    #[test]
    fn the_erasable_expression_wrappers_unwrap_to_what_is_underneath() {
        for source in [
            "let x = 1 as number;",
            "let x = 1 satisfies number;",
            "let x = (1)!;",
        ] {
            let StmtKind::Declare { bindings, .. } = one("t.ts", source).kind else {
                panic!("expected a declaration for {source}");
            };
            let init = bindings[0].init.as_ref().expect("an initialiser");
            assert_eq!(init.kind, ExprKind::Number(1.0), "for {source}");
        }
    }

    #[test]
    fn an_overload_signature_erases_and_the_implementation_stays() {
        let body = tree(
            "t.ts",
            "function pick(a: number): number;\nfunction pick(a: number): number { return a; }",
        );
        assert_eq!(body.len(), 1);
        let StmtKind::Function(function) = &body[0].kind else {
            panic!("expected a function");
        };
        assert_eq!(function.name.as_ref().expect("a name").name, "pick");
        assert_eq!(function.params.len(), 1);
        assert_eq!(function.body.len(), 1);
    }

    #[test]
    fn typescript_that_emits_code_is_refused_and_not_erased() {
        // The difference between these and an interface is that a reference to one of them means
        // something at run time, so dropping it would break code that looks correct.
        assert_eq!(refused("t.ts", "enum Color { Red }"), "a TypeScript enum");
        assert_eq!(
            refused("t.ts", "namespace N { export const x = 1; }"),
            "a TypeScript namespace"
        );
        assert_eq!(
            refused("t.ts", "class C { constructor(private x: number) {} }"),
            "a class"
        );
    }

    #[test]
    fn an_object_literal_keeps_its_properties_in_source_order() {
        let Expr {
            kind: ExprKind::Object { properties },
            ..
        } = init("let o = { b: 1, a: 2 };")
        else {
            panic!("expected an object literal");
        };
        let names: Vec<&str> = properties
            .iter()
            .map(|property| property.name.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["b", "a"],
            "source order is enumeration order, so the adapter must not sort"
        );
    }

    #[test]
    fn a_string_key_and_an_identifier_key_arrive_the_same_way() {
        // `{'a-b': 1}` has a name that is not an identifier and is still a name known at compile
        // time, which is the only distinction the object model cares about.
        for source in ["let o = { 'a-b': 1 };", "let o = { \"a-b\": 1 };"] {
            let Expr {
                kind: ExprKind::Object { properties },
                ..
            } = init(source)
            else {
                panic!("expected an object literal");
            };
            assert_eq!(properties[0].name.name, "a-b");
        }
    }

    #[test]
    fn shorthand_is_the_same_node_as_writing_the_name_twice() {
        // Nothing in the adapter handles shorthand, and this test is what says that is deliberate
        // rather than an oversight.
        let ExprKind::Object {
            properties: shorthand,
        } = init("let o = { x };").kind
        else {
            panic!("expected an object literal");
        };
        let ExprKind::Object { properties: long } = init("let o = { x: x };").kind else {
            panic!("expected an object literal");
        };
        assert_eq!(shorthand[0].name.name, long[0].name.name);
        // The spans differ because the two were written differently, and everything else about the
        // value is the same node reading the same variable.
        let (ExprKind::Ident(short), ExprKind::Ident(long)) =
            (&shorthand[0].value.kind, &long[0].value.kind)
        else {
            panic!("expected the value of each to be a name being read");
        };
        assert_eq!(short.name, long.name);
    }

    #[test]
    fn the_two_halves_of_an_accessor_arrive_marked_as_different_things_from_a_plain_value() {
        let ExprKind::Object { properties } =
            init("let o = { get a() { return 1; }, set a(v) {}, b: 2 };").kind
        else {
            panic!("expected an object literal");
        };
        // Three entries and not two. Joining the halves of `a` into one property is the object
        // model's job at run time, and doing it here would lose the order the two were written in.
        let kinds: Vec<_> = properties.iter().map(|property| property.kind).collect();
        assert_eq!(
            kinds,
            [
                PropertyKind::Getter,
                PropertyKind::Setter,
                PropertyKind::Value
            ]
        );
        assert_eq!(properties[0].name.name, properties[1].name.name);
    }

    #[test]
    fn every_part_of_an_object_literal_we_cannot_build_yet_is_refused_by_its_own_name() {
        // One message per construct rather than one for object literals as a whole, because a
        // program that used a getter should be told about the getter.
        assert_eq!(
            refused("t.js", "let o = { ...rest };"),
            "spread in an object literal"
        );
        assert_eq!(
            refused("t.js", "let o = { m() {} };"),
            "a method in an object literal"
        );
        assert_eq!(
            refused("t.js", "let o = { [k]: 1 };"),
            "a computed property name"
        );
        // A numeric key is an array index by another name, and there are no elements yet.
        assert_eq!(
            refused("t.js", "let o = { 1: 'a' };"),
            "a numeric property name"
        );
    }

    #[test]
    fn what_m0_does_not_cover_is_refused_by_name() {
        // This list is the M1 work list read backwards, and every line of it should disappear.
        assert_eq!(refused("t.js", "for (const x in o) {}"), "a for in loop");
        assert_eq!(refused("t.js", "for (const x of xs) {}"), "a for of loop");
        assert_eq!(refused("t.js", "class C {}"), "a class");
        assert_eq!(refused("t.js", "let x = [1, 2];"), "an array literal");
        assert_eq!(refused("t.js", "let f = () => 1;"), "an arrow function");
        assert_eq!(refused("t.js", "let x = `hi ${y}`;"), "a template literal");
        assert_eq!(refused("t.js", "let x = /re/;"), "a regular expression");
        assert_eq!(refused("t.js", "let x = 1n;"), "a bigint literal");
        assert_eq!(refused("t.js", "let x = a?.b;"), "optional chaining");
        assert_eq!(refused("t.js", "f(...args);"), "a spread argument");
        assert_eq!(refused("t.js", "let [a] = xs;"), "array destructuring");
        assert_eq!(refused("t.js", "let { a } = o;"), "object destructuring");
        assert_eq!(
            refused("t.js", "function f(a = 1) {}"),
            "a default parameter value"
        );
        assert_eq!(
            refused("t.js", "function f(...rest) {}"),
            "a rest parameter"
        );
        assert_eq!(refused("t.js", "function* g() {}"), "a generator function");
        assert_eq!(
            refused("t.js", "async function f() {}"),
            "an async function"
        );
    }

    #[test]
    fn a_try_keeps_its_three_parts_apart() {
        let StmtKind::Try {
            block,
            catch,
            finally,
        } = one("t.js", "try { f(); } catch (e) { g(); }").kind
        else {
            panic!("expected a try");
        };
        assert_eq!(block.body.len(), 1);
        let catch = catch.expect("there is a handler");
        assert_eq!(
            catch.param.expect("there is a parameter").name.as_str(),
            "e"
        );
        assert_eq!(catch.body.body.len(), 1);
        assert!(finally.is_none());
    }

    #[test]
    fn a_catch_can_have_no_parameter() {
        let StmtKind::Try { catch, .. } = one("t.js", "try { f(); } catch { g(); }").kind else {
            panic!("expected a try");
        };
        assert!(catch.expect("there is a handler").param.is_none());
    }

    #[test]
    fn a_try_can_have_a_finally_with_or_without_a_catch() {
        // Both shapes are legal and they lower differently, so the tree has to keep them apart
        // rather than inventing a catch that rethrows for the one that does not have one.
        let StmtKind::Try { catch, finally, .. } =
            one("t.js", "try { f(); } finally { g(); }").kind
        else {
            panic!("expected a try");
        };
        assert!(catch.is_none());
        assert_eq!(finally.expect("there is a finally").body.len(), 1);

        let StmtKind::Try { catch, finally, .. } =
            one("t.js", "try { f(); } catch (e) {} finally { g(); }").kind
        else {
            panic!("expected a try");
        };
        assert!(catch.is_some());
        assert!(finally.is_some());
    }

    #[test]
    fn a_destructuring_catch_parameter_is_refused_like_any_other_pattern() {
        assert_eq!(
            refused("t.js", "try { f(); } catch ({ message }) {}"),
            "object destructuring"
        );
    }

    #[test]
    fn a_module_is_strict_without_saying_so_and_a_script_is_not() {
        assert!(
            parse("t.mjs", "let x = 1;")
                .expect("should parse")
                .ast
                .strict,
            "an ES module is always strict"
        );
        assert!(
            !parse("t.js", "let x = 1;")
                .expect("should parse")
                .ast
                .strict,
            "a plain script is not strict unless it says so"
        );
        assert!(
            parse("t.js", "'use strict';\nlet x = 1;")
                .expect("should parse")
                .ast
                .strict,
            "the directive turns it on"
        );
    }

    #[test]
    fn strictness_is_inherited_by_nested_functions() {
        // A function inside strict code is strict whatever it says, which is why the enclosing
        // value is threaded through the adapter rather than recomputed from directives.
        let body = tree("t.mjs", "function inner() { return 1; }");
        let StmtKind::Function(function) = &body[0].kind else {
            panic!("expected a function");
        };
        assert!(function.strict, "inherited from the module");

        let body = tree("t.js", "function inner() { return 1; }");
        let StmtKind::Function(function) = &body[0].kind else {
            panic!("expected a function");
        };
        assert!(!function.strict, "a sloppy script leaves it sloppy");

        let body = tree("t.js", "function inner() { 'use strict'; return 1; }");
        let StmtKind::Function(function) = &body[0].kind else {
            panic!("expected a function");
        };
        assert!(function.strict, "its own directive turns it on");
    }

    #[test]
    fn the_nine_strict_reserved_words_are_not_identifiers_in_strict_code() {
        // These are the words ECMAScript reserved for a language nobody ended up writing. All nine
        // were run through node before this test was written and all nine give the same message,
        // which is the message asserted here because somebody's expectations depend on it.
        for word in [
            "implements",
            "interface",
            "let",
            "package",
            "private",
            "protected",
            "public",
            "static",
            "yield",
        ] {
            let (_, _, message) = early("t.js", &format!("'use strict'; var {word} = 1;"));
            assert_eq!(
                message, "Unexpected strict mode reserved word",
                "for {word}"
            );

            parse("t.js", &format!("var {word} = 1;"))
                .unwrap_or_else(|error| panic!("{word} is a fine name in sloppy code: {error}"));
        }
    }

    #[test]
    fn a_strict_reserved_word_is_refused_wherever_a_name_can_go() {
        // Reading one is enough. There is nothing to write and nothing to bind in `public;`, and
        // it is still a `SyntaxError`, because the word is not an identifier at all.
        for source in [
            "'use strict'; public;",
            "'use strict'; public = 1;",
            "'use strict'; var public = 1;",
            "'use strict'; function public() {}",
            "'use strict'; function f(public) {}",
            "'use strict'; try { f(); } catch (public) {}",
            "'use strict'; public++;",
            "'use strict'; f(public);",
        ] {
            let (_, _, message) = early("t.js", source);
            assert_eq!(
                message, "Unexpected strict mode reserved word",
                "for {source}"
            );
        }
    }

    #[test]
    fn eval_and_arguments_are_refused_where_a_name_moves_and_nowhere_else() {
        // The rule is narrower than the reserved word rule and the two are worth keeping apart.
        // Strict mode stops a program moving these two names so that a reader can tell what they
        // refer to, so binding one or writing to one is an error and reading one is not.
        for name in ["eval", "arguments"] {
            for source in [
                format!("'use strict'; var {name} = 1;"),
                format!("'use strict'; {name} = 42;"),
                format!("'use strict'; {name} += 1;"),
                format!("'use strict'; {name}++;"),
                format!("'use strict'; function {name}() {{}}"),
                format!("'use strict'; function f({name}) {{}}"),
                format!("'use strict'; try {{ f(); }} catch ({name}) {{}}"),
            ] {
                let (_, _, message) = early("t.js", &source);
                assert_eq!(
                    message, "Unexpected eval or arguments in strict mode",
                    "for {source}"
                );
            }

            // Reading is allowed and stays a runtime question. Node answers `x = arguments;` with
            // a `ReferenceError` when it runs, not a `SyntaxError` before it does.
            parse("t.js", &format!("'use strict'; var x = {name};"))
                .unwrap_or_else(|error| panic!("reading {name} is legal: {error}"));

            parse("t.js", &format!("var {name} = 1;"))
                .unwrap_or_else(|error| panic!("{name} is a fine name in sloppy code: {error}"));
        }
    }

    #[test]
    fn a_property_named_after_a_reserved_word_is_left_alone() {
        // A property is not a name in the sense the rule cares about, so the two checks must never
        // see one. They cannot, because a property name is adapted at its own site.
        parse(
            "t.js",
            "'use strict'; var o = { public: 1, static: 2 }; o.public;",
        )
        .expect("property names are not identifiers");
    }

    #[test]
    fn a_function_name_is_checked_against_the_functions_own_strictness() {
        // Nothing outside this function is strict and the name is still refused, because the
        // directive inside it is what makes the name a problem. This is why the check waits until
        // after the directives have been read rather than using the enclosing value.
        let (_, _, message) = early("t.js", "function eval() { 'use strict'; }");
        assert_eq!(message, "Unexpected eval or arguments in strict mode");

        let (_, _, message) = early("t.js", "function f(eval) { 'use strict'; }");
        assert_eq!(message, "Unexpected eval or arguments in strict mode");

        parse("t.js", "function eval() {}").expect("sloppy code can still call it eval");
    }

    #[test]
    fn the_refusal_points_at_the_word_rather_than_the_statement() {
        // A message with the wrong column in it is worse than no column, because it sends the
        // reader to a line and then to the wrong place on it.
        let (line, column, _) = early("t.js", "'use strict';\nvar public = 1;");
        assert_eq!((line, column), (2, 5));
    }

    #[test]
    fn control_flow_keeps_its_shape() {
        let StmtKind::If {
            consequent,
            alternate,
            ..
        } = one("t.js", "if (a) { f(); } else g();").kind
        else {
            panic!("expected an if");
        };
        assert!(matches!(consequent.kind, StmtKind::Block(_)));
        let alternate = alternate.expect("an else branch");
        assert!(matches!(alternate.kind, StmtKind::Expr(_)));

        let StmtKind::While { body, .. } = one("t.js", "while (a) { f(); }").kind else {
            panic!("expected a while");
        };
        assert!(matches!(body.kind, StmtKind::Block(_)));
    }

    #[test]
    fn a_switch_keeps_its_clauses_in_source_order_and_the_default_keeps_its_place() {
        let StmtKind::Switch { cases, .. } = one(
            "t.js",
            "switch (a) { case 1: f(); default: g(); case 2: break; }",
        )
        .kind
        else {
            panic!("expected a switch");
        };
        let tests: Vec<bool> = cases.iter().map(|case| case.test.is_some()).collect();
        assert_eq!(
            tests,
            [true, false, true],
            "the default stays in the middle"
        );
        assert_eq!(
            cases[2].body.len(),
            1,
            "a clause body is a list, not a block"
        );
        assert!(matches!(cases[2].body[0].kind, StmtKind::Break(None)));
    }

    #[test]
    fn a_clause_with_nothing_in_it_is_a_clause_with_an_empty_body() {
        // `case 1:` with no statements is how fallthrough to a shared body is written, and it has
        // to survive as a clause, because dropping it would change which values reach that body.
        let StmtKind::Switch { cases, .. } =
            one("t.js", "switch (a) { case 1: case 2: f(); }").kind
        else {
            panic!("expected a switch");
        };
        assert_eq!(cases.len(), 2);
        assert!(cases[0].body.is_empty());
        assert_eq!(cases[1].body.len(), 1);
    }

    #[test]
    fn a_label_carries_its_name_and_the_statement_it_names() {
        let body = tree("t.js", "outer: while (a) { break outer; }");
        let StmtKind::Labeled { label, body } = &body[0].kind else {
            panic!("expected a labelled statement");
        };
        assert_eq!(label.name, "outer");
        let StmtKind::While { body, .. } = &body.kind else {
            panic!("expected the label to name the loop");
        };
        let StmtKind::Block(inner) = &body.kind else {
            panic!("expected a block");
        };
        let StmtKind::Break(Some(target)) = &inner[0].kind else {
            panic!("expected a labelled break");
        };
        assert_eq!(target.name, "outer");
    }

    #[test]
    fn a_label_obeys_the_identifier_spelling_rules() {
        // A label is not a binding and nothing can read it, but it is written with the identifier
        // production, so strict mode takes the same nine words away from it that it takes away
        // everywhere else. `eval` stays legal, because nothing here writes to anything.
        let (_, _, message) = early("t.js", "\"use strict\"; public: while (a) {}");
        assert_eq!(message, "Unexpected strict mode reserved word");
        let (_, _, message) = early("t.js", "\"use strict\"; while (a) { break yield; }");
        assert_eq!(message, "Unexpected strict mode reserved word");
        assert!(parse("t.js", "\"use strict\"; eval: while (a) { break eval; }").is_ok());
    }

    #[test]
    fn a_function_declaration_where_only_a_statement_belongs_is_refused_by_name() {
        // Annex B lets sloppy code write a function declaration as the single statement of an `if`
        // or under a label, and node runs all three of these. Nothing here declares the name yet,
        // so what katsu used to do was walk into a body whose function had never been declared and
        // panic. An honest unsupported message is the right answer until the hoisting rules that
        // go with the form are written, because a crash tells a user nothing about their program.
        assert_eq!(
            refused("t.js", "if (a) function f() {}"),
            "a function declaration outside a block"
        );
        assert_eq!(
            refused("t.js", "l: function f() {}"),
            "a function declaration outside a block"
        );
        assert_eq!(
            refused("t.js", "while (a) function f() {}"),
            "a function declaration outside a block"
        );

        // In a block it is an ordinary declaration and always was.
        assert!(parse("t.js", "if (a) { function f() {} }").is_ok());
    }

    #[test]
    fn an_empty_statement_is_kept_rather_than_dropped() {
        // Dropping it would make the statement count depend on formatting, and would leave the
        // body of `while (a);` with nothing in it at all.
        let body = tree("t.js", ";;");
        assert_eq!(body.len(), 2);
        assert!(body.iter().all(|s| matches!(s.kind, StmtKind::Empty)));
    }

    #[test]
    fn a_refusal_inside_a_nested_function_still_reports_a_position() {
        let error = parse("t.js", "function f() {\n  return [1];\n}").expect_err("refused");
        let ParseError::Unsupported {
            line,
            column,
            construct,
            ..
        } = error
        else {
            panic!("expected an unsupported construct, got {error:?}");
        };
        assert_eq!(construct, "an array literal");
        assert_eq!((line, column), (2, 10));
    }

    #[test]
    fn a_column_is_counted_in_characters_and_not_in_bytes() {
        // The accented word is two bytes per character in the source. A column measured in bytes
        // would put the caret past the end of the line in an editor.
        let error = parse("t.js", "const é = 1; const bad = [];").expect_err("refused");
        let ParseError::Unsupported { line, column, .. } = error else {
            panic!("expected an unsupported construct, got {error:?}");
        };
        assert_eq!((line, column), (1, 26));
    }
}
