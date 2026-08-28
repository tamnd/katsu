//! Scope analysis: every name resolved to a slot before anything runs.
//!
//! `spec/04-frontend.md` 4.4 says that every identifier reference is resolved statically, into a
//! frame slot, a cell some number of environments up the chain, or a property of the global
//! object, and that a variable no closure captures never touches the heap. This is that pass. It
//! runs once, between the adapter and lowering, and it exists so that the interpreter never has to
//! ask a question about a name at run time.
//!
//! The output is three things. A function scope per function, saying how many frame slots and how
//! many cells it needs. A binding per declared name, saying where it lives and whether anything
//! captured it. And a resolution per identifier occurrence in the tree, keyed by where that
//! identifier was written.
//!
//! Keying by source position is the part worth defending. The alternative is to number identifiers
//! in a walk and have lowering repeat the same walk in the same order, which works right up until
//! one of the two walks changes and then produces a program that runs and is wrong. A byte offset
//! is unique per occurrence, it survives any reordering, and a lookup that misses is a loud
//! `None` rather than a quietly shifted answer.
//!
//! ## The top level is a function
//!
//! Top level `var` in a real script is a property of the global object, and that is not what we
//! implement here. It is also not what we run: Node wraps every CommonJS module in a function
//! before evaluating it, and an ES module has its own scope by definition, so in both of the
//! things katsu actually executes the top level is a function scope. The case where the
//! distinction bites is `eval` and a browser script tag, and neither exists yet.
//!
//! ## What is not here yet
//!
//! The Annex B sloppy mode rules for a function declaration inside a block, which also create a
//! var binding in the enclosing function, are not implemented. A function declaration in a block
//! is treated as an ordinary lexical binding of that block, which is the strict mode rule and the
//! one that modern code relies on.
//!
//! `arguments` is recognised and recorded on the function that mentions it, so that materialising
//! the object can be conditional the way 4.4 asks, but there is no arguments object to resolve it
//! to yet and the reference falls through to a global lookup. That is correct at the top level and
//! wrong inside a function, and it is the flag rather than the resolution that M1 should read.
//!
//! A `let` read from inside a closure is always checked for the dead zone, because textual order
//! says nothing about when the closure runs. Proving some of those checks dead needs a definite
//! assignment analysis, which is worth doing and is not worth doing before there is an interpreter
//! to measure it against.
//!
//! Direct `eval` and `with` poison a scope and lose static resolution for everything in it. Both
//! are refused by the adapter today, so there is nothing here to poison.

use std::rc::Rc;

use rustc_hash::FxHashMap;

use crate::ast::{
    Case, DeclKind, Expr, ExprKind, Func, Ident, Module, Span, Stmt, StmtKind, Target, TargetKind,
};

/// Which function a scope belongs to.
///
/// Zero is always the top level, which is why it is not an `Option`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionId(pub u32);

/// One declared name, anywhere in the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BindingId(pub u32);

/// How a name came to be bound, which is what decides its initialisation and its writability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    /// `var`, hoisted to the top of the enclosing function and initialised to undefined there.
    Var,
    /// `let`, block scoped, in the temporal dead zone until its declaration runs.
    Let,
    /// `const`, block scoped, in the temporal dead zone, and a write to it throws at run time.
    Const,
    /// A parameter, initialised by the calling convention before the body starts.
    Param,
    /// A function declaration, or the name a named function expression can see itself by. Both are
    /// initialised when the scope is entered, so neither has a dead zone.
    Function,
}

impl BindingKind {
    /// Whether a read before the declaration runs has to be checked for at run time.
    pub const fn has_dead_zone(self) -> bool {
        matches!(self, Self::Let | Self::Const)
    }

    /// Whether a write to this binding throws.
    pub const fn is_constant(self) -> bool {
        matches!(self, Self::Const)
    }

    /// Whether this kind occupies a scope exclusively, so that a second declaration is an error.
    ///
    /// `var`, parameters and function declarations are all var like and coexist with each other,
    /// which is why `function f(a) { var a; }` is legal and has always been legal.
    const fn is_exclusive(self) -> bool {
        matches!(self, Self::Let | Self::Const)
    }
}

/// One name, declared once, in one function.
#[derive(Clone, Debug)]
pub struct Binding {
    /// The name as written.
    ///
    /// Shared with the key in the scope it was declared in, because a declaration otherwise costs
    /// two copies of the same short string and a file of two hundred small functions has a
    /// thousand declarations in it.
    pub name: Rc<str>,
    /// The declarator, from the name to the end of the initialiser if there is one.
    ///
    /// The end matters as much as the start. A `let` is in its dead zone until its initialiser has
    /// finished running, which is why `let x = x;` throws, so the point a reference is compared
    /// against is the end of this span and not the start.
    pub span: Span,
    /// How it was declared.
    pub kind: BindingKind,
    /// The function whose scope it belongs to.
    pub function: FunctionId,
    /// Whether anything inside a nested function reads or writes it.
    ///
    /// This is the flag 4.4 is about. False means the binding lives in a register and never
    /// touches the heap, which is the common case by a wide margin.
    pub captured: bool,
    /// Whether any reference to it is checked for the temporal dead zone.
    ///
    /// Lowering writes the hole into a slot when the scope is entered, and there is no reason to do
    /// that for a binding nothing ever checks. The ordinary `let x = 1;` read afterwards costs
    /// nothing because of this flag.
    pub needs_dead_zone: bool,
    /// The frame slot if it is not captured, or the cell index in its function's environment if it
    /// is. Assigned once the whole function has been walked, because capture is not known before
    /// then.
    pub slot: u16,
}

/// Where a name lives, from the point of view of the code that mentions it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A slot in the frame of the function doing the mentioning.
    Local {
        /// The frame slot.
        slot: u16,
        /// Whether the read has to check for the temporal dead zone first.
        tdz: bool,
    },
    /// A cell in an environment, `hops` links up the chain.
    ///
    /// Zero hops means the environment of the function doing the mentioning. A function with no
    /// cells of its own has no environment, and its chain starts at the nearest enclosing one, so
    /// the hop count is a count of environments and not of function boundaries.
    Upvalue {
        /// How many links up the environment chain.
        hops: u16,
        /// The cell index within that environment.
        slot: u16,
        /// Whether the read has to check for the temporal dead zone first.
        tdz: bool,
    },
    /// Not declared anywhere we can see, so it is a property of the global object.
    Global,
}

/// One identifier occurrence, resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reference {
    /// Where the name lives.
    pub resolution: Resolution,
    /// Which binding it names, absent for a global.
    ///
    /// Lowering needs this as well as the resolution, because the resolution says where to read
    /// and the binding says whether writing there throws.
    pub binding: Option<BindingId>,
}

