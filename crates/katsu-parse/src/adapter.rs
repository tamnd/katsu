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
    AssignOp, BinaryOp, Binding, DeclKind, Expr, ExprKind, Func, Ident, LogicalOp, Module, Span,
    Stmt, StmtKind, Target, TargetKind, UnaryOp, UpdateOp,
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

    /// Adapt the statement in the arm of an `if` or the body of a `while`.
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
            let name = self.binding_name(&declarator.id)?;
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

        Ok(Stmt::new(
            span(node.span),
            StmtKind::Declare { kind, bindings },
        ))
    }

    /// Pull the single name out of a binding position, refusing destructuring.
    ///
    /// The type annotation hanging off the pattern is not read anywhere in this function, which is
    /// what erasure looks like in practice.
    fn binding_name(&self, pattern: &oxc::BindingPattern<'_>) -> Result<Ident, ParseError> {
        match pattern {
            oxc::BindingPattern::BindingIdentifier(node) => {
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
            params.push(self.binding_name(&parameter.pattern)?);
        }

        let name = node
            .id
            .as_ref()
            .map(|id| Ident::new(span(id.span), id.name.as_str()));

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

            oxc::Expression::Identifier(node) => Expr::new(
                span(node.span),
                ExprKind::Ident(Ident::new(span(node.span), node.name.as_str())),
            ),

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
            oxc::SimpleAssignmentTarget::AssignmentTargetIdentifier(node) => Ok(Target {
                span: span(node.span),
                kind: TargetKind::Ident(Ident::new(span(node.span), node.name.as_str())),
            }),

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
        oxc::Statement::BreakStatement(_) => "break",
        oxc::Statement::ContinueStatement(_) => "continue",
        oxc::Statement::DebuggerStatement(_) => "debugger",
        oxc::Statement::DoWhileStatement(_) => "a do while loop",
        oxc::Statement::ForInStatement(_) => "a for in loop",
        oxc::Statement::ForOfStatement(_) => "a for of loop",
        oxc::Statement::ForStatement(_) => "a for loop",
        oxc::Statement::LabeledStatement(_) => "a label",
        oxc::Statement::SwitchStatement(_) => "a switch",
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
fn line_and_column(source: &str, offset: u32) -> (u32, u32) {
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
        AssignOp, BinaryOp, DeclKind, Expr, ExprKind, LogicalOp, Stmt, StmtKind, TargetKind,
        UnaryOp, UpdateOp,
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
    fn what_m0_does_not_cover_is_refused_by_name() {
        // This list is the M1 work list read backwards, and every line of it should disappear.
        assert_eq!(refused("t.js", "for (;;) {}"), "a for loop");
        assert_eq!(refused("t.js", "for (const x of xs) {}"), "a for of loop");
        assert_eq!(refused("t.js", "try { f(); } catch {}"), "try");
        assert_eq!(refused("t.js", "throw new Error();"), "throw");
        assert_eq!(refused("t.js", "switch (x) {}"), "a switch");
        assert_eq!(refused("t.js", "class C {}"), "a class");
        assert_eq!(refused("t.js", "let x = [1, 2];"), "an array literal");
        assert_eq!(refused("t.js", "let x = { a: 1 };"), "an object literal");
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
