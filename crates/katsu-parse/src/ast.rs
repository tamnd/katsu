//! Our own syntax tree, the one every later pass is allowed to look at.
//!
//! Nothing in this file mentions oxc. That is the whole point. `spec/04-frontend.md` promises that
//! exactly one module consumes the parser's output and that swapping the parser later is a week of
//! work rather than a quarter, and the only way to keep that promise is to have a tree of our own
//! for the adapter to build. If a pass downstream of here reaches for an oxc type, the promise is
//! already broken and nobody will notice until the day we try to move.
//!
//! Two things shape the design. Every node carries a span from the moment it is built, because
//! source positions retrofitted into a tree are always wrong somewhere and stack traces are a
//! compatibility requirement rather than a nicety. And assignment targets are their own type
//! rather than an expression that lowering has to re-check, so an unassignable expression on the
//! left of an equals sign is rejected here, once, instead of in every pass that walks an
//! assignment.
//!
//! The tree covers the M0 subset and no more. Everything else is refused by name in `adapter.rs`,
//! which is a deliberate choice: a runtime that quietly produces wrong bytecode for a construct it
//! does not understand is worse than one that says it does not understand the construct.

/// A half open byte range into the source text, the same convention the parser uses.
///
/// Bytes and not characters, because that is what a source map wants and what slicing the original
/// text needs. Turning one of these into a line and column happens once, at the point where a
/// human is going to read it, and not on every node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first byte of the node.
    pub start: u32,
    /// Byte offset one past the last byte of the node.
    pub end: u32,
}

impl Span {
    /// Build a span from a start and an end offset.
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// How many bytes of source the node covers.
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span covers no source at all, which happens for synthesised nodes.
    pub const fn is_empty(self) -> bool {
        self.end <= self.start
    }
}

/// A name in the source, with the place it was written.
///
/// The name is an owned `String` and not an interned atom, because interning needs the heap and
/// `katsu-parse` and `katsu-gc` are both at layer 2, so neither can depend on the other. Interning
/// happens at lowering, which sits above both. That is a real cost of the layering, one allocation
/// per identifier occurrence, and it is worth paying to keep the parser independent of the heap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    /// Where the name was written.
    pub span: Span,
    /// The name itself, already unescaped.
    pub name: String,
}

impl Ident {
    /// Build an identifier from a span and a name.
    pub fn new(span: Span, name: impl Into<String>) -> Self {
        Self {
            span,
            name: name.into(),
        }
    }
}

/// A unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// `+x`, which is `ToNumber`.
    Plus,
    /// `-x`.
    Minus,
    /// `!x`.
    Not,
    /// `~x`.
    BitNot,
    /// `typeof x`, which does not throw on an unresolvable name and so cannot be lowered as a plain
    /// load followed by a call.
    Typeof,
    /// `void x`, which evaluates the operand and yields undefined.
    Void,
    /// `delete x.y`.
    Delete,
}

/// A prefix or postfix increment or decrement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOp {
    /// `++`.
    Increment,
    /// `--`.
    Decrement,
}

/// A binary operator.
///
/// The full set, including `in` and `instanceof`, because the tree's job is to represent what was
/// written. Whether a given operator has bytecode behind it yet is lowering's problem and lowering
/// reports it with its own error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    /// `==`.
    Equal,
    /// `!=`.
    NotEqual,
    /// `===`.
    StrictEqual,
    /// `!==`.
    StrictNotEqual,
    /// `<`.
    Less,
    /// `<=`.
    LessEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterEqual,
    /// `+`, which is addition or string concatenation depending on the operands.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Rem,
    /// `**`.
    Pow,
    /// `<<`.
    Shl,
    /// `>>`, the sign propagating shift.
    Shr,
    /// `>>>`, the zero filling shift.
    UnsignedShr,
    /// `|`.
    BitOr,
    /// `^`.
    BitXor,
    /// `&`.
    BitAnd,
    /// `in`.
    In,
    /// `instanceof`.
    Instanceof,
}

/// A short circuiting operator.
///
/// Separate from `BinaryOp` because these do not evaluate both sides, so they lower to a branch and
/// not to an instruction. Keeping them apart in the tree means lowering cannot forget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalOp {
    /// `&&`.
    And,
    /// `||`.
    Or,
    /// `??`.
    Coalesce,
}

