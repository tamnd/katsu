//! Lowering: the tree becomes bytecode.
//!
//! This is the last pass in the frontend. It walks the tree the adapter built, asks scope analysis
//! where every name lives, and produces the `FunctionBlueprint` in `katsu-ir` that the interpreter
//! runs. Nothing after this point looks at a syntax tree.
//!
//! ## Registers
//!
//! Scope analysis has already assigned a frame slot to every declared name that no closure captures,
//! and reserved registers zero to the arity for the parameters, because that is where the calling
//! convention puts the arguments. Those slots are live for the whole call. Lowering's own
//! temporaries start above them and are allocated with a stack discipline: an expression notes the
//! high water mark before it evaluates its operands and drops back to it afterwards, so a function
//! needs as many registers as its deepest expression and not as many as it has subexpressions.
//!
//! A temporary is freed before the destination of the instruction that consumes it is allocated, so
//! `a * b + c` reuses one register for both products rather than growing the frame. That is safe
//! because a three address instruction reads all of its operands before it writes its destination.
//!
//! An expression that is already in a register does not get copied into another one. Reading a local
//! variable yields the variable's own slot, which is what makes `a + b` a single `add r2, r0, r1`
//! rather than two moves and an add. The hazard that comes with it is real and is handled in one
//! place: if the other operand of an expression can assign to a variable, then the first operand is
//! copied into a temporary before the second is evaluated, because `x + (x = 2)` has to add the old
//! `x` to the new one. `writes_a_variable` is what decides, and it is deliberately conservative.
//!
//! ## Control flow
//!
//! Jumps are emitted with a placeholder target and patched once the target is known. A jump that is
//! never patched points at `u32::MAX`, and `FunctionBlueprint::verify` refuses it, so the failure
//! mode of forgetting to patch is a test failure with a message rather than a wild jump at run time.
//!
//! ## What is not here yet
//!
//! Only the M0 subset lowers, which is what the adapter accepts, minus two things it accepts and
//! this refuses by name: `delete` of anything that is not a property, which needs an opcode that can
//! delete a global binding, and `arguments` inside a function, which needs an arguments object to
//! resolve to. Both are `NotLowered` rather than quietly wrong bytecode.
//!
//! Block scoped names do not share slots between sibling blocks, so two blocks that each declare a
//! `let` use two frame slots even though their lifetimes cannot overlap. Fixing it needs scope
//! analysis to record which block a binding belongs to, and the cost until then is frame size on
//! functions with many blocks rather than anything incorrect.

use katsu_ir::{
    BlueprintIndex, CacheIndex, CodeOffset, ConstIndex, FunctionBlueprint, Op, Register,
};

use crate::ast::{
    AssignOp, BinaryOp, Binding, DeclKind, Expr, ExprKind, Func, Ident, LogicalOp, Module, Span,
    Stmt, StmtKind, Target, TargetKind, UnaryOp, UpdateOp,
};
use crate::scope::{BindingKind, FunctionId, Reference, Resolution, Scopes};

/// A construct the tree can hold and lowering has no bytecode for.
///
/// Distinct from the adapter's refusal, which is about syntax we have no tree shape for. This one is
/// about a shape we have and an opcode we do not, which is a different work list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{construct} is not lowered yet")]
pub struct LowerError {
    /// Where the construct was written.
    pub span: Span,
    /// What we hit, named the way a JavaScript programmer would name it.
    pub construct: &'static str,
}

/// The target a jump is emitted with before anybody knows where it goes.
///
/// Out of range on purpose. A jump left unpatched fails verification with a message naming the
/// instruction, which is the loudest available way to find a missing patch.
const UNPATCHED: CodeOffset = CodeOffset(u32::MAX);

/// Lower a whole module into the blueprint for its top level.
///
/// The functions written inside it are lowered as part of it, because a blueprint owns the
/// blueprints of the functions written inside it and one module is one self contained tree.
pub fn lower(module: &Module, scopes: &Scopes) -> Result<FunctionBlueprint, LowerError> {
    let end = module.body.last().map_or(0, |statement| statement.span.end);
    let mut lowerer = Lowerer::new(scopes, FunctionId(0), String::new(), Span::new(0, end));
    lowerer.new_context(0);
    lowerer.hoisted_vars(0);
    lowerer.body(&module.body)?;
    Ok(lowerer.finish())
}

/// Lower one function, which scope analysis has already seen.
fn lower_function(scopes: &Scopes, func: &Func) -> Result<FunctionBlueprint, LowerError> {
    let function = scopes
        .function_of(func.span)
        .expect("scope analysis saw every function in the tree");
    let name = func
        .name
        .as_ref()
        .map_or_else(String::new, |name| name.name.clone());

    let mut lowerer = Lowerer::new(scopes, function, name, func.span);
    lowerer.prologue(func);
    lowerer.body(&func.body)?;
    Ok(lowerer.finish())
}

/// Which checks a store has to make before it writes.
///
/// The three cases are genuinely different and collapsing any two of them produces a program that
/// runs and is wrong, so they are named rather than passed as a pair of booleans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreKind {
    /// The store a declaration performs, which is what takes a binding out of its dead zone. Neither
    /// the dead zone check nor the constant check applies, because `const x = 1;` is the write that
    /// makes `x` a constant rather than a violation of it.
    Initialise,
    /// An ordinary assignment, which checks both.
    Assign,
    /// The write half of a read modify write, where the read already checked the dead zone and a
    /// constant is still a constant.
    Rewrite,
}

/// Where a value can be read from and written to, worked out once.
///
/// Built before the value is evaluated, so that `o.x += f()` evaluates `o` once. The registers in
/// the member cases are already pinned against anything the value expression might do to them.
#[derive(Clone, Copy, Debug)]
enum Place {
    /// A frame slot in the function doing the writing.
    Local {
        slot: Register,
        tdz: bool,
        constant: bool,
        name: Option<ConstIndex>,
    },
    /// A cell some number of environments up the chain.
    Cell {
        hops: u16,
        slot: u16,
        tdz: bool,
        constant: bool,
        name: Option<ConstIndex>,
    },
    /// A property of the global object.
    Global { name: ConstIndex },
    /// A property with a name known at compile time.
    Field { object: Register, key: ConstIndex },
    /// A property with a key computed at run time.
    Index { object: Register, index: Register },
}

struct Lowerer<'a> {
    scopes: &'a Scopes,
    function: FunctionId,
    blueprint: FunctionBlueprint,
    /// The first register that is not a declared name, which is where temporaries start.
    first_temp: u16,
    /// The next free temporary.
    next_temp: u16,
    /// The furthest instruction any jump has been patched to point at.
    ///
    /// A jump to the instruction after the last one is legal while lowering is in progress and is
    /// how an `if` with no else leaves the end of the body, so the epilogue has to know that
    /// something is still pointing at the end even when the last instruction is a return.
    max_target: u32,
    /// Where the function ends, which is the position the epilogue is attributed to.
    end: u32,
}

impl<'a> Lowerer<'a> {
    fn new(scopes: &'a Scopes, function: FunctionId, name: String, span: Span) -> Self {
        let scope = scopes.function(function);
        let blueprint = FunctionBlueprint {
            name,
            source_offset: span.start,
            frame_size: scope.frame_slots,
            arity: scope.arity,
            cell_slots: scope.cell_slots,
            strict: scope.strict,
            ..FunctionBlueprint::default()
        };
        Self {
            scopes,
            function,
            blueprint,
            first_temp: scope.frame_slots,
            next_temp: scope.frame_slots,
            max_target: 0,
            end: span.end,
        }
    }