/// Everything known about one function's scope.
#[derive(Clone, Debug)]
pub struct FunctionScope {
    /// The source name, empty for the top level and for an anonymous function expression.
    pub name: String,
    /// The whole function, or the whole file for the top level.
    pub span: Span,
    /// The enclosing function, absent only for the top level.
    pub parent: Option<FunctionId>,
    /// Every name declared directly in this function, in declaration order.
    pub bindings: Vec<BindingId>,
    /// How many parameters were written, which is also how many registers the caller fills.
    pub arity: u16,
    /// How many frame slots the declared names need.
    ///
    /// Counts the parameters whether or not they are captured, because the calling convention puts
    /// argument number `n` in register `n` and a captured parameter is copied out of that register
    /// by the prologue rather than never arriving in it. Lowering's temporaries start above this,
    /// since these slots are live for the whole call.
    pub frame_slots: u16,
    /// How many cells the environment needs, zero if nothing is captured.
    pub cell_slots: u16,
    /// Whether the body mentions `this`.
    pub uses_this: bool,
    /// Whether the body mentions `arguments`.
    pub uses_arguments: bool,
    /// Whether the body runs in strict mode.
    pub strict: bool,
}

impl FunctionScope {
    /// Whether calling this function has to allocate an environment.
    ///
    /// A function that captures nothing does not, which is the whole point of the analysis.
    pub const fn needs_environment(&self) -> bool {
        self.cell_slots > 0
    }
}

/// The result of the pass.
#[derive(Clone, Debug)]
pub struct Scopes {
    functions: Vec<FunctionScope>,
    bindings: Vec<Binding>,
    references: FxHashMap<u32, Reference>,
    functions_by_span: FxHashMap<u32, FunctionId>,
}

impl Scopes {
    /// The top level scope, which is function zero.
    pub fn top_level(&self) -> &FunctionScope {
        &self.functions[0]
    }

    /// Every function in the program, the top level first and the rest in source order.
    pub fn functions(&self) -> &[FunctionScope] {
        &self.functions
    }

    /// One function by id.
    pub fn function(&self, id: FunctionId) -> &FunctionScope {
        &self.functions[id.0 as usize]
    }

    /// Every binding in the program.
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// One binding by id.
    pub fn binding(&self, id: BindingId) -> &Binding {
        &self.bindings[id.0 as usize]
    }

    /// What one identifier occurrence resolves to.
    ///
    /// Every identifier the walk saw has an entry, including the name in a declaration, so lowering
    /// can ask the same question about a declaration and a use and get an answer in the same shape.
    /// `None` means the identifier is not one this pass looked at, which is a bug rather than a
    /// program that does something interesting.
    pub fn reference(&self, ident: &Ident) -> Option<Reference> {
        self.references.get(&ident.span.start).copied()
    }

    /// How many identifier occurrences were resolved.
    pub fn reference_count(&self) -> usize {
        self.references.len()
    }

    /// Which scope belongs to the function written at this span.
    ///
    /// Keyed by where the function starts, for the same reason references are keyed by where an
    /// identifier starts. The alternative is for lowering to walk the tree in the same order this
    /// pass did and count, which works until one of the two walks changes. Two functions cannot
    /// start at the same byte, so the key is unique. The top level is not in the map, because it is
    /// function zero and nobody has to look it up.
    pub fn function_of(&self, span: Span) -> Option<FunctionId> {
        self.functions_by_span.get(&span.start).copied()
    }
}

/// A rule the language checks before running anything, broken.
///
/// Carries the span rather than a line and a column, because turning one into the other needs the
/// source text and this module does not have it. The caller in `lib.rs` does.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ScopeError {
    /// Where to point.
    pub span: Span,
    /// What is wrong, phrased the way the engines phrase it, because these messages end up in
    /// other people's test expectations.
    pub message: String,
}

/// Resolve every name in a module.
pub fn analyse(module: &Module) -> Result<Scopes, ScopeError> {
    let mut analyser = Analyser::default();
    analyser.top_level(module)?;
    Ok(analyser.finish())
}

/// One lexical scope while the walk is in progress.
struct BlockScope {
    /// The braces this scope covers, used to decide whether a hoisted `var` passes through it.
    span: Span,
    /// The names declared directly in it.
    names: FxHashMap<Rc<str>, BindingId>,
}

/// The function currently being walked, and where its own scope sits on the block stack.
struct Frame {
    id: FunctionId,
    base: usize,
    /// The enclosing function's loop count, put back when this one ends.
    loops: usize,
    /// The enclosing function's count of loops and switches together.
    breakables: usize,
}

/// One identifier occurrence, before slots exist.
struct RawReference {
    at: u32,
    binding: Option<BindingId>,
    from: FunctionId,
    tdz: bool,
}

#[derive(Default)]
struct Analyser {
    functions: Vec<FunctionScope>,
    bindings: Vec<Binding>,
    blocks: Vec<BlockScope>,
    frames: Vec<Frame>,
    references: Vec<RawReference>,
    /// How many loops enclose the statement being walked, which is what `continue` needs.
    loops: usize,
    /// How many loops and switches together, which is what `break` needs.
    ///
    /// Two counters rather than one, because `continue` inside a switch is an error and `break`
    /// inside one is not, and both counters reset at a function boundary because a `break` cannot
    /// leave the function it was written in.
    breakables: usize,
}

impl Analyser {
    /// Walk the file, which is a function like any other for the reason in the module docs.
    fn top_level(&mut self, module: &Module) -> Result<(), ScopeError> {
        // The file has no span of its own, and the only thing a function scope's span is used for
        // is deciding whether a nested `var` hoisted through a block, so a span covering the whole
        // possible file is both correct and never a false positive.
        let span = Span::new(0, u32::MAX);
        let id = self.push_function(String::new(), span, 0, module.strict);
        self.hoist_vars(&module.body)?;
        self.block_body(&module.body)?;
        self.pop_function(id);
        Ok(())
    }

    /// Walk one function, whether it was written as a declaration or as an expression.
    ///
    /// `self_named` is true for a function expression with a name, where the name is visible
    /// inside the function and nowhere else. For a declaration the name belongs to the enclosing
    /// scope and was declared there before this is called.
    fn function(&mut self, func: &Func, self_named: bool) -> Result<(), ScopeError> {
        let name = func
            .name
            .as_ref()
            .map_or_else(String::new, |n| n.name.clone());
        let arity = u16::try_from(func.params.len())
            .expect("a function has fewer than sixty five thousand parameters");
        let id = self.push_function(name, func.span, arity, func.strict);

        if self_named && let Some(own) = func.name.as_ref() {
            let binding = self.declare(own, BindingKind::Function, own.span)?;
            self.declaration_reference(own, binding);
        }

        for param in &func.params {
            // Two parameters with one name is legal in sloppy mode and the later one wins, which
            // is why `declare` lets them coexist. Strict mode makes it an early error, and this is
            // the only place that can tell the difference between a repeated parameter and a
            // parameter shadowing the name a function expression sees itself by.
            if func.strict
                && self
                    .declared_in_scope(&param.name)
                    .is_some_and(|kind| kind == BindingKind::Param)
            {
                return Err(ScopeError {
                    span: param.span,
                    message: "Duplicate parameter name not allowed in this context".to_owned(),
                });
            }
            let binding = self.declare(param, BindingKind::Param, param.span)?;
            self.declaration_reference(param, binding);
        }

        self.hoist_vars(&func.body)?;
        self.block_body(&func.body)?;
        self.pop_function(id);
        Ok(())
    }