/// An assignment operator.
///
/// `Assign` is a plain store. Every other variant is a read, an operation and a store, and the
/// three logical ones short circuit, which means they do not always store at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`.
    Assign,
    /// A compound assignment such as `+=`, carrying the operation it applies.
    Binary(BinaryOp),
    /// A short circuiting assignment such as `&&=`, which only stores if the left side says so.
    Logical(LogicalOp),
}

/// What a value can be assigned to.
///
/// A separate type from `Expr` on purpose. The grammar allows only a few shapes on the left of an
/// assignment, and encoding that here means lowering can pattern match on three cases with no
/// fallback arm for the impossible ones. Destructuring patterns are not in the M0 subset and so are
/// not here yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// Where the target was written.
    pub span: Span,
    /// Which shape of target it is.
    pub kind: TargetKind,
}

/// The shapes a value can be assigned to.
#[derive(Clone, Debug, PartialEq)]
pub enum TargetKind {
    /// A bare name, as in `x = 1`.
    Ident(Ident),
    /// A fixed property name, as in `o.x = 1`.
    Field {
        /// The object being stored into.
        object: Box<Expr>,
        /// The property name, known at compile time, which is what makes an inline cache possible.
        name: Ident,
    },
    /// A computed property, as in `o[k] = 1`.
    Index {
        /// The object being stored into.
        object: Box<Expr>,
        /// The key expression, evaluated at run time.
        index: Box<Expr>,
    },
}

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    /// Where the expression was written.
    pub span: Span,
    /// Which kind of expression it is.
    pub kind: ExprKind,
}

impl Expr {
    /// Build an expression from a span and a kind.
    pub fn new(span: Span, kind: ExprKind) -> Self {
        Self { span, kind }
    }
}

/// The kinds of expression in the M0 subset.
#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// A numeric literal, already converted to the double it denotes.
    ///
    /// The parser has done the decimal to binary conversion and it is required to be correctly
    /// rounded, so there is nothing left for us to decide. An integer valued literal small enough
    /// to be a small integer is recognised at lowering, where the constant pool lives.
    Number(f64),
    /// A string literal, with escapes already resolved.
    String(String),
    /// `true` or `false`.
    Boolean(bool),
    /// `null`.
    Null,
    /// A reference to a name.
    Ident(Ident),
    /// `this`.
    This,
    /// A unary operator applied to one operand.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<Expr>,
    },
    /// An increment or decrement, prefix or postfix.
    Update {
        /// Increment or decrement.
        op: UpdateOp,
        /// True for `++x`, false for `x++`. The difference is which value the expression yields,
        /// and it matters even in statement position once the result is used.
        prefix: bool,
        /// What is being incremented, which the grammar restricts to an assignable shape.
        target: Target,
    },
    /// A binary operator applied to two operands, both of which are always evaluated.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// A short circuiting operator, where the right side may not be evaluated.
    Logical {
        /// The operator.
        op: LogicalOp,
        /// The left operand, always evaluated.
        left: Box<Expr>,
        /// The right operand, evaluated only if the left side does not decide the result.
        right: Box<Expr>,
    },
    /// An assignment, which is an expression because its value is the value stored.
    Assign {
        /// Plain, compound or short circuiting.
        op: AssignOp,
        /// Where the value goes.
        target: Target,
        /// The value.
        value: Box<Expr>,
    },
    /// `test ? consequent : alternate`.
    Conditional {
        /// The condition.
        test: Box<Expr>,
        /// The value if the condition is truthy.
        consequent: Box<Expr>,
        /// The value if it is not.
        alternate: Box<Expr>,
    },
    /// A property read with a name known at compile time, as in `o.x`.
    Field {
        /// The object.
        object: Box<Expr>,
        /// The property name.
        name: Ident,
    },
    /// A property read with a computed key, as in `o[k]`.
    Index {
        /// The object.
        object: Box<Expr>,
        /// The key expression.
        index: Box<Expr>,
    },
    /// A call.
    Call {
        /// What is being called. A `Field` callee is a method call and gets the object as the
        /// receiver, which lowering has to preserve, so the shape is kept rather than flattened.
        callee: Box<Expr>,
        /// The arguments, in source order. Spread is not in the M0 subset.
        arguments: Vec<Expr>,
    },
    /// A function expression, named or not.
    Function(Box<Func>),
}

/// How a variable was declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclKind {
    /// `var`, function scoped and hoisted to the top of the enclosing function.
    Var,
    /// `let`, block scoped, in the temporal dead zone until its declaration runs.
    Let,
    /// `const`, block scoped and write once.
    Const,
}