    /// Allocate the environment this function's cells live in, if it has any.
    ///
    /// The top level needs one as much as a function does, because a function written at the top
    /// level can capture a name declared there.
    fn new_context(&mut self, at: u32) {
        let scope = self.scopes.function(self.function);
        if scope.needs_environment() {
            let size = scope.cell_slots;
            self.emit(at, Op::NewContext { size });
        }
    }

    /// Everything that has to happen before the first statement of a function body runs.
    fn prologue(&mut self, func: &Func) {
        let scopes = self.scopes;
        let at = func.span.start;
        self.new_context(at);

        // A captured parameter still arrives in the register the caller filled, and moves into its
        // cell here. An uncaptured one is already where it belongs and costs nothing.
        for (index, param) in func.params.iter().enumerate() {
            let src =
                Register(u16::try_from(index).expect("scope analysis counted the parameters"));
            match self.reference(param).resolution {
                Resolution::Upvalue { hops, slot, .. } => {
                    self.emit(param.span.start, Op::StoreUpvalue { hops, slot, src });
                }
                Resolution::Local { slot, .. } => {
                    debug_assert_eq!(
                        Register(slot),
                        src,
                        "an uncaptured parameter has to be in the register the caller filled"
                    );
                }
                Resolution::Global => unreachable!("a parameter is declared"),
            }
        }

        // The name a function expression sees itself by is bound to the closure that is running,
        // which is not what the variable the closure was assigned to holds now.
        if let Some(name) = func.name.as_ref() {
            let reference = self.reference(name);
            let own = reference
                .binding
                .is_some_and(|id| scopes.binding(id).function == self.function);
            if own {
                let mark = self.next_temp;
                let place = self.ident_place(name);
                let dst =
                    direct_register(place, StoreKind::Initialise).unwrap_or_else(|| self.alloc());
                self.emit(name.span.start, Op::LoadClosure { dst });
                self.store(name.span.start, place, dst, StoreKind::Initialise);
                self.release(mark);
            }
        }

        self.hoisted_vars(at);
    }

    /// Give every `var` in this function undefined, wherever in the body it was written.
    ///
    /// A `var` read before its declaration runs is undefined rather than an error, so the value has
    /// to be there before the first statement. A definite assignment analysis could prove most of
    /// these dead, and that is worth doing when there is an interpreter to measure it against.
    fn hoisted_vars(&mut self, at: u32) {
        let scopes = self.scopes;
        let mark = self.next_temp;
        let mut undefined = None;

        for id in &scopes.function(self.function).bindings {
            let binding = scopes.binding(*id);
            if binding.kind != BindingKind::Var {
                continue;
            }
            if binding.captured {
                let src = if let Some(register) = undefined {
                    register
                } else {
                    let register = self.alloc();
                    self.emit(at, Op::LoadUndefined { dst: register });
                    undefined = Some(register);
                    register
                };
                self.emit(
                    at,
                    Op::StoreUpvalue {
                        hops: 0,
                        slot: binding.slot,
                        src,
                    },
                );
            } else {
                self.emit(
                    at,
                    Op::LoadUndefined {
                        dst: Register(binding.slot),
                    },
                );
            }
        }

        self.release(mark);
    }

    /// Close the function off so that control can never run past the end of the code.
    fn finish(mut self) -> FunctionBlueprint {
        let terminated = self
            .blueprint
            .code
            .last()
            .is_some_and(|op| op.is_terminator());
        let open = self.max_target as usize >= self.blueprint.code.len();

        // A body that ends in a return still needs the implicit one when a jump points past it,
        // which is what `if (c) { return 1; } else { return 2; }` produces.
        if !terminated || open {
            let at = self.end;
            let register = self.alloc();
            self.emit(at, Op::LoadUndefined { dst: register });
            self.emit(at, Op::Return { src: register });
        }

        self.blueprint
    }

    // Emitting.

    fn emit(&mut self, at: u32, op: Op) -> usize {
        let index = self.blueprint.code.len();
        self.blueprint.positions.record(index, at);
        self.blueprint.code.push(op);
        index
    }

    fn here(&self) -> CodeOffset {
        CodeOffset(u32::try_from(self.blueprint.code.len()).expect("a function fits in u32 code"))
    }

    /// Point a jump emitted earlier at the next instruction to be emitted.
    fn patch(&mut self, jump: usize) {
        let target = self.here();
        self.blueprint.code[jump].set_jump_target(target);
        self.max_target = self.max_target.max(target.0);
    }

    fn cache(&mut self) -> CacheIndex {
        let index = self.blueprint.cache_slots;
        self.blueprint.cache_slots += 1;
        CacheIndex(index)
    }

    fn constant(&mut self, text: &str) -> ConstIndex {
        self.blueprint.constants.string(text)
    }

    // Registers.

    fn alloc(&mut self) -> Register {
        let register = Register(self.next_temp);
        self.next_temp = self
            .next_temp
            .checked_add(1)
            .expect("a function fits in sixty five thousand registers");
        self.blueprint.frame_size = self.blueprint.frame_size.max(self.next_temp);
        register
    }

    fn release(&mut self, mark: u16) {
        self.next_temp = mark;
    }

    fn destination(&mut self, dst: Option<Register>) -> Register {
        dst.unwrap_or_else(|| self.alloc())
    }

    /// Copy a value out of a variable's own slot if evaluating something else could overwrite it.
    fn pin(&mut self, at: u32, register: Register, threatened: bool) -> Register {
        if threatened && register.0 < self.first_temp {
            let copy = self.alloc();
            self.emit(
                at,
                Op::Move {
                    dst: copy,
                    src: register,
                },
            );
            copy
        } else {
            register
        }
    }

    /// Give back the temporaries an assignment needed, keeping the value it produced.
    ///
    /// Without a destination the value stays where it is and everything above it is freed, which
    /// costs nothing at run time. The caller releases to its own mark when it is done and that mark
    /// is at or below this one, so holding a register here does not leak.
    ///
    /// With a destination the move reads a register that has just been released, which is safe
    /// because nothing is allocated between the release and the move.
    fn settle(&mut self, at: u32, value: Register, mark: u16, dst: Option<Register>) -> Register {
        let Some(dst) = dst else {
            self.next_temp = if value.0 < mark { mark } else { value.0 + 1 };
            return value;
        };

        self.release(mark);
        if dst != value {
            self.emit(at, Op::Move { dst, src: value });
        }
        dst
    }

    // Names.

    fn reference(&self, ident: &Ident) -> Reference {
        self.scopes
            .reference(ident)
            .expect("scope analysis resolved every identifier in the tree")
    }

    fn ident_place(&mut self, ident: &Ident) -> Place {
        let reference = self.reference(ident);
        let constant = reference
            .binding
            .is_some_and(|id| self.scopes.binding(id).kind.is_constant());
        match reference.resolution {
            Resolution::Local { slot, tdz } => {
                let name = tdz.then(|| self.constant(&ident.name));
                Place::Local {
                    slot: Register(slot),
                    tdz,
                    constant,
                    name,
                }
            }
            Resolution::Upvalue { hops, slot, tdz } => {
                let name = tdz.then(|| self.constant(&ident.name));
                Place::Cell {
                    hops,
                    slot,
                    tdz,
                    constant,
                    name,
                }
            }
            Resolution::Global => Place::Global {
                name: self.constant(&ident.name),
            },
        }
    }