    /// Start a function scope and the block scope that is its body.
    fn push_function(&mut self, name: String, span: Span, arity: u16, strict: bool) -> FunctionId {
        let id = FunctionId(
            u32::try_from(self.functions.len())
                .expect("a program has fewer than four billion functions"),
        );
        self.functions.push(FunctionScope {
            name,
            span,
            parent: self.frames.last().map(|frame| frame.id),
            bindings: Vec::new(),
            arity,
            frame_slots: 0,
            cell_slots: 0,
            uses_this: false,
            uses_arguments: false,
            strict,
        });
        self.frames.push(Frame {
            id,
            base: self.blocks.len(),
            loops: self.loops,
            breakables: self.breakables,
        });
        // A `break` cannot leave the function it was written in, so a function written inside a
        // loop starts the count again rather than inheriting it.
        self.loops = 0;
        self.breakables = 0;
        self.push_block(span);
        id
    }

    /// End a function scope, dropping every block scope it opened.
    fn pop_function(&mut self, id: FunctionId) {
        let frame = self.frames.pop().expect("a function scope was pushed");
        debug_assert_eq!(frame.id, id, "function scopes nest");
        self.blocks.truncate(frame.base);
        self.loops = frame.loops;
        self.breakables = frame.breakables;
    }

    fn push_block(&mut self, span: Span) {
        self.blocks.push(BlockScope {
            span,
            names: FxHashMap::default(),
        });
    }

    fn current(&self) -> FunctionId {
        self.frames.last().map_or(FunctionId(0), |frame| frame.id)
    }