/// One name bound by a declaration, with its initialiser if it has one.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    /// The whole declarator, name and initialiser together.
    pub span: Span,
    /// The name being bound. Destructuring patterns are not in the M0 subset.
    pub name: Ident,
    /// The initialiser, absent for `var x;` and `let x;`.
    pub init: Option<Expr>,
}

/// A function, however it was written.
///
/// Function declarations and function expressions share this type because the difference between
/// them is where the binding goes, which is a statement level question, and not what the function
/// is.
#[derive(Clone, Debug, PartialEq)]
pub struct Func {
    /// The whole function, from the keyword to the closing brace.
    pub span: Span,
    /// The name, absent for an anonymous function expression.
    pub name: Option<Ident>,
    /// The parameters. Defaults, rest and destructuring are not in the M0 subset.
    pub params: Vec<Ident>,
    /// The body.
    pub body: Vec<Stmt>,
    /// Whether the body runs in strict mode.
    ///
    /// Computed here rather than left for a later pass, because strictness is inherited from the
    /// enclosing code and the adapter is the only place that still has the nesting in hand. It
    /// changes what `this` is in a plain call and whether an assignment to an undeclared name
    /// throws, so it is not a detail that can be filled in afterwards.
    pub strict: bool,
}

/// One clause of a `switch`.
///
/// The test is absent for `default`. The statements are not a scope of their own: every clause of a
/// switch shares one block scope, so a `let` in one clause is visible in the next and is in its
/// temporal dead zone until its own clause runs. Giving each clause a scope would be the intuitive
/// reading and it is not what the standard says.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    /// The whole clause, from `case` or `default` to the start of the next one.
    pub span: Span,
    /// The value compared against the discriminant, absent for `default`.
    pub test: Option<Expr>,
    /// The statements of the clause, which fall through into the next clause unless something
    /// leaves.
    pub body: Vec<Stmt>,
}

/// A statement.
#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    /// Where the statement was written.
    pub span: Span,
    /// Which kind of statement it is.
    pub kind: StmtKind,
}

impl Stmt {
    /// Build a statement from a span and a kind.
    pub fn new(span: Span, kind: StmtKind) -> Self {
        Self { span, kind }
    }
}

/// The kinds of statement in the M0 subset.
#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    /// An expression evaluated for its effect.
    Expr(Expr),
    /// A `var`, `let` or `const` declaration, which may bind several names at once.
    Declare {
        /// Which of the three it was.
        kind: DeclKind,
        /// The names bound, in source order.
        bindings: Vec<Binding>,
    },
    /// A function declaration, which binds its name in the enclosing scope.
    Function(Box<Func>),
    /// `return`, with or without a value.
    Return(Option<Expr>),
    /// `if`, with an optional `else`.
    If {
        /// The condition.
        test: Expr,
        /// The branch taken when the condition is truthy.
        consequent: Box<Stmt>,
        /// The `else` branch, if there is one.
        alternate: Option<Box<Stmt>>,
    },
    /// `while`.
    While {
        /// The condition, tested before every iteration including the first.
        test: Expr,
        /// The body.
        body: Box<Stmt>,
    },
    /// `switch`, with its clauses in the order they were written.
    Switch {
        /// The value every clause is compared against, evaluated once.
        discriminant: Expr,
        /// The clauses, `default` among them and in its written position, because where it sits
        /// decides where control lands when nothing matched and also what falls through into it.
        cases: Vec<Case>,
    },
    /// `break`, which leaves the nearest enclosing loop or switch.
    ///
    /// No label, because a labelled statement is not in the subset yet and a label can only name
    /// one, so there is nothing a label could refer to.
    Break,
    /// `continue`, which starts the next iteration of the nearest enclosing loop.
    Continue,
    /// A braced block, which is a scope for `let` and `const`.
    Block(Vec<Stmt>),
    /// A lone semicolon.
    ///
    /// Kept rather than dropped because a tree that silently loses nodes is a tree whose spans
    /// stop lining up with the source, and because dropping it would make the statement count in
    /// a test depend on formatting.
    Empty,
}

/// One source file, adapted.
#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    /// The path the source came from, kept for diagnostics and stack traces.
    pub path: String,
    /// The top level statements.
    pub body: Vec<Stmt>,
    /// Whether the top level runs in strict mode.
    ///
    /// True for an ES module without anything being written, because modules are always strict.
    /// True for a script that opens with a `use strict` directive.
    pub strict: bool,
}