    fn load(&mut self, at: u32, place: Place, dst: Option<Register>) -> Register {
        match place {
            Place::Local {
                slot, tdz, name, ..
            } => {
                if let Some(name) = name.filter(|_| tdz) {
                    self.emit(at, Op::ThrowIfUninitialized { src: slot, name });
                }
                match dst {
                    Some(dst) if dst != slot => {
                        self.emit(at, Op::Move { dst, src: slot });
                        dst
                    }
                    Some(dst) => dst,
                    None => slot,
                }
            }
            Place::Cell {
                hops,
                slot,
                tdz,
                name,
                ..
            } => {
                let dst = self.destination(dst);
                self.emit(at, Op::LoadUpvalue { dst, hops, slot });
                if let Some(name) = name.filter(|_| tdz) {
                    self.emit(at, Op::ThrowIfUninitialized { src: dst, name });
                }
                dst
            }
            Place::Global { name } => {
                let dst = self.destination(dst);
                let cache = self.cache();
                self.emit(at, Op::LoadGlobal { dst, name, cache });
                dst
            }
            Place::Field { object, key } => {
                let dst = self.destination(dst);
                let cache = self.cache();
                self.emit(
                    at,
                    Op::GetProp {
                        dst,
                        obj: object,
                        key,
                        cache,
                    },
                );
                dst
            }
            Place::Index { object, index } => {
                let dst = self.destination(dst);
                let cache = self.cache();
                self.emit(
                    at,
                    Op::GetIndex {
                        dst,
                        obj: object,
                        index,
                        cache,
                    },
                );
                dst
            }
        }
    }

    fn store(&mut self, at: u32, place: Place, src: Register, kind: StoreKind) {
        let checked = kind != StoreKind::Initialise;
        match place {
            Place::Local {
                slot,
                tdz,
                constant,
                name,
            } => {
                if constant && checked {
                    self.emit(at, Op::ThrowConstAssignment);
                    return;
                }
                if kind == StoreKind::Assign
                    && tdz
                    && let Some(name) = name
                {
                    self.emit(at, Op::ThrowIfUninitialized { src: slot, name });
                }
                if src != slot {
                    self.emit(at, Op::Move { dst: slot, src });
                }
            }
            Place::Cell {
                hops,
                slot,
                tdz,
                constant,
                name,
            } => {
                if constant && checked {
                    self.emit(at, Op::ThrowConstAssignment);
                    return;
                }
                if kind == StoreKind::Assign
                    && tdz
                    && let Some(name) = name
                {
                    let mark = self.next_temp;
                    let current = self.alloc();
                    self.emit(
                        at,
                        Op::LoadUpvalue {
                            dst: current,
                            hops,
                            slot,
                        },
                    );
                    self.emit(at, Op::ThrowIfUninitialized { src: current, name });
                    self.release(mark);
                }
                self.emit(at, Op::StoreUpvalue { hops, slot, src });
            }
            Place::Global { name } => {
                let cache = self.cache();
                self.emit(at, Op::StoreGlobal { name, src, cache });
            }
            Place::Field { object, key } => {
                let cache = self.cache();
                self.emit(
                    at,
                    Op::SetProp {
                        obj: object,
                        key,
                        value: src,
                        cache,
                    },
                );
            }
            Place::Index { object, index } => {
                let cache = self.cache();
                self.emit(
                    at,
                    Op::SetIndex {
                        obj: object,
                        index,
                        value: src,
                        cache,
                    },
                );
            }
        }
    }

    /// Work out where an assignment target lives, evaluating its subexpressions once.
    ///
    /// `later` is whatever will be evaluated between here and the store, which is what decides
    /// whether the object register has to be copied out of a variable's slot first.
    fn place(&mut self, target: &Target, later: Option<&Expr>) -> Result<Place, LowerError> {
        let threatened = later.is_some_and(writes_a_variable);
        match &target.kind {
            TargetKind::Ident(ident) => Ok(self.ident_place(ident)),
            TargetKind::Field { object, name } => {
                let register = self.expr(object)?;
                let object = self.pin(target.span.start, register, threatened);
                let key = self.constant(&name.name);
                Ok(Place::Field { object, key })
            }
            TargetKind::Index { object, index } => {
                let register = self.expr(object)?;
                let object = self.pin(
                    target.span.start,
                    register,
                    threatened || writes_a_variable(index),
                );
                let register = self.expr(index)?;
                let index = self.pin(target.span.start, register, threatened);
                Ok(Place::Index { object, index })
            }
        }
    }

    // Statements.

    /// Enter a scope: create its bindings, then run its statements.
    fn body(&mut self, body: &[Stmt]) -> Result<(), LowerError> {
        self.scope_prelude(body)?;
        for statement in body {
            self.statement(statement)?;
        }
        Ok(())
    }

    /// Everything a scope owes its statements before the first of them runs.
    ///
    /// Lexical bindings get the hole, so that a read before the declaration finds something that is
    /// not a value, and then function declarations get their closures, so that a function can be
    /// called from above where it was written. That order is the one the language specifies and it
    /// is also the only one that works, since a function declared here can capture a `let` declared
    /// below it.
    fn scope_prelude(&mut self, body: &[Stmt]) -> Result<(), LowerError> {
        for statement in body {
            let StmtKind::Declare { kind, bindings } = &statement.kind else {
                continue;
            };
            if *kind == DeclKind::Var {
                continue;
            }
            for binding in bindings {
                self.dead_zone_hole(binding);
            }
        }

        for statement in body {
            let StmtKind::Function(func) = &statement.kind else {
                continue;
            };
            let Some(name) = func.name.as_ref() else {
                return Err(LowerError {
                    span: statement.span,
                    construct: "a function declaration with no name",
                });
            };

            let blueprint = self.nested(func)?;
            let mark = self.next_temp;
            let place = self.ident_place(name);
            let dst = direct_register(place, StoreKind::Initialise).unwrap_or_else(|| self.alloc());
            self.emit(statement.span.start, Op::NewClosure { dst, blueprint });
            self.store(statement.span.start, place, dst, StoreKind::Initialise);
            self.release(mark);
        }

        Ok(())
    }

    /// Write the hole into a `let` or `const` that something reads before its declaration runs.
    fn dead_zone_hole(&mut self, binding: &Binding) {
        let reference = self.reference(&binding.name);
        let checked = reference
            .binding
            .is_some_and(|id| self.scopes.binding(id).needs_dead_zone);
        if !checked {
            return;
        }

        let at = binding.name.span.start;
        match reference.resolution {
            Resolution::Local { slot, .. } => {
                self.emit(
                    at,
                    Op::LoadUninitialized {
                        dst: Register(slot),
                    },
                );
            }
            Resolution::Upvalue { hops, slot, .. } => {
                let mark = self.next_temp;
                let hole = self.alloc();
                self.emit(at, Op::LoadUninitialized { dst: hole });
                self.emit(
                    at,
                    Op::StoreUpvalue {
                        hops,
                        slot,
                        src: hole,
                    },
                );
                self.release(mark);
            }
            Resolution::Global => unreachable!("a lexical declaration is never a global"),
        }
    }