    /// Declare every name a list of statements binds lexically, before any of them is walked.
    ///
    /// Hoisting is why this is a separate pass over the same list. A function declared at the top
    /// of a block can refer to a `let` declared at the bottom of it, so every name in a scope has
    /// to exist before any reference in that scope is resolved.
    fn declare_lexicals(&mut self, body: &[Stmt]) -> Result<(), ScopeError> {
        for statement in body {
            match &statement.kind {
                StmtKind::Declare { kind, bindings } if *kind != DeclKind::Var => {
                    let kind = match kind {
                        DeclKind::Let => BindingKind::Let,
                        _ => BindingKind::Const,
                    };
                    for binding in bindings {
                        if kind == BindingKind::Const && binding.init.is_none() {
                            return Err(ScopeError {
                                span: binding.span,
                                message: "Missing initializer in const declaration".to_owned(),
                            });
                        }
                        self.declare(&binding.name, kind, binding.span)?;
                    }
                }
                StmtKind::Function(func) => {
                    if let Some(name) = func.name.as_ref() {
                        self.declare(name, BindingKind::Function, name.span)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Bind one name in the innermost scope, refusing a clash.
    ///
    /// `span` is the declarator rather than the name, because the end of the declarator is where
    /// the dead zone stops.
    fn declare(
        &mut self,
        name: &Ident,
        kind: BindingKind,
        span: Span,
    ) -> Result<BindingId, ScopeError> {
        let scope = self.blocks.last().expect("a scope is always open");

        if let Some(existing) = scope.names.get(name.name.as_str()).copied() {
            let existing = self.bindings[existing.0 as usize].kind;
            if existing.is_exclusive() || kind.is_exclusive() {
                return Err(already_declared(name));
            }
        }

        // A `var` hoisted out of a block and past a `let` of the same name is the same collision
        // written the other way round, and the block it passed through is the one that catches it.
        // Containment of spans is what says it passed through: a block scope covers every byte of
        // everything nested inside it.
        if kind.is_exclusive()
            && let Some(clash) = self.hoisted_var_inside(&name.name, scope.span)
        {
            return Err(already_declared(name).with_span_of(clash));
        }

        let id = BindingId(
            u32::try_from(self.bindings.len())
                .expect("a program has fewer than four billion bindings"),
        );
        let function = self.current();
        let shared: Rc<str> = Rc::from(name.name.as_str());
        self.bindings.push(Binding {
            name: Rc::clone(&shared),
            span,
            kind,
            function,
            captured: false,
            needs_dead_zone: false,
            slot: 0,
        });
        self.functions[function.0 as usize].bindings.push(id);
        self.blocks
            .last_mut()
            .expect("a scope is always open")
            .names
            .insert(shared, id);
        Ok(id)
    }

    /// Whether a `var` of this name was written inside the given block.
    fn hoisted_var_inside(&self, name: &str, block: Span) -> Option<Span> {
        let base = self.frames.last()?.base;
        let id = *self.blocks.get(base)?.names.get(name)?;
        let binding = &self.bindings[id.0 as usize];
        (binding.kind == BindingKind::Var
            && binding.span.start >= block.start
            && binding.span.end <= block.end)
            .then_some(binding.span)
    }

    /// Declare every `var` in a function body, wherever inside it they were written.
    ///
    /// A `var` belongs to the function and not to the block it appears in, so this walks through
    /// blocks and control flow and stops at a nested function, which owns its own.
    fn hoist_vars(&mut self, body: &[Stmt]) -> Result<(), ScopeError> {
        for statement in body {
            match &statement.kind {
                StmtKind::Declare {
                    kind: DeclKind::Var,
                    bindings,
                } => {
                    for binding in bindings {
                        // A second `var` of the same name is not a redeclaration, it is the same
                        // binding written twice, which is legal and common in old code, and so is
                        // a `var` that repeats a parameter. The question is only about this
                        // function's own scope: a name of the same spelling in an enclosing
                        // function is a different binding and this one shadows it.
                        if self.declared_in_scope(&binding.name.name).is_none() {
                            self.declare(&binding.name, BindingKind::Var, binding.span)?;
                        }
                    }
                }
                StmtKind::Block(body) => self.hoist_vars(body)?,
                StmtKind::If {
                    consequent,
                    alternate,
                    ..
                } => {
                    self.hoist_vars(std::slice::from_ref(consequent))?;
                    if let Some(alternate) = alternate {
                        self.hoist_vars(std::slice::from_ref(alternate))?;
                    }
                }
                StmtKind::While { body, .. } => self.hoist_vars(std::slice::from_ref(body))?,
                StmtKind::Switch { cases, .. } => {
                    for case in cases {
                        self.hoist_vars(&case.body)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Declare a scope's names and then walk its statements.
    fn block_body(&mut self, body: &[Stmt]) -> Result<(), ScopeError> {
        self.declare_lexicals(body)?;
        for statement in body {
            self.statement(statement)?;
        }
        Ok(())
    }

    /// Declare the names of every clause of a switch, then walk all of them.
    ///
    /// Every clause shares the one scope the caller has already pushed, so the declarations of all
    /// of them go in before any of them is walked. That is what makes a `let` in a later clause
    /// visible, and in its dead zone, in an earlier one, and it is also what makes the same name
    /// declared in two clauses the redeclaration error it is.
    fn switch_body(&mut self, cases: &[Case]) -> Result<(), ScopeError> {
        for case in cases {
            self.declare_lexicals(&case.body)?;
        }
        for case in cases {
            if let Some(test) = &case.test {
                self.expression(test)?;
            }
            for statement in &case.body {
                self.statement(statement)?;
            }
        }
        Ok(())
    }

    /// Walk something a `break` can leave, and that a `continue` can restart if it is a loop.
    ///
    /// The counters go up for the body and are put back after it, so the question a `break` asks is
    /// answered by whether the count is zero rather than by a search back up a stack. They are put
    /// back rather than decremented because the body can fail part way through a nested function,
    /// which zeroes them on the way in and never gets to the line that restores them, and a
    /// decrement from zero would panic on the way back out of a program that is already refused.
    fn breakable(
        &mut self,
        is_loop: bool,
        body: impl FnOnce(&mut Self) -> Result<(), ScopeError>,
    ) -> Result<(), ScopeError> {
        let saved = (self.loops, self.breakables);
        if is_loop {
            self.loops += 1;
        }
        self.breakables += 1;
        let result = body(self);
        (self.loops, self.breakables) = saved;
        result
    }

    fn statement(&mut self, statement: &Stmt) -> Result<(), ScopeError> {
        match &statement.kind {
            StmtKind::Expr(expr) => self.expression(expr)?,
            StmtKind::Declare { bindings, .. } => {
                for binding in bindings {
                    if let Some(init) = &binding.init {
                        self.expression(init)?;
                    }
                    // The declared name is resolved like any other occurrence, so that lowering
                    // asks one question rather than two.
                    let id = self
                        .lookup(&binding.name.name)
                        .expect("a declared name was declared");
                    self.declaration_reference(&binding.name, id);
                }
            }
            StmtKind::Function(func) => {
                if let Some(name) = func.name.as_ref() {
                    let id = self
                        .lookup(&name.name)
                        .expect("a declared name was declared");
                    self.declaration_reference(name, id);
                }
                self.function(func, false)?;
            }
            StmtKind::Return(value) => {
                if let Some(value) = value {
                    self.expression(value)?;
                }
            }
            StmtKind::If {
                test,
                consequent,
                alternate,
            } => {
                self.expression(test)?;
                self.statement(consequent)?;
                if let Some(alternate) = alternate {
                    self.statement(alternate)?;
                }
            }
            StmtKind::While { test, body } => {
                self.expression(test)?;
                self.breakable(true, |walker| walker.statement(body))?;
            }
            StmtKind::Block(body) => {
                self.push_block(statement.span);
                let result = self.block_body(body);
                self.blocks.pop();
                result?;
            }
            StmtKind::Switch {
                discriminant,
                cases,
            } => {
                // The discriminant is evaluated before the scope exists, which is not a detail:
                // `switch (x) { case 1: let x = 2; }` reads the outer `x` and would be a dead zone
                // error if the clauses' scope were open around it.
                self.expression(discriminant)?;
                self.push_block(statement.span);
                let result = self.breakable(false, |this| this.switch_body(cases));
                self.blocks.pop();
                result?;
            }
            // Both are early errors rather than runtime ones, and the messages are Node's word for
            // word, because a program that fails to parse in one engine and parses in another is
            // the most confusing kind of incompatibility there is.
            StmtKind::Break => {
                if self.breakables == 0 {
                    return Err(ScopeError {
                        span: statement.span,
                        message: "Illegal break statement".to_owned(),
                    });
                }
            }
            StmtKind::Continue => {
                if self.loops == 0 {
                    return Err(ScopeError {
                        span: statement.span,
                        message: "Illegal continue statement: no surrounding iteration statement"
                            .to_owned(),
                    });
                }
            }
            StmtKind::Empty => {}
        }
        Ok(())
    }

    fn expression(&mut self, expr: &Expr) -> Result<(), ScopeError> {
        match &expr.kind {
            ExprKind::Number(_) | ExprKind::String(_) | ExprKind::Boolean(_) | ExprKind::Null => {}
            ExprKind::Ident(ident) => self.reference(ident),
            ExprKind::This => {
                let current = self.current();
                self.functions[current.0 as usize].uses_this = true;
            }
            ExprKind::Unary { operand, .. } => self.expression(operand)?,
            ExprKind::Update { target, .. } => self.target(target)?,
            ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
                self.expression(left)?;
                self.expression(right)?;
            }
            ExprKind::Assign { target, value, .. } => {
                self.expression(value)?;
                self.target(target)?;
            }
            ExprKind::Conditional {
                test,
                consequent,
                alternate,
            } => {
                self.expression(test)?;
                self.expression(consequent)?;
                self.expression(alternate)?;
            }
            ExprKind::Field { object, .. } => self.expression(object)?,
            ExprKind::Index { object, index } => {
                self.expression(object)?;
                self.expression(index)?;
            }
            ExprKind::Call { callee, arguments } => {
                self.expression(callee)?;
                for argument in arguments {
                    self.expression(argument)?;
                }
            }
            ExprKind::Function(func) => self.function(func, true)?,
        }
        Ok(())
    }

    fn target(&mut self, target: &Target) -> Result<(), ScopeError> {
        match &target.kind {
            TargetKind::Ident(ident) => self.reference(ident),
            TargetKind::Field { object, .. } => self.expression(object)?,
            TargetKind::Index { object, index } => {
                self.expression(object)?;
                self.expression(index)?;
            }
        }
        Ok(())
    }

    /// How a name is bound in the innermost scope only, if it is bound there at all.
    fn declared_in_scope(&self, name: &str) -> Option<BindingKind> {
        let id = *self.blocks.last()?.names.get(name)?;
        Some(self.bindings[id.0 as usize].kind)
    }

    /// Find a name, innermost scope first.
    fn lookup(&self, name: &str) -> Option<BindingId> {
        self.blocks
            .iter()
            .rev()
            .find_map(|scope| scope.names.get(name).copied())
    }

    /// Resolve one identifier occurrence and record what it named.
    fn reference(&mut self, ident: &Ident) {
        let from = self.current();

        let Some(id) = self.lookup(&ident.name) else {
            // Not declared anywhere, so it is a global. `arguments` is the one name where that is
            // the wrong answer inside a function, and the flag is what says so until there is an
            // arguments object to point at.
            if ident.name == "arguments" && from != FunctionId(0) {
                self.functions[from.0 as usize].uses_arguments = true;
            }
            self.references.push(RawReference {
                at: ident.span.start,
                binding: None,
                from,
                tdz: false,
            });
            return;
        };

        let binding = &mut self.bindings[id.0 as usize];
        let crossed = binding.function != from;
        if crossed {
            binding.captured = true;
        }

        // Textually after the declarator, in the same function, is the one case where the dead
        // zone cannot still be open: reaching that point means the declarator ran, and a loop that
        // jumps backwards re-enters the block and re-runs it. That reasoning holds for the
        // statements M0 has and stops holding the day a `switch` can jump past a declaration into
        // a later case, which is where this rule has to become conservative.
        let tdz = binding.kind.has_dead_zone() && (crossed || ident.span.start < binding.span.end);

        // Recorded on the binding here rather than folded back from the references afterwards. The
        // binding is already in hand and already being written to, so it costs nothing, while the
        // fold was a second pass over every identifier in the file.
        binding.needs_dead_zone |= tdz;

        self.references.push(RawReference {
            at: ident.span.start,
            binding: Some(id),
            from,
            tdz,
        });
    }

    /// Record the name in a declaration, which names a binding but never reads it.
    fn declaration_reference(&mut self, ident: &Ident, binding: BindingId) {
        let from = self.current();
        self.references.push(RawReference {
            at: ident.span.start,
            binding: Some(binding),
            from,
            tdz: false,
        });
    }

    /// Assign slots and turn every recorded reference into a resolution.
    ///
    /// This cannot happen during the walk. Whether a binding is captured is only known once every
    /// nested function has been seen, and whether it gets a frame slot or a cell follows from that,
    /// so the walk records what each reference named and this decides where that lives.
    fn finish(mut self) -> Scopes {
        for function in &mut self.functions {
            // Registers zero to `arity` belong to the parameters before a single instruction runs,
            // because that is where the caller put the arguments. A parameter that is captured
            // still arrives in its register and is copied into a cell by the prologue, so its
            // register stays reserved and everything else starts above the whole run. Getting this
            // wrong is invisible until a function captures its first parameter and then every
            // other local reads the wrong slot.
            let mut frame_slots = function.arity;
            let (mut cell_slots, mut parameter) = (0u16, 0u16);
            for id in &function.bindings {
                let binding = &mut self.bindings[id.0 as usize];
                let is_parameter = binding.kind == BindingKind::Param;
                if binding.captured {
                    binding.slot = cell_slots;
                    cell_slots = cell_slots
                        .checked_add(1)
                        .expect("a scope has fewer than sixty five thousand names");
                } else if is_parameter {
                    binding.slot = parameter;
                } else {
                    binding.slot = frame_slots;
                    frame_slots = frame_slots
                        .checked_add(1)
                        .expect("a scope has fewer than sixty five thousand names");
                }
                if is_parameter {
                    parameter += 1;
                }
            }
            function.frame_slots = frame_slots;
            function.cell_slots = cell_slots;
        }

        // How many environments there are between the root and each function, counting its own.
        // Functions are created parents first, so one forward pass is enough.
        let mut depth = vec![0u16; self.functions.len()];
        for index in 0..self.functions.len() {
            let function = &self.functions[index];
            let above = function.parent.map_or(0, |parent| depth[parent.0 as usize]);
            depth[index] = above + u16::from(function.needs_environment());
        }

        let mut references = FxHashMap::default();
        references.reserve(self.references.len());
        for raw in &self.references {
            let resolution = match raw.binding {
                None => Resolution::Global,
                Some(id) => {
                    let binding = &self.bindings[id.0 as usize];
                    if binding.captured {
                        Resolution::Upvalue {
                            hops: depth[raw.from.0 as usize] - depth[binding.function.0 as usize],
                            slot: binding.slot,
                            tdz: raw.tdz,
                        }
                    } else {
                        Resolution::Local {
                            slot: binding.slot,
                            tdz: raw.tdz,
                        }
                    }
                }
            };
            references.insert(
                raw.at,
                Reference {
                    resolution,
                    binding: raw.binding,
                },
            );
        }

        let functions_by_span = self
            .functions
            .iter()
            .enumerate()
            .skip(1)
            .map(|(index, function)| {
                let id = FunctionId(u32::try_from(index).expect("counted above"));
                (function.span.start, id)
            })
            .collect();

        Scopes {
            functions: self.functions,
            bindings: self.bindings,
            references,
            functions_by_span,
        }
    }
}

/// The message every engine uses for a name declared twice, because it ends up in test
/// expectations that were written against those engines.
fn already_declared(name: &Ident) -> ScopeError {
    ScopeError {
        span: name.span,
        message: format!("Identifier '{}' has already been declared", name.name),
    }
}

impl ScopeError {
    /// Point at a different place, for the case where the second declaration is the one the reader
    /// needs to see.
    fn with_span_of(mut self, span: Span) -> Self {
        self.span = span;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{BindingKind, FunctionId, Reference, Resolution, Scopes};
    use crate::ParseError;
    use crate::ast::{Expr, ExprKind, Func, Ident, Stmt, StmtKind, Target, TargetKind};

    /// Parse and analyse, for the cases that are expected to be accepted.
    fn analysed(source: &str) -> Scopes {
        crate::frontend("test.js", source)
            .expect("should parse and resolve")
            .1
    }

    /// The early error a source produces, as a line, a column and a message.
    fn refused(source: &str) -> (u32, u32, String) {
        let error = crate::frontend("test.js", source).expect_err("should be refused");
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

    /// The byte offset of the nth whole word occurrence of a name in the source.
    ///
    /// Whole word, because a test looking for `x` should not find the one inside `max`, and a test
    /// that silently matched the wrong occurrence would assert something true about a reference
    /// nobody meant to check.
    fn offset_of(source: &str, name: &str, nth: usize) -> u32 {
        let bytes = source.as_bytes();
        let word = |byte: Option<&u8>| {
            byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$')
        };

        let mut seen = 0;
        let mut from = 0;
        while let Some(index) = source[from..].find(name) {
            let at = from + index;
            if !word(at.checked_sub(1).and_then(|before| bytes.get(before)))
                && !word(bytes.get(at + name.len()))
            {
                if seen == nth {
                    return u32::try_from(at).expect("test sources are short");
                }
                seen += 1;
            }
            from = at + name.len();
        }
        panic!("there is no occurrence {nth} of {name} in the source");
    }

    /// What the nth occurrence of a name resolved to.
    fn resolved(scopes: &Scopes, source: &str, name: &str, nth: usize) -> Reference {
        scopes.references[&offset_of(source, name, nth)]
    }

    /// Every identifier in the tree that names a variable rather than a property.
    ///
    /// The distinction is the point. `o.x` has an identifier in it that is not a name in any scope,
    /// and a pass that resolved it would be resolving the wrong thing.
    fn variable_idents(body: &[Stmt], out: &mut Vec<Ident>) {
        for statement in body {
            match &statement.kind {
                StmtKind::Expr(expr) => expression_idents(expr, out),
                StmtKind::Declare { bindings, .. } => {
                    for binding in bindings {
                        out.push(binding.name.clone());
                        if let Some(init) = &binding.init {
                            expression_idents(init, out);
                        }
                    }
                }
                StmtKind::Function(func) => {
                    if let Some(name) = &func.name {
                        out.push(name.clone());
                    }
                    function_idents(func, out);
                }
                StmtKind::Return(value) => {
                    if let Some(value) = value {
                        expression_idents(value, out);
                    }
                }
                StmtKind::If {
                    test,
                    consequent,
                    alternate,
                } => {
                    expression_idents(test, out);
                    variable_idents(std::slice::from_ref(consequent), out);
                    if let Some(alternate) = alternate {
                        variable_idents(std::slice::from_ref(alternate), out);
                    }
                }
                StmtKind::While { test, body } => {
                    expression_idents(test, out);
                    variable_idents(std::slice::from_ref(body), out);
                }
                StmtKind::Switch {
                    discriminant,
                    cases,
                } => {
                    expression_idents(discriminant, out);
                    for case in cases {
                        if let Some(test) = &case.test {
                            expression_idents(test, out);
                        }
                        variable_idents(&case.body, out);
                    }
                }
                StmtKind::Block(body) => variable_idents(body, out),
                StmtKind::Break | StmtKind::Continue | StmtKind::Empty => {}
            }
        }
    }

    fn function_idents(func: &Func, out: &mut Vec<Ident>) {
        out.extend(func.params.iter().cloned());
        variable_idents(&func.body, out);
    }

    fn expression_idents(expr: &Expr, out: &mut Vec<Ident>) {
        match &expr.kind {
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Boolean(_)
            | ExprKind::Null
            | ExprKind::This => {}
            ExprKind::Ident(ident) => out.push(ident.clone()),
            ExprKind::Unary { operand, .. } => expression_idents(operand, out),
            ExprKind::Update { target, .. } => target_idents(target, out),
            ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
                expression_idents(left, out);
                expression_idents(right, out);
            }
            ExprKind::Assign { target, value, .. } => {
                target_idents(target, out);
                expression_idents(value, out);
            }
            ExprKind::Conditional {
                test,
                consequent,
                alternate,
            } => {
                expression_idents(test, out);
                expression_idents(consequent, out);
                expression_idents(alternate, out);
            }
            ExprKind::Field { object, .. } => expression_idents(object, out),
            ExprKind::Index { object, index } => {
                expression_idents(object, out);
                expression_idents(index, out);
            }
            ExprKind::Call { callee, arguments } => {
                expression_idents(callee, out);
                for argument in arguments {
                    expression_idents(argument, out);
                }
            }
            ExprKind::Function(func) => {
                if let Some(name) = &func.name {
                    out.push(name.clone());
                }
                function_idents(func, out);
            }
        }
    }

    fn target_idents(target: &Target, out: &mut Vec<Ident>) {
        match &target.kind {
            TargetKind::Ident(ident) => out.push(ident.clone()),
            TargetKind::Field { object, .. } => expression_idents(object, out),
            TargetKind::Index { object, index } => {
                expression_idents(object, out);
                expression_idents(index, out);
            }
        }
    }

    #[test]
    fn a_top_level_name_is_a_frame_slot_and_nothing_is_captured() {
        let source = "const answer = 42; answer;";
        let scopes = analysed(source);

        assert_eq!(scopes.functions().len(), 1);
        assert_eq!(scopes.top_level().frame_slots, 1);
        assert_eq!(scopes.top_level().cell_slots, 0);
        assert!(!scopes.top_level().needs_environment());
        assert!(!scopes.bindings()[0].captured);

        let reference = resolved(&scopes, source, "answer", 1);
        assert_eq!(
            reference.resolution,
            Resolution::Local {
                slot: 0,
                tdz: false
            }
        );
    }

    #[test]
    fn the_top_level_is_a_function_scope_and_not_the_global_object() {
        // Node wraps a CommonJS module in a function and an ES module has its own scope, so top
        // level `var` is a frame slot in everything we actually run. This is the assertion that
        // says so, and it is the one that has to change the day `eval` arrives.
        let source = "var x = 1; x;";
        let scopes = analysed(source);

        assert_eq!(scopes.top_level().frame_slots, 1);
        assert_eq!(
            resolved(&scopes, source, "x", 1).resolution,
            Resolution::Local {
                slot: 0,
                tdz: false
            }
        );
    }

    #[test]
    fn a_name_nothing_declares_is_a_property_of_the_global_object() {
        let source = "console.log(1);";
        let scopes = analysed(source);

        let reference = resolved(&scopes, source, "console", 0);
        assert_eq!(reference.resolution, Resolution::Global);
        assert!(reference.binding.is_none());
    }

    #[test]
    fn a_property_name_is_not_a_variable_and_gets_no_resolution() {
        let source = "const o = 1; o.answer;";
        let scopes = analysed(source);

        // `answer` here is a property name. If it had an entry, the pass would be resolving
        // something that is not a name in any scope.
        assert!(
            !scopes
                .references
                .contains_key(&offset_of(source, "answer", 0))
        );
    }

    #[test]
    fn a_capture_moves_a_binding_out_of_the_frame_and_into_a_cell() {
        let source = "function outer() {
            let captured = 1;
            let plain = 2;
            return function inner() { return captured + plain; };
        }";
        let scopes = analysed(source);

        // `plain` is read from the nested function too, so both are captured. That is the point of
        // the second name: it would be easy to write an analysis that captures everything in a
        // function that captures anything, and the next test is the one that catches that.
        let outer = &scopes.functions()[1];
        assert_eq!(outer.cell_slots, 2);
        assert_eq!(outer.frame_slots, 0);
        assert!(outer.needs_environment());
    }

    #[test]
    fn only_the_names_a_closure_actually_reads_get_cells() {
        let source = "function outer() {
            let captured = 1;
            let plain = 2;
            return function inner() { return captured; } + plain;
        }";
        let scopes = analysed(source);

        let outer = &scopes.functions()[1];
        assert_eq!(outer.cell_slots, 1);
        assert_eq!(outer.frame_slots, 1);

        let captured = scopes
            .bindings()
            .iter()
            .find(|binding| &*binding.name == "captured")
            .expect("the name is declared");
        let plain = scopes
            .bindings()
            .iter()
            .find(|binding| &*binding.name == "plain")
            .expect("the name is declared");
        assert!(captured.captured);
        assert!(!plain.captured);
    }

    #[test]
    fn a_function_that_captures_nothing_needs_no_environment() {
        let source = "function f(a, b) { let c = a + b; return c; }";
        let scopes = analysed(source);

        let f = &scopes.functions()[1];
        assert_eq!(f.frame_slots, 3);
        assert_eq!(f.cell_slots, 0);
        assert!(!f.needs_environment());
    }

    #[test]
    fn parameters_get_the_first_slots_because_the_caller_puts_them_there() {
        let source = "function f(a, b) { let c = 1; return a + b + c; }";
        let scopes = analysed(source);

        for (name, expected) in [("a", 0), ("b", 1), ("c", 2)] {
            let Resolution::Local { slot, .. } = resolved(&scopes, source, name, 1).resolution
            else {
                panic!("{name} should be a frame slot");
            };
            assert_eq!(slot, expected, "{name} is in the wrong slot");
        }
    }

    #[test]
    fn a_hop_counts_environments_and_not_function_boundaries() {
        // `b` captures nothing of its own, so it has no environment and `c` reaching past two
        // function boundaries is still zero hops. An analysis that counted functions would say one
        // here and read the wrong environment at run time.
        let source = "function a() {
            let x = 1;
            return function b() { return function c() { return x; }; };
        }";
        let scopes = analysed(source);

        assert!(!scopes.functions()[2].needs_environment());
        assert_eq!(
            resolved(&scopes, source, "x", 1).resolution,
            Resolution::Upvalue {
                hops: 0,
                slot: 0,
                tdz: true
            }
        );
    }

    #[test]
    fn an_environment_in_the_middle_adds_a_hop() {
        let source = "function a() {
            let x = 1;
            return function b(y) {
                let z = y;
                return function c() { return x + z; };
            };
        }";
        let scopes = analysed(source);

        assert!(scopes.functions()[2].needs_environment());
        assert_eq!(
            resolved(&scopes, source, "x", 1).resolution,
            Resolution::Upvalue {
                hops: 1,
                slot: 0,
                tdz: true
            }
        );
        assert_eq!(
            resolved(&scopes, source, "z", 1).resolution,
            Resolution::Upvalue {
                hops: 0,
                slot: 0,
                tdz: true
            }
        );
    }

    #[test]
    fn a_captured_parameter_leaves_its_register_reserved() {
        // `a` is read from the closure so it lives in a cell, and `b` is not so it lives in a
        // register. The caller still puts the second argument in register one, so `b` has to be
        // register one and not register zero. An analysis that packed the registers down would
        // make every call to this function read its arguments in the wrong order.
        let source = "function f(a, b) { return function g() { return a; } + b; }";
        let scopes = analysed(source);

        let f = &scopes.functions()[1];
        assert_eq!(f.arity, 2);
        assert_eq!(f.frame_slots, 2);
        assert_eq!(f.cell_slots, 1);
        assert_eq!(
            resolved(&scopes, source, "b", 1).resolution,
            Resolution::Local {
                slot: 1,
                tdz: false
            }
        );
    }

    #[test]
    fn the_name_a_function_expression_sees_itself_by_does_not_take_a_parameter_register() {
        let source = "const f = function me(a) { return me(a); };";
        let scopes = analysed(source);

        assert_eq!(
            resolved(&scopes, source, "a", 1).resolution,
            Resolution::Local {
                slot: 0,
                tdz: false
            }
        );
        assert_eq!(
            resolved(&scopes, source, "me", 1).resolution,
            Resolution::Local {
                slot: 1,
                tdz: false
            }
        );
    }

    #[test]
    fn only_a_binding_something_checks_is_marked_for_the_dead_zone() {
        let source = "function f() { let checked; let plain = 1; return function g() { return checked; } + plain; }";
        let scopes = analysed(source);

        let marked = |name: &str| {
            scopes
                .bindings()
                .iter()
                .find(|binding| &*binding.name == name)
                .expect("the name is declared")
                .needs_dead_zone
        };
        assert!(marked("checked"));
        assert!(!marked("plain"));
    }

    #[test]
    fn a_function_can_be_found_by_where_it_was_written() {
        let source = "function outer() { return function inner() { return 1; }; }";
        let scopes = analysed(source);

        let outer = scopes.functions()[1].span;
        let inner = scopes.functions()[2].span;
        assert_eq!(scopes.function_of(outer), Some(FunctionId(1)));
        assert_eq!(scopes.function_of(inner), Some(FunctionId(2)));
        assert_eq!(scopes.function_of(crate::ast::Span::new(9999, 9999)), None);
    }

    #[test]
    fn var_hoists_out_of_a_block_and_let_does_not() {
        let hoisted = "function f() { { var x = 1; } return x; }";
        let scopes = analysed(hoisted);
        assert_eq!(scopes.functions()[1].frame_slots, 1);
        assert_eq!(
            resolved(&scopes, hoisted, "x", 1).resolution,
            Resolution::Local {
                slot: 0,
                tdz: false
            }
        );

        let blocked = "function f() { { let x = 1; } return x; }";
        let scopes = analysed(blocked);
        assert_eq!(
            resolved(&scopes, blocked, "x", 1).resolution,
            Resolution::Global
        );
    }

    #[test]
    fn var_written_twice_is_one_binding() {
        let source = "function f() { var x = 1; var x = 2; return x; }";
        let scopes = analysed(source);

        assert_eq!(scopes.functions()[1].bindings.len(), 1);
        assert_eq!(scopes.functions()[1].frame_slots, 1);
    }

    #[test]
    fn a_var_never_needs_a_dead_zone_check_and_a_let_read_early_does() {
        let hoisted = "function f() { x; var x = 1; }";
        let scopes = analysed(hoisted);
        assert_eq!(
            resolved(&scopes, hoisted, "x", 0).resolution,
            Resolution::Local {
                slot: 0,
                tdz: false
            }
        );

        let early = "function f() { x; let x = 1; }";
        let scopes = analysed(early);
        assert_eq!(
            resolved(&scopes, early, "x", 0).resolution,
            Resolution::Local { slot: 0, tdz: true }
        );
    }

    #[test]
    fn a_let_read_after_its_declaration_in_the_same_function_needs_no_check() {
        let source = "function f() { let x = 1; return x; }";
        let scopes = analysed(source);

        assert_eq!(
            resolved(&scopes, source, "x", 1).resolution,
            Resolution::Local {
                slot: 0,
                tdz: false
            }
        );
    }

    #[test]
    fn a_let_initialised_from_itself_is_still_in_its_own_dead_zone() {
        // The declarator has not finished running when the initialiser reads the name, which is
        // why the comparison is against the end of the declarator and not the start of it.
        let source = "function f() { let x = x; }";
        let scopes = analysed(source);

        assert_eq!(
            resolved(&scopes, source, "x", 1).resolution,
            Resolution::Local { slot: 0, tdz: true }
        );
    }

    #[test]
    fn a_let_read_from_a_closure_is_always_checked_wherever_it_was_written() {
        // Textual order says nothing here, because `g` can be called before the declaration runs.
        let source = "function f() { function g() { return x; } let x = 1; return g; }";
        let scopes = analysed(source);

        assert_eq!(
            resolved(&scopes, source, "x", 0).resolution,
            Resolution::Upvalue {
                hops: 0,
                slot: 0,
                tdz: true
            }
        );
    }

    #[test]
    fn a_const_is_marked_so_that_lowering_can_make_the_write_throw() {
        // Assignment to a constant is a run time TypeError and not an early error, so this parses
        // and the information lowering needs comes back on the binding.
        let source = "const x = 1; x = 2;";
        let scopes = analysed(source);

        let reference = resolved(&scopes, source, "x", 1);
        let binding = reference.binding.expect("the name is declared");
        assert!(scopes.binding(binding).kind.is_constant());
    }

    #[test]
    fn this_and_arguments_are_recorded_only_on_the_function_that_mentions_them() {
        let source = "function f() { return this; } function g() { return arguments; }";
        let scopes = analysed(source);

        assert!(!scopes.top_level().uses_this);
        assert!(!scopes.top_level().uses_arguments);
        assert!(scopes.functions()[1].uses_this);
        assert!(!scopes.functions()[1].uses_arguments);
        assert!(!scopes.functions()[2].uses_this);
        assert!(scopes.functions()[2].uses_arguments);
    }

    #[test]
    fn a_named_function_expression_can_see_itself_and_nobody_else_can() {
        let source = "const f = function me() { return me; }; me;";
        let scopes = analysed(source);

        let inside = resolved(&scopes, source, "me", 1);
        let binding = inside.binding.expect("the name is bound inside");
        assert_eq!(scopes.binding(binding).kind, BindingKind::Function);
        assert_eq!(scopes.binding(binding).function, FunctionId(1));

        assert_eq!(
            resolved(&scopes, source, "me", 2).resolution,
            Resolution::Global
        );
    }

    #[test]
    fn every_variable_in_the_tree_has_an_answer() {
        // The contract the whole keying scheme rests on: lowering asks about an identifier it
        // found in the tree and always gets something back.
        let source = "const base = 2;
            function scale(factor) {
                let total = 0;
                while (total < factor) { total = total + base; }
                if (total > 10) { return total; } else { return -total; }
            }
            var doubled = scale(base);
            doubled.toFixed(1);
            const twice = function again(n) { return n > 0 ? again(n - 1) : base; };
            twice(3);";
        let module = crate::parse("test.js", source).expect("should parse and resolve");

        let mut idents = Vec::new();
        variable_idents(&module.ast.body, &mut idents);
        assert!(idents.len() > 20, "the sample should be worth the walk");

        for ident in &idents {
            assert!(
                module.scopes.reference(ident).is_some(),
                "{} at {:?} was not resolved",
                ident.name,
                ident.span
            );
        }
        assert_eq!(module.scopes.reference_count(), idents.len());
    }

    #[test]
    fn a_name_declared_twice_in_one_scope_is_refused() {
        let (line, column, message) = refused("let x = 1;\nlet x = 2;");
        assert_eq!((line, column), (2, 5));
        assert_eq!(message, "Identifier 'x' has already been declared");
    }

    #[test]
    fn a_var_and_a_let_of_the_same_name_collide_in_both_orders() {
        assert_eq!(
            refused("var x; let x;").2,
            "Identifier 'x' has already been declared"
        );
        assert_eq!(
            refused("let x; var x;").2,
            "Identifier 'x' has already been declared"
        );
    }

    #[test]
    fn a_var_that_hoists_past_a_let_of_the_same_name_is_refused() {
        // The two are in different blocks as written, and the `var` does not stay in its block, so
        // they end up in the same scope after hoisting. Span containment is what notices.
        let (line, column, _) = refused("{ let x = 1; { var x = 2; } }");
        assert_eq!((line, column), (1, 20));

        // Neither of these ever share a scope, so both are fine.
        analysed("{ let x = 1; } var x = 2;");
        analysed("var x = 1; { let x = 2; }");
    }

    #[test]
    fn a_parameter_and_a_let_of_the_same_name_collide() {
        assert_eq!(
            refused("function f(a) { let a = 1; }").2,
            "Identifier 'a' has already been declared"
        );

        // A `var` repeating a parameter is legal and has been forever.
        analysed("function f(a) { var a = 1; return a; }");
    }

    #[test]
    fn a_repeated_parameter_is_refused_only_in_strict_mode() {
        let sloppy = analysed("function f(a, a) { return a; }");
        assert_eq!(sloppy.functions()[1].frame_slots, 2);

        assert_eq!(
            refused("'use strict';\nfunction f(a, a) { return a; }").2,
            "Duplicate parameter name not allowed in this context"
        );
    }

    #[test]
    fn a_const_with_no_initialiser_is_refused() {
        // oxc reports this one itself, so the program is refused before this pass runs. The check
        // stays here anyway, because the rule belongs to this pass and the comment in `parse` is
        // explicit that the parser's diagnostics are not a complete list of the early errors. Both
        // paths word it the same way, which is why this asserts on the message and not on which
        // variant carried it.
        let error = crate::parse("test.js", "const x;").expect_err("should be refused");
        assert!(
            error
                .to_string()
                .contains("Missing initializer in const declaration"),
            "unexpected message: {error}"
        );
    }

    #[test]
    fn break_and_continue_need_something_to_leave() {
        // Both messages are Node's word for word. A program that is a syntax error in one engine
        // and a running program in another is the worst kind of incompatibility, and a program
        // that is a syntax error in both but says so differently is the second worst, because it
        // is what ends up in somebody's test expectations.
        assert_eq!(refused("break;").2, "Illegal break statement");
        assert_eq!(
            refused("continue;").2,
            "Illegal continue statement: no surrounding iteration statement"
        );

        // A switch is something to break out of and is not something to continue.
        analysed("switch (1) { case 1: break; }");
        assert_eq!(
            refused("switch (1) { case 1: continue; }").2,
            "Illegal continue statement: no surrounding iteration statement"
        );

        // A loop is both, and being past the loop is not being in it.
        analysed("while (0) { break; }");
        analysed("while (0) { continue; }");
        assert_eq!(refused("while (0) {} break;").2, "Illegal break statement");

        // A function is a wall. The loop outside it is not a loop this `break` can see.
        assert_eq!(
            refused("while (0) { function f() { break; } }").2,
            "Illegal break statement"
        );
    }

    #[test]
    fn every_clause_of_a_switch_shares_one_scope_and_it_opens_before_the_first_test() {
        // The intuitive reading is that each clause is its own scope, and it is not what the
        // standard says. One `let` in the last clause is in scope for all of them, so the `x` the
        // first clause reads is the one being declared three lines down and reading it early is a
        // dead zone error rather than a read of the outer `x`.
        let source = "let x = 1; switch (0) { case 0: x; break; case 1: let x = 2; }";
        let scopes = analysed(source);
        let inner = resolved(&scopes, source, "x", 1);
        let binding = inner.binding.expect("the read resolves to a binding");
        assert!(
            scopes.bindings()[binding.0 as usize].needs_dead_zone,
            "the read in the first clause is a read of the let three clauses down"
        );

        // The discriminant is evaluated before that scope exists, so it reads the outer name, and
        // the case tests are evaluated inside it, so they do not.
        let source = "let y = 1; switch (y) { case y: let y = 2; }";
        let scopes = analysed(source);
        let discriminant = resolved(&scopes, source, "y", 1);
        let test = resolved(&scopes, source, "y", 2);
        assert_ne!(
            discriminant.binding, test.binding,
            "the discriminant is outside the clauses' scope and the case test is inside it"
        );
    }

    #[test]
    fn a_var_written_in_a_clause_hoists_out_of_the_switch() {
        let source = "switch (0) { case 0: var x = 1; } x;";
        let scopes = analysed(source);
        assert_eq!(
            resolved(&scopes, source, "x", 0).binding,
            resolved(&scopes, source, "x", 1).binding,
            "the same binding, because a var does not stop at the switch's braces"
        );
    }

    #[test]
    fn a_block_scope_shadows_and_then_gives_the_name_back() {
        let source = "let x = 1; { let x = 2; x; } x;";
        let scopes = analysed(source);

        let inner = resolved(&scopes, source, "x", 2);
        let outer = resolved(&scopes, source, "x", 3);
        assert_ne!(inner.binding, outer.binding);
        assert_eq!(scopes.top_level().frame_slots, 2);
    }
}