    fn statement(&mut self, statement: &Stmt) -> Result<(), LowerError> {
        let at = statement.span.start;
        match &statement.kind {
            StmtKind::Expr(expr) => {
                let mark = self.next_temp;
                self.expr(expr)?;
                self.release(mark);
            }
            StmtKind::Declare { kind, bindings } => {
                for binding in bindings {
                    self.declare(*kind, binding)?;
                }
            }
            // The closure was created when the scope was entered, which is what makes a function
            // callable from above the line it was written on.
            StmtKind::Function(_) | StmtKind::Empty => {}
            StmtKind::Return(value) => {
                let mark = self.next_temp;
                let src = if let Some(value) = value {
                    self.expr(value)?
                } else {
                    let register = self.alloc();
                    self.emit(at, Op::LoadUndefined { dst: register });
                    register
                };
                self.emit(at, Op::Return { src });
                self.release(mark);
            }
            StmtKind::If {
                test,
                consequent,
                alternate,
            } => {
                let mark = self.next_temp;
                let cond = self.expr(test)?;
                self.release(mark);
                let branch = self.emit(
                    at,
                    Op::JumpIfFalse {
                        cond,
                        target: UNPATCHED,
                    },
                );

                self.statement(consequent)?;
                match alternate {
                    Some(alternate) => {
                        let over = self.emit(at, Op::Jump { target: UNPATCHED });
                        self.patch(branch);
                        self.statement(alternate)?;
                        self.patch(over);
                    }
                    None => self.patch(branch),
                }
            }
            StmtKind::While { test, body } => {
                let top = self.here();
                let mark = self.next_temp;
                let cond = self.expr(test)?;
                self.release(mark);
                let exit = self.emit(
                    at,
                    Op::JumpIfFalse {
                        cond,
                        target: UNPATCHED,
                    },
                );

                self.statement(body)?;
                let profile = self.cache();
                self.emit(
                    at,
                    Op::LoopBackEdge {
                        target: top,
                        profile,
                    },
                );
                self.patch(exit);
            }
            StmtKind::Block(body) => self.body(body)?,
        }
        Ok(())
    }

    fn declare(&mut self, kind: DeclKind, binding: &Binding) -> Result<(), LowerError> {
        let at = binding.span.start;
        let mark = self.next_temp;
        let place = self.ident_place(&binding.name);

        match &binding.init {
            Some(init) => {
                let hint = direct_register(place, StoreKind::Initialise);
                let src = self.expr_to(init, hint)?;
                self.store(at, place, src, StoreKind::Initialise);
            }
            // `let x;` and `const x;` give the binding undefined when the declaration runs, which is
            // what takes it out of its dead zone. A `var` already has undefined from the prologue.
            None if kind != DeclKind::Var => {
                let dst =
                    direct_register(place, StoreKind::Initialise).unwrap_or_else(|| self.alloc());
                self.emit(at, Op::LoadUndefined { dst });
                self.store(at, place, dst, StoreKind::Initialise);
            }
            None => {}
        }

        self.release(mark);
        Ok(())
    }

    // Expressions.

    fn expr(&mut self, expr: &Expr) -> Result<Register, LowerError> {
        self.expr_to(expr, None)
    }

    /// Evaluate an expression and leave its value in exactly this register.
    fn expr_into(&mut self, expr: &Expr, dst: Register) -> Result<(), LowerError> {
        let mark = self.next_temp;
        let src = self.expr_to(expr, Some(dst))?;
        self.release(mark);
        if src != dst {
            self.emit(expr.span.start, Op::Move { dst, src });
        }
        Ok(())
    }

    /// Evaluate an expression, preferring to leave its value in `dst` when there is a choice.
    ///
    /// The hint is only ever used by the instruction that produces the value, never passed down to
    /// an operand, because an operand is evaluated before its siblings and writing the destination
    /// early would clobber a variable that a later operand still reads.
    #[allow(clippy::too_many_lines)]
    fn expr_to(&mut self, expr: &Expr, dst: Option<Register>) -> Result<Register, LowerError> {
        let at = expr.span.start;
        match &expr.kind {
            ExprKind::Number(value) => {
                let register = self.destination(dst);
                if let Some(value) = small_integer(*value) {
                    self.emit(
                        at,
                        Op::LoadInt {
                            dst: register,
                            value,
                        },
                    );
                } else {
                    let src = self.blueprint.constants.number(*value);
                    self.emit(at, Op::LoadConst { dst: register, src });
                }
                Ok(register)
            }
            ExprKind::String(value) => {
                let register = self.destination(dst);
                let src = self.constant(value);
                self.emit(at, Op::LoadConst { dst: register, src });
                Ok(register)
            }
            ExprKind::Boolean(value) => {
                let register = self.destination(dst);
                self.emit(
                    at,
                    Op::LoadBool {
                        dst: register,
                        value: *value,
                    },
                );
                Ok(register)
            }
            ExprKind::Null => {
                let register = self.destination(dst);
                self.emit(at, Op::LoadNull { dst: register });
                Ok(register)
            }
            ExprKind::This => {
                let register = self.destination(dst);
                self.emit(at, Op::LoadThis { dst: register });
                Ok(register)
            }
            ExprKind::Ident(ident) => {
                self.refuse_arguments(ident)?;
                let place = self.ident_place(ident);
                Ok(self.load(at, place, dst))
            }
            ExprKind::Unary { op, operand } => self.unary(at, *op, operand, dst),
            ExprKind::Update { op, prefix, target } => self.update(at, *op, *prefix, target, dst),
            ExprKind::Binary { op, left, right } => {
                let mark = self.next_temp;
                let register = self.expr(left)?;
                let lhs = self.pin(at, register, writes_a_variable(right));
                let rhs = self.expr(right)?;
                self.release(mark);

                let register = self.destination(dst);
                let cache = self.cache();
                self.emit(at, three_address(*op, register, lhs, rhs, cache));
                Ok(register)
            }
            ExprKind::Logical { op, left, right } => {
                // The rule three doc comments above this one, which this arm used to break. A
                // short circuiting operator evaluates its left side, keeps the value, and then
                // evaluates its right side, so building the result in the destination writes the
                // destination before an operand that is still allowed to read it. When the
                // destination is a variable's own slot that is the variable being read, and
                // `v = 1.5 && ('x' + v)` gave back the 1.5 instead of the old `v`.
                //
                // The compound form of the same operator has always allocated here, with a comment
                // saying the result has to end up in a register that is nobody's variable. It was
                // right and this was the same problem one arm over.
                //
                // Found by the differential harness against node. The cost is one move on
                // `let x = a || b`, and removing it needs to know whether the right side reads the
                // variable the destination belongs to, which is a walk with the resolutions in hand
                // rather than the shape check `writes_a_variable` does.
                let wanted = self.destination(dst);
                let register = if wanted.0 < self.first_temp {
                    self.alloc()
                } else {
                    wanted
                };
                self.expr_into(left, register)?;
                let jump = self.short_circuit(at, *op, register);
                self.expr_into(right, register)?;
                self.patch(jump);
                if register != wanted {
                    self.emit(
                        at,
                        Op::Move {
                            dst: wanted,
                            src: register,
                        },
                    );
                }
                Ok(wanted)
            }
            ExprKind::Assign { op, target, value } => self.assign(at, *op, target, value, dst),
            ExprKind::Conditional {
                test,
                consequent,
                alternate,
            } => {
                let mark = self.next_temp;
                let cond = self.expr(test)?;
                self.release(mark);

                let register = self.destination(dst);
                let branch = self.emit(
                    at,
                    Op::JumpIfFalse {
                        cond,
                        target: UNPATCHED,
                    },
                );
                self.expr_into(consequent, register)?;
                let over = self.emit(at, Op::Jump { target: UNPATCHED });
                self.patch(branch);
                self.expr_into(alternate, register)?;
                self.patch(over);
                Ok(register)
            }
            ExprKind::Field { object, name } => {
                let mark = self.next_temp;
                let obj = self.expr(object)?;
                self.release(mark);

                let register = self.destination(dst);
                let key = self.constant(&name.name);
                let cache = self.cache();
                self.emit(
                    at,
                    Op::GetProp {
                        dst: register,
                        obj,
                        key,
                        cache,
                    },
                );
                Ok(register)
            }
            ExprKind::Index { object, index } => {
                let mark = self.next_temp;
                let register = self.expr(object)?;
                let obj = self.pin(at, register, writes_a_variable(index));
                let index = self.expr(index)?;
                self.release(mark);

                let register = self.destination(dst);
                let cache = self.cache();
                self.emit(
                    at,
                    Op::GetIndex {
                        dst: register,
                        obj,
                        index,
                        cache,
                    },
                );
                Ok(register)
            }
            ExprKind::Call { callee, arguments } => self.call(at, callee, arguments, dst),
            ExprKind::Function(func) => {
                let blueprint = self.nested(func)?;
                let register = self.destination(dst);
                self.emit(
                    at,
                    Op::NewClosure {
                        dst: register,
                        blueprint,
                    },
                );
                Ok(register)
            }
        }
    }

    /// Refuse `arguments` inside a function rather than looking it up as a global.
    ///
    /// Scope analysis flags the function that mentions it and resolves it as a global, which is the
    /// right answer at the top level and the wrong one anywhere else. There is no arguments object
    /// to resolve it to until M1, and emitting a global lookup would turn a missing feature into a
    /// program that runs and produces the wrong answer.
    fn refuse_arguments(&self, ident: &Ident) -> Result<(), LowerError> {
        let global = matches!(self.reference(ident).resolution, Resolution::Global);
        if global && ident.name == "arguments" && self.function != FunctionId(0) {
            return Err(LowerError {
                span: ident.span,
                construct: "the arguments object",
            });
        }
        Ok(())
    }

    fn nested(&mut self, func: &Func) -> Result<BlueprintIndex, LowerError> {
        let blueprint = lower_function(self.scopes, func)?;
        let index = u32::try_from(self.blueprint.blueprints.len())
            .expect("a function holds fewer than four billion functions");
        self.blueprint.blueprints.push(blueprint);
        Ok(BlueprintIndex(index))
    }

    fn unary(
        &mut self,
        at: u32,
        op: UnaryOp,
        operand: &Expr,
        dst: Option<Register>,
    ) -> Result<Register, LowerError> {
        match op {
            UnaryOp::Delete => return self.delete(at, operand, dst),
            // `typeof` on a name nothing declares is the one read in the language that does not
            // throw, so it cannot be a load followed by an instruction.
            UnaryOp::Typeof => {
                if let ExprKind::Ident(ident) = &operand.kind
                    && matches!(self.reference(ident).resolution, Resolution::Global)
                {
                    let register = self.destination(dst);
                    let name = self.constant(&ident.name);
                    let cache = self.cache();
                    self.emit(
                        at,
                        Op::LoadGlobalForTypeof {
                            dst: register,
                            name,
                            cache,
                        },
                    );
                    self.emit(
                        at,
                        Op::TypeOf {
                            dst: register,
                            src: register,
                        },
                    );
                    return Ok(register);
                }
            }
            _ => {}
        }

        let mark = self.next_temp;
        let src = self.expr(operand)?;
        self.release(mark);
        let register = self.destination(dst);

        let op = match op {
            UnaryOp::Plus => {
                let cache = self.cache();
                Op::ToNumber {
                    dst: register,
                    src,
                    cache,
                }
            }
            UnaryOp::Minus => {
                let cache = self.cache();
                Op::Neg {
                    dst: register,
                    src,
                    cache,
                }
            }
            UnaryOp::BitNot => {
                let cache = self.cache();
                Op::BitNot {
                    dst: register,
                    src,
                    cache,
                }
            }
            UnaryOp::Not => Op::Not { dst: register, src },
            UnaryOp::Typeof => Op::TypeOf { dst: register, src },
            // The operand was evaluated for its effect and its value is thrown away.
            UnaryOp::Void => Op::LoadUndefined { dst: register },
            UnaryOp::Delete => unreachable!("handled above"),
        };
        self.emit(at, op);
        Ok(register)
    }

    fn delete(
        &mut self,
        at: u32,
        operand: &Expr,
        dst: Option<Register>,
    ) -> Result<Register, LowerError> {
        let mark = self.next_temp;
        match &operand.kind {
            ExprKind::Field { object, name } => {
                let obj = self.expr(object)?;
                self.release(mark);
                let register = self.destination(dst);
                let key = self.constant(&name.name);
                self.emit(
                    at,
                    Op::DeleteProp {
                        dst: register,
                        obj,
                        key,
                    },
                );
                Ok(register)
            }
            ExprKind::Index { object, index } => {
                let register = self.expr(object)?;
                let obj = self.pin(at, register, writes_a_variable(index));
                let index = self.expr(index)?;
                self.release(mark);
                let register = self.destination(dst);
                self.emit(
                    at,
                    Op::DeleteIndex {
                        dst: register,
                        obj,
                        index,
                    },
                );
                Ok(register)
            }
            // `delete x` answers false for a declared name and deletes the property for an
            // undeclared one, which needs an opcode that can do both. Refusing is better than
            // guessing at an answer that is right half the time.
            _ => Err(LowerError {
                span: operand.span,
                construct: "delete of anything other than a property",
            }),
        }
    }

    fn update(
        &mut self,
        at: u32,
        op: UpdateOp,
        prefix: bool,
        target: &Target,
        dst: Option<Register>,
    ) -> Result<Register, LowerError> {
        let mark = self.next_temp;
        let place = self.place(target, None)?;
        let current = self.load(at, place, None);

        let result = if prefix {
            let register =
                direct_register(place, StoreKind::Rewrite).unwrap_or_else(|| self.alloc());
            let cache = self.cache();
            self.emit(at, step(op, register, current, cache));
            self.store(at, place, register, StoreKind::Rewrite);
            register
        } else {
            // `x++` is the numeric value of the old `x`, not the old `x`, so the conversion is part
            // of the answer and not an implementation detail of the increment.
            let old = self.alloc();
            let cache = self.cache();
            self.emit(
                at,
                Op::ToNumber {
                    dst: old,
                    src: current,
                    cache,
                },
            );

            let register =
                direct_register(place, StoreKind::Rewrite).unwrap_or_else(|| self.alloc());
            let cache = self.cache();
            self.emit(at, step(op, register, old, cache));
            self.store(at, place, register, StoreKind::Rewrite);
            old
        };

        Ok(self.settle(at, result, mark, dst))
    }

    fn assign(
        &mut self,
        at: u32,
        op: AssignOp,
        target: &Target,
        value: &Expr,
        dst: Option<Register>,
    ) -> Result<Register, LowerError> {
        let mark = self.next_temp;
        let place = self.place(target, Some(value))?;

        let result = match op {
            AssignOp::Assign => {
                let hint = direct_register(place, StoreKind::Assign);
                let src = self.expr_to(value, hint)?;
                self.store(at, place, src, StoreKind::Assign);
                // The value of an assignment is the value stored, which is in the slot when the
                // store wrote straight into one and in the temporary otherwise.
                hint.unwrap_or(src)
            }
            AssignOp::Binary(binary) => {
                let current = self.load(at, place, None);
                let lhs = self.pin(at, current, writes_a_variable(value));
                let rhs = self.expr(value)?;
                let register =
                    direct_register(place, StoreKind::Rewrite).unwrap_or_else(|| self.alloc());
                let cache = self.cache();
                self.emit(at, three_address(binary, register, lhs, rhs, cache));
                self.store(at, place, register, StoreKind::Rewrite);
                register
            }
            AssignOp::Logical(logical) => {
                // The result is the left side when it short circuits and the stored value when it
                // does not, so both paths end in one register that is nobody's variable.
                let register = self.alloc();
                self.load(at, place, Some(register));
                let jump = self.short_circuit(at, logical, register);
                self.expr_into(value, register)?;
                self.store(at, place, register, StoreKind::Rewrite);
                self.patch(jump);
                register
            }
        };

        Ok(self.settle(at, result, mark, dst))
    }

    /// Emit the jump that skips the right side of a short circuiting operator.
    fn short_circuit(&mut self, at: u32, op: LogicalOp, value: Register) -> usize {
        match op {
            LogicalOp::And => self.emit(
                at,
                Op::JumpIfFalse {
                    cond: value,
                    target: UNPATCHED,
                },
            ),
            LogicalOp::Or => self.emit(
                at,
                Op::JumpIfTrue {
                    cond: value,
                    target: UNPATCHED,
                },
            ),
            // Loose equality against null is true for null and undefined and false for everything
            // else, which is exactly the test `??` makes, so there is no opcode for it.
            LogicalOp::Coalesce => {
                let mark = self.next_temp;
                let null = self.alloc();
                self.emit(at, Op::LoadNull { dst: null });
                let test = self.alloc();
                let cache = self.cache();
                self.emit(
                    at,
                    Op::Equal {
                        dst: test,
                        lhs: value,
                        rhs: null,
                        cache,
                    },
                );
                self.release(mark);
                self.emit(
                    at,
                    Op::JumpIfFalse {
                        cond: test,
                        target: UNPATCHED,
                    },
                )
            }
        }
    }

    fn call(
        &mut self,
        at: u32,
        callee: &Expr,
        arguments: &[Expr],
        dst: Option<Register>,
    ) -> Result<Register, LowerError> {
        let count = u16::try_from(arguments.len()).expect("a call has fewer than 65535 arguments");
        let threatened = arguments.iter().any(writes_a_variable);
        let mark = self.next_temp;

        // A method call keeps the object it came from, because that object is the receiver and
        // materialising the function first would lose it.
        let (receiver, function) = if let ExprKind::Field { object, name } = &callee.kind {
            let register = self.expr(object)?;
            let object = self.pin(at, register, threatened);
            let key = self.constant(&name.name);
            (Some((object, key)), None)
        } else {
            let register = self.expr(callee)?;
            (None, Some(self.pin(at, register, threatened)))
        };

        // The arguments have to end up in consecutive registers, because that is what building the
        // callee's frame copies. A call with no arguments still names the register they would start
        // at, so one is reserved either way.
        let args = self.alloc();
        for _ in 1..count {
            self.alloc();
        }
        for (index, argument) in arguments.iter().enumerate() {
            let slot = args.0 + u16::try_from(index).expect("counted above");
            self.expr_into(argument, Register(slot))?;
        }

        self.release(mark);
        let register = self.destination(dst);
        let cache = self.cache();
        let op = match (receiver, function) {
            (Some((obj, key)), _) => Op::CallMethod {
                dst: register,
                obj,
                key,
                args,
                argc: count,
                cache,
            },
            (None, Some(callee)) => Op::Call {
                dst: register,
                callee,
                args,
                argc: count,
                cache,
            },
            (None, None) => unreachable!("a call has either a receiver or a callee register"),
        };
        self.emit(at, op);
        Ok(register)
    }
}

/// Whether evaluating this expression can write to a variable's own register.
///
/// Conservative on purpose. A false positive costs one move, and a false negative is a program that
/// adds the wrong two numbers, so anything that contains an assignment or an update counts whether
/// or not the name it writes is one the other operand reads. A call cannot write a caller's
/// register, and a variable a closure can reach lives in a cell rather than a register, so neither
/// is a hazard here.
fn writes_a_variable(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Number(_)
        | ExprKind::String(_)
        | ExprKind::Boolean(_)
        | ExprKind::Null
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Function(_) => false,
        ExprKind::Assign { .. } | ExprKind::Update { .. } => true,
        ExprKind::Unary { operand, .. } => writes_a_variable(operand),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            writes_a_variable(left) || writes_a_variable(right)
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            writes_a_variable(test) || writes_a_variable(consequent) || writes_a_variable(alternate)
        }
        ExprKind::Field { object, .. } => writes_a_variable(object),
        ExprKind::Index { object, index } => writes_a_variable(object) || writes_a_variable(index),
        ExprKind::Call { callee, arguments } => {
            writes_a_variable(callee) || arguments.iter().any(writes_a_variable)
        }
    }
}

/// The register a store of this kind can write straight into, when there is one.
///
/// Only a frame slot qualifies, and only when nothing has to happen between computing the value and
/// storing it. A dead zone check reads the old value, so it has to run before the new one lands,
/// which is why an assignment to a name still in its dead zone goes through a temporary.
const fn direct_register(place: Place, kind: StoreKind) -> Option<Register> {
    match place {
        Place::Local {
            slot,
            tdz,
            constant,
            ..
        } => match kind {
            StoreKind::Initialise => Some(slot),
            StoreKind::Rewrite if !constant => Some(slot),
            StoreKind::Assign if !constant && !tdz => Some(slot),
            _ => None,
        },
        _ => None,
    }
}

/// The immediate an integer valued literal can be loaded as, if it fits in one.
///
/// The round trip through `f64` is what makes the cast safe: a float outside the range saturates and
/// then fails to compare equal, and so does a fraction and so does a NaN. Negative zero is the one
/// value that survives the round trip and must not, because `1 / -0` is not `1 / 0`.
///
/// Comparing floats exactly is the intent rather than an oversight. The question being asked is
/// whether the two values are the same number and not whether they are close, so a tolerance would
/// answer the wrong question and quietly turn a large literal into a different one.
#[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
fn small_integer(value: f64) -> Option<i32> {
    if value == 0.0 && value.is_sign_negative() {
        return None;
    }
    let candidate = value as i32;
    (f64::from(candidate) == value).then_some(candidate)
}

/// The instruction one increment or decrement lowers to.
fn step(op: UpdateOp, dst: Register, src: Register, cache: CacheIndex) -> Op {
    match op {
        UpdateOp::Increment => Op::Inc { dst, src, cache },
        UpdateOp::Decrement => Op::Dec { dst, src, cache },
    }
}

/// The instruction one binary operator lowers to.
///
/// Every operator in the tree has one, which is the point: an operator with no instruction behind it
/// would have to be refused here, and none is. That makes it long and there is nothing to factor out
/// of it, since the whole content is the mapping.
#[allow(clippy::too_many_lines)]
fn three_address(
    op: BinaryOp,
    dst: Register,
    lhs: Register,
    rhs: Register,
    cache: CacheIndex,
) -> Op {
    match op {
        BinaryOp::Add => Op::Add {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Sub => Op::Sub {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Mul => Op::Mul {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Div => Op::Div {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Rem => Op::Rem {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Pow => Op::Pow {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Shl => Op::Shl {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Shr => Op::Shr {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::UnsignedShr => Op::UnsignedShr {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::BitOr => Op::BitOr {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::BitXor => Op::BitXor {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::BitAnd => Op::BitAnd {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Equal => Op::Equal {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::NotEqual => Op::NotEqual {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::StrictEqual => Op::StrictEqual {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::StrictNotEqual => Op::StrictNotEqual {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Less => Op::Less {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::LessEqual => Op::LessEqual {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Greater => Op::Greater {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::GreaterEqual => Op::GreaterEqual {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::In => Op::In {
            dst,
            lhs,
            rhs,
            cache,
        },
        BinaryOp::Instanceof => Op::InstanceOf {
            dst,
            lhs,
            rhs,
            cache,
        },
    }
}

#[cfg(test)]
mod tests {
    use katsu_ir::FunctionBlueprint;

    use super::lower;

    /// Lower a source and check that what came out is a blueprint the verifier accepts.
    ///
    /// Every test here goes through this, so the structural checks run on every case rather than on
    /// the one case somebody remembered to write them for.
    fn lowered(source: &str) -> FunctionBlueprint {
        let (ast, scopes) = crate::frontend("test.js", source).expect("should parse and resolve");
        let blueprint = lower(&ast, &scopes).expect("should lower");
        blueprint
            .verify()
            .expect("lowering produces a blueprint that verifies");
        blueprint
    }

    /// The instructions of one blueprint, one per line.
    ///
    /// The full listing carries source offsets as well, which are exactly the thing that changes
    /// when a test source is reindented, so the tests that are about instructions do not look at
    /// them and the one test that is about positions looks at them directly.
    fn code(blueprint: &FunctionBlueprint) -> String {
        blueprint
            .code
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Compare against an expected listing, ignoring the indentation it is written with.
    #[track_caller]
    fn assert_code(actual: &str, expected: &str) {
        let expected: Vec<&str> = expected.trim().lines().map(str::trim).collect();
        assert_eq!(actual.lines().collect::<Vec<_>>(), expected);
    }

    /// What lowering refused a source for.
    fn refused(source: &str) -> &'static str {
        let (ast, scopes) = crate::frontend("test.js", source).expect("should parse and resolve");
        lower(&ast, &scopes)
            .expect_err("should be refused")
            .construct
    }

    #[test]
    fn a_module_that_does_nothing_still_returns() {
        let blueprint = lowered("");
        assert_code(
            &code(&blueprint),
            "
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn arithmetic_is_three_address_and_gives_its_temporaries_back() {
        // The frame is three registers rather than five, because the multiply's operands are
        // released before its result is given a register.
        let blueprint = lowered("1 + 2 * 3;");
        assert_eq!(blueprint.frame_size, 3);
        assert_code(
            &code(&blueprint),
            "
            load_int r0, 1
            load_int r1, 2
            load_int r2, 3
            mul r1, r1, r2, ic0
            add r0, r0, r1, ic1
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn reading_a_local_variable_does_not_copy_it() {
        // Two loads and an add. Anything more than that is a move that did not need to happen.
        assert_code(
            &code(&lowered("let a = 1; let b = 2; a + b;")),
            "
            load_int r0, 1
            load_int r1, 2
            add r2, r0, r1, ic0
            load_undefined r2
            return r2
            ",
        );
    }

    #[test]
    fn an_operand_that_assigns_forces_the_other_one_into_a_temporary() {
        // `a + (a = 2)` is 1 + 2 and not 2 + 2, which is what the move is there for.
        assert_code(
            &code(&lowered("let a = 1; a + (a = 2);")),
            "
            load_int r0, 1
            move r1, r0
            load_int r0, 2
            add r1, r1, r0, ic0
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_number_that_is_not_a_small_integer_goes_to_the_constant_pool() {
        assert_code(
            &code(&lowered("1; 1.5; 2147483648;")),
            "
            load_int r0, 1
            load_const r0, k0
            load_const r0, k1
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn the_same_string_written_twice_is_one_constant() {
        // Not written as two expression statements, because a string on its own at the top of a
        // file is a directive rather than an expression and never reaches lowering.
        let blueprint = lowered("let a = \"hi\"; let b = \"hi\";");
        assert_eq!(blueprint.constants.len(), 1);
        assert_code(
            &code(&blueprint),
            "
            load_const r0, k0
            load_const r1, k0
            load_undefined r2
            return r2
            ",
        );
    }

    #[test]
    fn a_function_declaration_gets_its_closure_when_the_scope_is_entered() {
        // The closure is created before the first statement runs, which is what makes a function
        // callable from above the line it was written on.
        let blueprint = lowered("f(1); function f(a) { return a; }");
        assert_code(
            &code(&blueprint),
            "
            new_closure r0, fn0
            load_int r1, 1
            call r1, r0, r1, 1, ic0
            load_undefined r1
            return r1
            ",
        );
        assert_eq!(blueprint.blueprints.len(), 1);
        assert_code(&code(&blueprint.blueprints[0]), "return r0");
        assert_eq!(blueprint.blueprints[0].arity, 1);
    }

    #[test]
    fn a_method_call_keeps_the_object_it_was_reached_through() {
        assert_code(
            &code(&lowered("console.log(1);")),
            "
            load_global r0, k0, ic0
            load_int r1, 1
            call_method r0, r0, k1, r1, 1, ic1
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn a_call_with_no_arguments_still_reserves_the_register_they_would_start_at() {
        // `last_argument` counts back from the first argument register, so the register has to be
        // inside the frame even when nothing is written to it.
        let blueprint = lowered("f();");
        assert_eq!(blueprint.frame_size, 2);
        assert_code(
            &code(&blueprint),
            "
            load_global r0, k0, ic0
            call r0, r0, r1, 0, ic1
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn a_captured_variable_lives_in_a_cell_and_the_parameter_still_arrives_in_a_register() {
        let blueprint = lowered("function outer(a) { return function () { return a; }; }");
        let outer = &blueprint.blueprints[0];
        assert_eq!((outer.arity, outer.cell_slots), (1, 1));
        assert_code(
            &code(outer),
            "
            new_context 1
            store_upvalue 0, 0, r0
            new_closure r1, fn0
            return r1
            ",
        );
        // Zero hops, because the inner function has no environment of its own and its chain starts
        // at the nearest enclosing one.
        assert_code(
            &code(&outer.blueprints[0]),
            "
            load_upvalue r0, 0, 0
            return r0
            ",
        );
    }

    #[test]
    fn the_top_level_gets_an_environment_when_something_written_in_it_captures() {
        // A `var` rather than a `let` so that the hoisted undefined is what shows up here and not a
        // dead zone hole, which the test below is about.
        assert_code(
            &code(&lowered("var x = 1; function f() { return x; }")),
            "
            new_context 1
            load_undefined r1
            store_upvalue 0, 0, r1
            new_closure r0, fn0
            load_int r1, 1
            store_upvalue 0, 0, r1
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_named_function_expression_sees_itself_as_the_closure_that_is_running() {
        let blueprint = lowered("const f = function me() { return me; };");
        assert_code(
            &code(&blueprint),
            "
            new_closure r0, fn0
            load_undefined r1
            return r1
            ",
        );
        assert_code(
            &code(&blueprint.blueprints[0]),
            "
            load_closure r0
            return r0
            ",
        );
    }

    #[test]
    fn a_var_is_undefined_before_the_line_that_declares_it() {
        assert_code(
            &code(&lowered("function f() { var x; return x; }").blueprints[0]),
            "
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn only_a_binding_something_checks_pays_for_the_dead_zone() {
        // Nothing reads `x` before its declaration, so there is no hole to write and no check to
        // make, and the whole thing costs one instruction.
        assert_code(
            &code(&lowered("let x = 1; x;")),
            "
            load_int r0, 1
            load_undefined r1
            return r1
            ",
        );

        // Here something does, so the hole is written when the scope is entered and the read that
        // could see it is checked.
        assert_code(
            &code(&lowered("function early() { return x; } let x = 1;")),
            "
            new_context 1
            load_uninitialized r1
            store_upvalue 0, 0, r1
            new_closure r0, fn0
            load_int r1, 1
            store_upvalue 0, 0, r1
            load_undefined r1
            return r1
            ",
        );
        assert_code(
            &code(&lowered("function early() { return x; } let x = 1;").blueprints[0]),
            "
            load_upvalue r0, 0, 0
            throw_if_uninitialized r0, k0
            return r0
            ",
        );
    }

    #[test]
    fn assigning_to_a_const_throws_and_declaring_one_does_not() {
        assert_code(
            &code(&lowered("const x = 1; x = 2;")),
            "
            load_int r0, 1
            load_int r1, 2
            throw_const_assignment
            ",
        );
    }

    #[test]
    fn both_branches_returning_still_leaves_something_at_the_end_to_land_on() {
        // The jump over the else lands one past the last instruction, so the epilogue has to be
        // emitted even though the instruction before it is a return.
        assert_code(
            &code(
                &lowered("function f(c) { if (c) { return 1; } else { return 2; } }").blueprints[0],
            ),
            "
            jump_if_false r0, @4
            load_int r1, 1
            return r1
            jump @6
            load_int r1, 2
            return r1
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_while_loop_tests_at_the_top_and_jumps_back_from_the_bottom() {
        assert_code(
            &code(&lowered("let i = 0; while (i < 10) { i = i + 1; }")),
            "
            load_int r0, 0
            load_int r1, 10
            less r1, r0, r1, ic0
            jump_if_false r1, @7
            load_int r1, 1
            add r0, r0, r1, ic1
            loop_back_edge @1, ic2
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn postfix_increment_is_the_number_the_variable_held_before() {
        assert_code(
            &code(&lowered("let x = 0; let y = x++;")),
            "
            load_int r0, 0
            to_number r2, r0, ic0
            inc r0, r2, ic1
            move r1, r2
            load_undefined r2
            return r2
            ",
        );
    }

    #[test]
    fn prefix_increment_writes_straight_into_the_variable() {
        assert_code(
            &code(&lowered("let x = 0; let y = ++x;")),
            "
            load_int r0, 0
            inc r0, r0, ic0
            move r1, r0
            load_undefined r2
            return r2
            ",
        );
    }

    #[test]
    fn coalescing_tests_loosely_against_null_because_that_is_what_it_means() {
        // `x == null` is true for null and for undefined and false for everything else, which is
        // exactly the question `??` asks, so there is no opcode for it.
        assert_code(
            &code(&lowered("let a = null; let b = a ?? 1;")),
            "
            load_null r0
            move r2, r0
            load_null r3
            equal r4, r2, r3, ic0
            jump_if_false r4, @6
            load_int r2, 1
            move r1, r2
            load_undefined r2
            return r2
            ",
        );
    }

    #[test]
    fn and_short_circuits_without_evaluating_the_right_side() {
        // The result is built in a temporary and moved into `b` at the end rather than built in
        // `b`, because the right side of a short circuiting operator runs after the left side has
        // already produced a value and is allowed to read the destination while it does.
        assert_code(
            &code(&lowered("let a = 1; let b = a && 2;")),
            "
            load_int r0, 1
            move r2, r0
            jump_if_false r2, @4
            load_int r2, 2
            move r1, r2
            load_undefined r2
            return r2
            ",
        );
    }

    #[test]
    fn a_short_circuiting_operator_does_not_clobber_the_variable_it_assigns_to() {
        // `v = 1.5 && ('x' + v)` printed "x1.5" instead of "x3", because the left side landed in
        // `v`'s own slot before the right side read `v`. Asserted on the bytecode rather than on
        // the answer so that the shape that was wrong is the thing being checked: the left side
        // goes to a temporary and `v` is written once, at the end.
        assert_code(
            &code(&lowered("let v = 3; v = 1.5 && v;")),
            "
            load_int r0, 3
            load_const r1, k0
            jump_if_false r1, @4
            move r1, r0
            move r0, r1
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_compound_assignment_to_a_property_evaluates_the_object_once() {
        assert_code(
            &code(&lowered("o.x += 1;")),
            "
            load_global r0, k0, ic0
            get_prop r1, r0, k1, ic1
            load_int r2, 1
            add r3, r1, r2, ic2
            set_prop r0, k1, r3, ic3
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn typeof_on_a_name_nothing_declares_does_not_throw() {
        // The one read in the language that is allowed to find nothing, which is why it cannot be a
        // global load followed by a `type_of`.
        assert_code(
            &code(&lowered("typeof nope;")),
            "
            load_global_for_typeof r0, k0, ic0
            type_of r0, r0
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn a_source_position_is_recorded_for_every_instruction() {
        let blueprint = lowered("let a = 1;\nlet b = 2;");
        for at in 0..blueprint.code.len() {
            assert!(
                blueprint.positions.offset_at(at).is_some(),
                "instruction {at} has no source position"
            );
        }
        // The two loads point at the literals rather than at the declarations, because the
        // instruction that fails is the one that has to name a line, and it is the literal that
        // produced it.
        assert_eq!(blueprint.positions.offset_at(0), Some(8));
        assert_eq!(blueprint.positions.offset_at(1), Some(19));
    }

    #[test]
    fn the_two_constructs_with_no_bytecode_behind_them_are_refused_by_name() {
        assert_eq!(
            refused("function f() { return arguments; }"),
            "the arguments object"
        );
        assert_eq!(
            refused("let x = 1; delete x;"),
            "delete of anything other than a property"
        );
    }

    #[test]
    fn a_program_using_the_whole_subset_verifies() {
        // The catch all. Every operator, both loops of control flow, closures, properties and
        // calls, lowered together, with `verify` run over the whole tree by `lowered`.
        let blueprint = lowered(
            "
            let total = 0;
            const limit = 10;
            var seen = false;
            function accumulate(n, step) {
                let i = 0;
                while (i < n) {
                    total += i * step - (i % 3) + (i ** 2) / 2;
                    total = total | (i & 1) ^ (i << 1) + (i >> 1) + (i >>> 1);
                    i++;
                }
                return function () { return total; };
            }
            const closure = accumulate(limit, 2);
            seen = total > 0 && total !== 0 || !(total < 0);
            let scaled = -total;
            scaled--;
            scaled ||= 1;
            scaled &&= 2;
            scaled ??= 3;
            scaled **= 2;
            flags.a = flags.a ?? 2;
            flags[\"b\"] = typeof flags === \"object\" ? -flags.a : ~flags.a;
            delete flags.a;
            if (seen) {
                console.log(closure(), flags.b, +total, void 0, \"a\" in flags, flags instanceof Object);
            } else {
                console.log(scaled, this);
            }
            ",
        );
        assert!(blueprint.frame_size > 0);
        assert_eq!(blueprint.blueprints.len(), 1);
    }
}
