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
    BlueprintIndex, CacheIndex, CodeOffset, ConstIndex, FunctionBlueprint, Handler, Op, Register,
};

use crate::ast::{
    AssignOp, BinaryOp, Binding, Block, Case, Catch, DeclKind, Expr, ExprKind, ForInit, Func,
    Ident, LogicalOp, Module, Span, Stmt, StmtKind, Target, TargetKind, UnaryOp, UpdateOp,
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
    /// The loops, switches and `finally` clauses the statement being lowered is inside, innermost
    /// last.
    ///
    /// One stack rather than two, because whether a `break` has to run a `finally` on its way out is
    /// a question about the order the two were entered in and nothing else. A `break` inside a
    /// `finally`'s protected block leaves through the `finally`, and a `break` inside a loop written
    /// inside that block does not, and only the interleaving says which is which.
    enclosing: Vec<Enclosing>,
    /// Labels written on the statement about to be lowered, which it takes as it opens its frame.
    ///
    /// A label is not a construct of its own in the bytecode, it is a name on the construct it sits
    /// in front of, so `a: while (x)` is one loop frame wearing one name rather than two frames.
    /// This is how the name gets from the label to the loop without every loop taking a parameter
    /// that is empty almost every time.
    pending_labels: Vec<String>,
    /// The next token value a labelled completion can travel as through a `finally`.
    ///
    /// Counted per function and never reset, so two labelled jumps aimed at different places can
    /// never collide inside one dispatch. The four fixed values are what everything else uses.
    next_completion: i32,
}

/// Something a `break`, a `continue` or a `return` has to deal with on its way out.
enum Enclosing {
    /// A construct `break` can leave and, if it is a loop, that `continue` can go round again.
    ///
    /// Both lists hold jumps that have been emitted with no target yet. A `break` cannot know where
    /// the end of the construct is, because the rest of the body has not been lowered, and a
    /// `continue` deliberately does not jump straight back to the top even though the top is known:
    /// it aims at the back edge instruction at the bottom of the loop, so that every iteration
    /// passes through the counter the tiering decision is going to read.
    Breakable {
        /// Which of the three it is, which decides what can aim at it.
        kind: BreakableKind,
        /// The labels written on it, outermost first, usually none.
        ///
        /// A construct can wear more than one, because `a: b: for (;;)` puts both names on the same
        /// loop, and a `break` or a `continue` may use either of them.
        labels: Vec<String>,
        /// Jumps waiting for the instruction after the construct.
        breaks: Vec<usize>,
        /// Jumps waiting for the back edge, empty for a switch and for a label.
        continues: Vec<usize>,
    },
    /// A `finally` body that every way out of the block it guards has to pass through first.
    Finally {
        /// Holds which of the completions the body was reached by.
        token: Register,
        /// Holds the value that completion is carrying, which is a thrown value or a returned one.
        payload: Register,
        /// Jumps into the body, from the normal path and from every abrupt one.
        entries: Vec<usize>,
        /// The abrupt completions that actually routed through here, and the token value each one
        /// travels as.
        ///
        /// A list rather than three flags, because a labelled jump is not one completion but one
        /// per label it aims at: `a: { b: { try { break a; } finally {} } }` has to come out of the
        /// dispatch knowing which of the two it was carrying. The unlabelled cases keep their fixed
        /// token values so the common `finally` compares the same numbers it always did.
        outcomes: Vec<(i32, Outcome)>,
    },
}

/// What a `break` or a `continue` is allowed to aim at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BreakableKind {
    /// A loop, which both a `break` and a `continue` can aim at, with or without a label.
    Loop,
    /// A switch, which an unlabelled `break` can leave and a `continue` walks straight past.
    Switch,
    /// Anything else wearing a label, which only a `break` naming that label can leave.
    ///
    /// A plain block is the usual one. It is deliberately invisible to an unlabelled `break`,
    /// because `while (x) { a: { break; } }` leaves the loop and not the block.
    Labelled,
}

/// An abrupt completion on its way out through a `finally`.
///
/// The label is carried rather than resolved, because the frame the jump is aimed at is inside the
/// construct being lowered and will be gone by the time the dispatch re issues it. A name survives
/// that, and it is unambiguous, since a label nested inside another of the same name is refused
/// before lowering ever runs.
#[derive(Clone, PartialEq, Eq)]
enum Outcome {
    Throw,
    Return,
    Break(Option<String>),
    Continue(Option<String>),
}

impl Enclosing {
    fn loop_(labels: Vec<String>) -> Self {
        Self::Breakable {
            kind: BreakableKind::Loop,
            labels,
            breaks: Vec::new(),
            continues: Vec::new(),
        }
    }

    fn switch(labels: Vec<String>) -> Self {
        Self::Breakable {
            kind: BreakableKind::Switch,
            labels,
            breaks: Vec::new(),
            continues: Vec::new(),
        }
    }

    fn labelled(labels: Vec<String>) -> Self {
        Self::Breakable {
            kind: BreakableKind::Labelled,
            labels,
            breaks: Vec::new(),
            continues: Vec::new(),
        }
    }

    /// Whether a jump naming this label can aim at this frame.
    fn wears(&self, label: &str) -> bool {
        match self {
            Self::Breakable { labels, .. } => labels.iter().any(|held| held == label),
            Self::Finally { .. } => false,
        }
    }

    /// Take the two jump lists out of a frame a loop or a switch pushed.
    ///
    /// Both callers pushed a `Breakable` a few lines earlier and popped it themselves, so the other
    /// variant is not reachable here, and saying that once is better than saying it at both.
    fn jumps(self) -> (Vec<usize>, Vec<usize>) {
        match self {
            Self::Breakable {
                breaks, continues, ..
            } => (breaks, continues),
            Self::Finally { .. } => {
                unreachable!("a loop and a switch pop the frame they pushed")
            }
        }
    }
}

/// Which way a `finally` body was reached, in the register the dispatch after it reads.
///
/// A normal completion is zero because zero is falsy, so the path almost every `finally` takes gets
/// out of the dispatch on one `jump_if_false` with nothing compared.
const COMPLETION_NORMAL: i32 = 0;
/// The block threw and nothing between it and here caught it.
const COMPLETION_THROW: i32 = 1;
/// The block returned, and the value it returned is in the payload register.
const COMPLETION_RETURN: i32 = 2;
/// The block broke out of a loop or a switch that is outside this `finally`.
const COMPLETION_BREAK: i32 = 3;
/// The block continued a loop that is outside this `finally`.
const COMPLETION_CONTINUE: i32 = 4;

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
            enclosing: Vec::new(),
            pending_labels: Vec::new(),
            next_completion: COMPLETION_CONTINUE + 1,
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
        self.patch_to(jump, target);
    }

    /// Point a jump emitted earlier at an instruction that already exists.
    ///
    /// Only a switch needs this. Its jump past the tests lands on whichever clause is the default,
    /// and that clause can be anywhere among the others, so the target is known after the fact
    /// rather than at the point the jump is patched.
    fn patch_to(&mut self, jump: usize, target: CodeOffset) {
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
        self.statements(body)
    }

    /// Run a list of statements in a scope somebody else has already entered.
    fn statements(&mut self, body: &[Stmt]) -> Result<(), LowerError> {
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
        self.scope_prelude_parts(std::slice::from_ref(&body))
    }

    /// The same prelude for a scope whose statements arrive in more than one list.
    ///
    /// A switch is the one of those. Its clauses are separate lists of statements and one shared
    /// scope, so all of their holes go in before any of their closures, exactly as they would if
    /// the clauses were written as one block.
    fn scope_prelude_parts(&mut self, parts: &[&[Stmt]]) -> Result<(), LowerError> {
        for statement in parts.iter().copied().flatten() {
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

        for statement in parts.iter().copied().flatten() {
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
                self.return_value(at, src);
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
            StmtKind::While { test, body } => self.while_loop(at, test, body)?,
            StmtKind::DoWhile { body, test } => self.do_while_loop(at, body, test)?,
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => self.for_loop(at, init.as_ref(), test.as_ref(), update.as_ref(), body)?,
            StmtKind::Switch {
                discriminant,
                cases,
            } => self.switch(at, discriminant, cases)?,
            StmtKind::Labeled { label, body } => self.labeled(label, body)?,
            StmtKind::Break(label) => self.break_out(at, label.as_ref().map(|it| it.name.as_str())),
            StmtKind::Continue(label) => {
                self.continue_on(at, label.as_ref().map(|it| it.name.as_str()));
            }
            StmtKind::Throw(value) => {
                let mark = self.next_temp;
                let src = self.expr(value)?;
                self.emit(at, Op::Throw { src });
                self.release(mark);
            }
            StmtKind::Try {
                block,
                catch,
                finally,
            } => self.try_statement(at, block, catch.as_ref(), finally.as_ref())?,
            StmtKind::Block(body) => self.body(body)?,
        }
        Ok(())
    }

    /// Emit a `return`, or send it through the `finally` clauses that have to run before it.
    ///
    /// The value is moved into the innermost `finally`'s payload register rather than being carried
    /// in whatever temporary the expression landed in, because the finally body is about to run and
    /// is allowed to use every temporary above the two this construct reserved.
    fn return_value(&mut self, at: u32, src: Register) {
        let Some(index) = self.innermost_finally() else {
            self.emit(at, Op::Return { src });
            return;
        };
        let Enclosing::Finally { payload, .. } = self.enclosing[index] else {
            unreachable!("innermost_finally only ever answers with a finally")
        };
        self.emit(at, Op::Move { dst: payload, src });
        self.route(at, index, Outcome::Return);
    }

    /// Emit a `break`, or send it through a `finally` that is in the way.
    ///
    /// The search stops at the first frame the `break` could be talking about, and a `finally` counts
    /// as one of those wherever it sits, because its body has to run before the jump goes anywhere.
    /// An unlabelled `break` walks past a labelled block, since `while (x) { a: { break; } }` leaves
    /// the loop, and a labelled one walks past everything that is not wearing its name.
    fn break_out(&mut self, at: u32, label: Option<&str>) {
        let index = self
            .enclosing
            .iter()
            .rposition(|frame| match label {
                Some(label) => matches!(frame, Enclosing::Finally { .. }) || frame.wears(label),
                None => !matches!(
                    frame,
                    Enclosing::Breakable {
                        kind: BreakableKind::Labelled,
                        ..
                    }
                ),
            })
            .expect("scope analysis rejected a break with nothing to leave");
        if matches!(self.enclosing[index], Enclosing::Finally { .. }) {
            self.route(at, index, Outcome::Break(label.map(str::to_owned)));
            return;
        }
        let jump = self.emit(at, Op::Jump { target: UNPATCHED });
        let Enclosing::Breakable { breaks, .. } = &mut self.enclosing[index] else {
            unreachable!("the frame was checked one line ago")
        };
        breaks.push(jump);
    }

    /// Emit a `continue`, or send it through a `finally` that is in the way.
    ///
    /// A switch is walked past because a `continue` inside one belongs to the loop around it, and a
    /// `finally` is not, because its body runs whatever the `continue` is aimed at. A labelled
    /// `continue` also walks past every loop that is not wearing its name, which is the only thing
    /// it can do that the unlabelled one cannot.
    fn continue_on(&mut self, at: u32, label: Option<&str>) {
        let index = self
            .enclosing
            .iter()
            .rposition(|frame| {
                if matches!(frame, Enclosing::Finally { .. }) {
                    return true;
                }
                let loops = matches!(
                    frame,
                    Enclosing::Breakable {
                        kind: BreakableKind::Loop,
                        ..
                    }
                );
                match label {
                    Some(label) => loops && frame.wears(label),
                    None => loops,
                }
            })
            .expect("scope analysis rejected a continue with no loop around it");
        if matches!(self.enclosing[index], Enclosing::Finally { .. }) {
            self.route(at, index, Outcome::Continue(label.map(str::to_owned)));
            return;
        }
        let jump = self.emit(at, Op::Jump { target: UNPATCHED });
        let Enclosing::Breakable { continues, .. } = &mut self.enclosing[index] else {
            unreachable!("the frame was checked one line ago")
        };
        continues.push(jump);
    }

    /// Which `finally` an abrupt completion written here reaches first, if any.
    fn innermost_finally(&self) -> Option<usize> {
        self.enclosing
            .iter()
            .rposition(|frame| matches!(frame, Enclosing::Finally { .. }))
    }

    /// Jump into a `finally` body, saying which completion is being carried into it.
    ///
    /// The token is what the dispatch after the body reads to decide where to send the completion
    /// on, and recording the kind on the frame is what keeps that dispatch to the completions that
    /// can actually arrive rather than all four.
    fn route(&mut self, at: u32, index: usize, outcome: Outcome) {
        let Enclosing::Finally {
            token, outcomes, ..
        } = &self.enclosing[index]
        else {
            unreachable!("the caller checked the frame")
        };
        let token = *token;
        // Two `break`s aimed at the same place travel as the same token value and share one arm of
        // the dispatch, which is what keeps the ordinary `finally` down to the numbers it always
        // compared. Only a jump aimed somewhere new needs a value of its own.
        let kind = match outcomes.iter().find(|(_, held)| *held == outcome) {
            Some((kind, _)) => *kind,
            None => match &outcome {
                Outcome::Return => COMPLETION_RETURN,
                Outcome::Break(None) => COMPLETION_BREAK,
                Outcome::Continue(None) => COMPLETION_CONTINUE,
                Outcome::Break(Some(_)) | Outcome::Continue(Some(_)) => {
                    self.next_completion += 1;
                    self.next_completion - 1
                }
                Outcome::Throw => unreachable!("a throw finds its handler without being routed"),
            },
        };

        self.emit(
            at,
            Op::LoadInt {
                dst: token,
                value: kind,
            },
        );
        let jump = self.emit(at, Op::Jump { target: UNPATCHED });
        let Enclosing::Finally {
            entries, outcomes, ..
        } = &mut self.enclosing[index]
        else {
            unreachable!("the caller checked the frame")
        };
        entries.push(jump);
        if !outcomes.iter().any(|(_, held)| *held == outcome) {
            outcomes.push((kind, outcome));
        }
    }

    /// Lower a `try` and whichever of its two clauses are written.
    ///
    /// `try B catch C finally F` is lowered as `try { try B catch C } finally F`, which is how the
    /// standard defines it and is why there is no arm here for all three at once. It also gets the
    /// table order right for free: the inner entry is pushed first, so a throw in `B` finds `C` and
    /// a throw in `C` finds `F`.
    fn try_statement(
        &mut self,
        at: u32,
        block: &Block,
        catch: Option<&Catch>,
        finally: Option<&Block>,
    ) -> Result<(), LowerError> {
        let Some(finally) = finally else {
            let catch = catch.expect("the grammar requires a catch when there is no finally");
            return self.try_catch(at, block, catch);
        };
        self.try_finally(at, block, catch, finally)
    }

    /// Lower a `try` and its `catch`.
    ///
    /// Nothing is emitted for entering the protected block, which is the point of a handler table:
    /// the range is recorded once at lowering time and a `try` that never throws costs exactly what
    /// the same statements would cost without it. The only instruction the shape adds is the jump
    /// over the handler, and that one is on the path a program actually takes.
    ///
    /// The entry goes into the table after the block it protects has been lowered, which is what
    /// puts a nested `try` in front of the one around it and makes first match the same thing as
    /// innermost match.
    fn try_catch(&mut self, at: u32, block: &Block, catch: &Catch) -> Result<(), LowerError> {
        let start = self.here();
        self.body(&block.body)?;
        let end = self.here();
        let over = self.emit(at, Op::Jump { target: UNPATCHED });

        let target = self.here();
        let register = self.catch_clause(catch)?;
        // An empty protected block cannot throw, so it gets no entry rather than an entry that
        // could never fire. `verify` rejects an empty range for the same reason.
        if start < end {
            self.blueprint.handlers.push(Handler {
                start,
                end,
                target,
                register,
            });
        }
        self.patch(over);
        Ok(())
    }

    /// Lower a `try` with a `finally`, which is where a completion becomes a value in a register.
    ///
    /// A `catch` runs on one way out of a block and a `finally` runs on all five, so it cannot be
    /// another entry in the handler table: the table only knows about throwing. What it is instead
    /// is a body with one entry point, reached from every way out of the guarded block, and a
    /// dispatch after it that sends the completion on to wherever it was going.
    ///
    /// Two registers carry that across the body. The token says which of the five ways out this was
    /// and the payload holds what the completion is carrying, which is a thrown value or a returned
    /// one. A throw needs no jump because the handler table already lands on the prologue that sets
    /// the token, and a normal completion sets the token to zero, which is falsy, so the dispatch
    /// takes one instruction on the path nearly every `finally` takes.
    ///
    /// The frame is popped before the body is lowered, which is not an ordering detail. It is what
    /// makes `try { return 1; } finally { return 2; }` answer two: the `return` inside the body is
    /// lowered against whatever is outside this construct, so it leaves rather than routing back
    /// into the body it is written in, and the pending completion is dropped because the dispatch is
    /// never reached.
    fn try_finally(
        &mut self,
        at: u32,
        block: &Block,
        catch: Option<&Catch>,
        finally: &Block,
    ) -> Result<(), LowerError> {
        let mark = self.next_temp;
        let token = self.alloc();
        let payload = self.alloc();

        let start = self.here();
        self.enclosing.push(Enclosing::Finally {
            token,
            payload,
            entries: Vec::new(),
            outcomes: Vec::new(),
        });
        let result = match catch {
            Some(catch) => self.try_catch(at, block, catch),
            None => self.body(&block.body),
        };
        let end = self.here();
        let Some(Enclosing::Finally {
            entries, outcomes, ..
        }) = self.enclosing.pop()
        else {
            unreachable!("the finally frame was pushed")
        };
        result?;

        // A guarded block with no instructions in it has no way out but the normal one, so the
        // token, the handler entry and the dispatch would all be machinery around a body that just
        // runs. `try {} finally { f(); }` is a call and nothing else.
        if start == end {
            self.release(mark);
            return self.body(&finally.body);
        }

        self.emit(
            at,
            Op::LoadInt {
                dst: token,
                value: COMPLETION_NORMAL,
            },
        );
        let over = self.emit(at, Op::Jump { target: UNPATCHED });

        // The handler target, which is one instruction and then a fall through into the body. The
        // search has already put the thrown value in the payload register, so there is nothing to
        // move and nothing to jump to.
        let handler = self.here();
        self.emit(
            at,
            Op::LoadInt {
                dst: token,
                value: COMPLETION_THROW,
            },
        );
        self.blueprint.handlers.push(Handler {
            start,
            end,
            target: handler,
            register: payload,
        });

        let body = self.here();
        self.patch_to(over, body);
        for jump in entries {
            self.patch_to(jump, body);
        }
        self.body(&finally.body)?;

        self.dispatch(at, token, payload, outcomes);
        self.release(mark);
        Ok(())
    }

    /// Send a completion on from the end of a `finally` body to wherever it was going.
    ///
    /// Every arm ends the run of instructions it is in, by returning, throwing or jumping, so the
    /// arms are laid out one after another with no jump between them and the instruction after the
    /// last of them is where a normal completion carries on.
    ///
    /// A throw is always one of the arms, because the handler entry over the guarded block always
    /// exists by the time this runs. The other three are here only if something actually routed
    /// through, which is what keeps the usual `try` and `finally` down to two instructions of
    /// dispatch rather than a chain of comparisons about completions that cannot arrive.
    fn dispatch(
        &mut self,
        at: u32,
        token: Register,
        payload: Register,
        outcomes: Vec<(i32, Outcome)>,
    ) {
        let mut kinds = vec![(COMPLETION_THROW, Outcome::Throw)];
        kinds.extend(outcomes);

        let done = self.emit(
            at,
            Op::JumpIfFalse {
                cond: token,
                target: UNPATCHED,
            },
        );
        let last = kinds.pop().expect("a throw is always one of them");
        let mut tests = Vec::with_capacity(kinds.len());
        for (kind, outcome) in kinds {
            let inner = self.next_temp;
            let want = self.alloc();
            self.emit(
                at,
                Op::LoadInt {
                    dst: want,
                    value: kind,
                },
            );
            let matched = self.alloc();
            let cache = self.cache();
            self.emit(
                at,
                Op::StrictEqual {
                    dst: matched,
                    lhs: token,
                    rhs: want,
                    cache,
                },
            );
            let jump = self.emit(
                at,
                Op::JumpIfTrue {
                    cond: matched,
                    target: UNPATCHED,
                },
            );
            self.release(inner);
            tests.push((outcome, jump));
        }

        // The last kind needs no test, because the token is not zero and every other kind has been
        // asked about and jumped away.
        self.completion(at, &last.1, payload);
        for (outcome, jump) in tests {
            self.patch(jump);
            self.completion(at, &outcome, payload);
        }
        self.patch(done);
    }

    /// Emit the one completion a dispatch arm carries.
    ///
    /// A return, a break and a continue go through the same three functions an ordinary statement
    /// goes through, which is what makes a `finally` inside a `finally` work without anything here
    /// knowing about nesting: the completion is re issued as if it had been written just outside
    /// this construct, so the next one out picks it up the same way this one did.
    ///
    /// A throw does not need that, because the `throw` instruction is inside whatever range guards
    /// this code and the table finds the next handler on its own.
    fn completion(&mut self, at: u32, outcome: &Outcome, payload: Register) {
        match outcome {
            Outcome::Throw => {
                self.emit(at, Op::Throw { src: payload });
            }
            Outcome::Return => self.return_value(at, payload),
            Outcome::Break(label) => self.break_out(at, label.as_deref()),
            Outcome::Continue(label) => self.continue_on(at, label.as_deref()),
        }
    }

    /// Lower the body of a `catch`, and say which register the thrown value has to arrive in.
    ///
    /// A parameter that nothing captures is a frame slot, so the value is written straight into it
    /// and the clause starts with no instructions at all. A captured one lives in a cell, so the
    /// value arrives in a temporary and the first instruction copies it across, which is the same
    /// two cases a function parameter has and for the same reason.
    fn catch_clause(&mut self, catch: &Catch) -> Result<Register, LowerError> {
        let mark = self.next_temp;
        let register = match &catch.param {
            Some(param) => {
                let place = self.ident_place(param);
                if let Some(slot) = direct_register(place, StoreKind::Initialise) {
                    slot
                } else {
                    let temp = self.alloc();
                    self.store(param.span.start, place, temp, StoreKind::Initialise);
                    temp
                }
            }
            // `catch {}` still needs somewhere for the value to land, because the search stores it
            // before it knows whether anybody wanted it. A register nothing reads is cheaper than
            // a second kind of table entry saying the value is not wanted.
            None => self.alloc(),
        };
        self.body(&catch.body.body)?;
        self.release(mark);
        Ok(register)
    }

    /// Lower a `while`.
    ///
    /// The test is at the top and the jump back is at the bottom, so an iteration costs one taken
    /// backwards jump rather than a test and a forwards jump, and a loop that never runs costs one
    /// test and nothing else.
    fn while_loop(&mut self, at: u32, test: &Expr, body: &Stmt) -> Result<(), LowerError> {
        let labels = self.take_labels();
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

        self.enclosing.push(Enclosing::loop_(labels));
        let result = self.statement(body);
        let frame = self.enclosing.pop().expect("the loop frame was pushed");
        let (breaks, continues) = frame.jumps();
        result?;

        // Every `continue` lands here, on the back edge itself rather than past it, so a loop that
        // is mostly continues still counts its iterations and still gets to a hotter tier.
        for jump in continues {
            self.patch(jump);
        }
        let profile = self.cache();
        self.emit(
            at,
            Op::LoopBackEdge {
                target: top,
                profile,
            },
        );
        self.patch(exit);
        for jump in breaks {
            self.patch(jump);
        }
        Ok(())
    }

    /// Lower a labelled statement, which emits nothing of its own.
    ///
    /// A label is a name on the statement after it and not a construct, so nearly always the right
    /// thing to do is hand the name to that statement and let it wear it. A loop and a switch
    /// already open a frame a jump can aim at, so they take the pending names as they open it and
    /// this costs exactly nothing.
    ///
    /// Anything else needs a frame of its own, because `a: { break a; }` has somewhere to go and a
    /// plain block does not open one. That frame is deliberately invisible to an unlabelled `break`,
    /// so a `break` with no name written inside it still leaves the loop around it.
    fn labeled(&mut self, label: &Ident, body: &Stmt) -> Result<(), LowerError> {
        self.pending_labels.push(label.name.clone());
        if matches!(
            body.kind,
            StmtKind::Labeled { .. }
                | StmtKind::While { .. }
                | StmtKind::DoWhile { .. }
                | StmtKind::For { .. }
                | StmtKind::Switch { .. }
        ) {
            return self.statement(body);
        }

        let labels = std::mem::take(&mut self.pending_labels);
        self.enclosing.push(Enclosing::labelled(labels));
        let result = self.statement(body);
        let frame = self.enclosing.pop().expect("the label frame was pushed");
        let (breaks, _) = frame.jumps();
        result?;
        for jump in breaks {
            self.patch(jump);
        }
        Ok(())
    }

    /// Take the labels written on the construct being lowered, leaving none for the next one.
    fn take_labels(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_labels)
    }

    /// Lower a `do while`.
    ///
    /// The body comes first and the test comes after it, which is the whole of the difference from
    /// a `while` in the source and almost the whole of it in the bytecode too. The one thing that
    /// is not obvious is where a `continue` goes. It lands on the test rather than past it, because
    /// an iteration cut short still has to ask the condition before the next one starts, and a
    /// `continue` that jumped to the back edge would turn `do { continue; } while (false)` into an
    /// endless loop.
    fn do_while_loop(&mut self, at: u32, body: &Stmt, test: &Expr) -> Result<(), LowerError> {
        let labels = self.take_labels();
        let top = self.here();

        self.enclosing.push(Enclosing::loop_(labels));
        let result = self.statement(body);
        let frame = self.enclosing.pop().expect("the loop frame was pushed");
        let (breaks, continues) = frame.jumps();
        result?;

        for jump in continues {
            self.patch(jump);
        }

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
        let profile = self.cache();
        self.emit(
            at,
            Op::LoopBackEdge {
                target: top,
                profile,
            },
        );

        self.patch(exit);
        for jump in breaks {
            self.patch(jump);
        }
        Ok(())
    }

    /// Lower a `for`, the three part one.
    ///
    /// The layout is the head, then the test, then the body, then the update, then the jump back to
    /// the test. A `continue` lands on the update rather than on the back edge, which is the only
    /// place it can land: skipping the update would make `for (let i = 0; i < 3; i++) { continue; }`
    /// an endless loop, and skipping the test would run the body after the condition went false.
    ///
    /// Every part is optional and every part that is absent costs nothing. `for (;;)` lowers to a
    /// body and a back edge with no comparison in it at all, which is a tighter loop than the same
    /// thing written as `while (true)`, and that is a real difference rather than an accident: an
    /// absent test is not a test against `true`.
    ///
    /// The scope the head opens is the loop's own and the analysis already put the head's names in
    /// it, so all this owes it is the dead zone holes, which go in before the head runs because
    /// `for (let i = i; ;)` has to be a `ReferenceError` rather than a read of an outer `i`.
    fn for_loop(
        &mut self,
        at: u32,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> Result<(), LowerError> {
        let labels = self.take_labels();
        match init {
            None => {}
            Some(ForInit::Expr(expr)) => {
                let mark = self.next_temp;
                self.expr(expr)?;
                self.release(mark);
            }
            Some(ForInit::Declare { kind, bindings }) => {
                if *kind != DeclKind::Var {
                    for binding in bindings {
                        self.dead_zone_hole(binding);
                    }
                }
                for binding in bindings {
                    self.declare(*kind, binding)?;
                }
            }
        }

        let top = self.here();
        let exit = match test {
            Some(test) => {
                let mark = self.next_temp;
                let cond = self.expr(test)?;
                self.release(mark);
                Some(self.emit(
                    at,
                    Op::JumpIfFalse {
                        cond,
                        target: UNPATCHED,
                    },
                ))
            }
            None => None,
        };

        self.enclosing.push(Enclosing::loop_(labels));
        let result = self.statement(body);
        let frame = self.enclosing.pop().expect("the loop frame was pushed");
        let (breaks, continues) = frame.jumps();
        result?;

        for jump in continues {
            self.patch(jump);
        }
        if let Some(update) = update {
            let mark = self.next_temp;
            self.expr(update)?;
            self.release(mark);
        }

        let profile = self.cache();
        self.emit(
            at,
            Op::LoopBackEdge {
                target: top,
                profile,
            },
        );

        if let Some(exit) = exit {
            self.patch(exit);
        }
        for jump in breaks {
            self.patch(jump);
        }
        Ok(())
    }

    /// Lower a `switch`.
    ///
    /// The shape is a run of comparisons followed by the clause bodies laid out in source order, so
    /// that falling out of the bottom of one clause falls into the next one, which is what the
    /// language does and what a jump table would have to work to reproduce.
    ///
    /// Two things about the order are the standard's rather than ours. The comparisons run in
    /// source order over the clauses that have a test, which is why a `default` in the middle does
    /// not stop the clauses after it from being compared, and the scope holding the clauses'
    /// declarations is created before the first comparison rather than at the first clause that
    /// runs, which is why a `case` test can be in the dead zone of a `let` written three clauses
    /// further down.
    fn switch(&mut self, at: u32, discriminant: &Expr, cases: &[Case]) -> Result<(), LowerError> {
        let labels = self.take_labels();
        let mark = self.next_temp;
        let value = self.expr(discriminant)?;
        // Every test is evaluated after this and any of them can assign, so a discriminant that
        // came back in a variable's own slot is copied out of it first.
        let value = self.pin(at, value, true);

        let parts: Vec<&[Stmt]> = cases.iter().map(|case| case.body.as_slice()).collect();
        self.scope_prelude_parts(&parts)?;

        // One entry per clause, holding the jump that reaches it, and nothing for the default,
        // which is reached by falling off the end of the comparisons instead.
        let mut entries: Vec<Option<usize>> = Vec::with_capacity(cases.len());
        for case in cases {
            let Some(test) = case.test.as_ref() else {
                entries.push(None);
                continue;
            };
            let inner = self.next_temp;
            let candidate = self.expr(test)?;
            let matched = self.alloc();
            let cache = self.cache();
            self.emit(
                case.span.start,
                Op::StrictEqual {
                    dst: matched,
                    lhs: value,
                    rhs: candidate,
                    cache,
                },
            );
            let jump = self.emit(
                case.span.start,
                Op::JumpIfTrue {
                    cond: matched,
                    target: UNPATCHED,
                },
            );
            self.release(inner);
            entries.push(Some(jump));
        }

        // Nothing matched. Where that goes depends on whether there is a default, and that is not
        // known as an instruction offset until the bodies have been laid out.
        let miss = self.emit(at, Op::Jump { target: UNPATCHED });
        self.release(mark);

        self.enclosing.push(Enclosing::switch(labels));
        let mut default_target = None;
        let mut result = Ok(());
        for (case, entry) in cases.iter().zip(entries) {
            match entry {
                Some(jump) => self.patch(jump),
                None => default_target = Some(self.here()),
            }
            result = self.statements(&case.body);
            if result.is_err() {
                break;
            }
        }
        let frame = self.enclosing.pop().expect("the switch frame was pushed");
        let (breaks, _) = frame.jumps();
        result?;

        match default_target {
            Some(target) => self.patch_to(miss, target),
            None => self.patch(miss),
        }
        for jump in breaks {
            self.patch(jump);
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
            ExprKind::Object { properties } => {
                // An object literal is the one expression that writes its destination before its
                // operands run, because the stores that fill it need somewhere to store into. That
                // makes the `dst` hint unsafe in a way no other expression is, and unsafe for
                // reads and not only for writes. `x = {a: (x = 1)}` would build into `x`, let the
                // inner assignment overwrite it with a number and then store into that number, and
                // `x = {a: x}` would show the property the half made object rather than the value
                // `x` had before the line. The second one is why this asks the stronger question,
                // and the differential harness is what found it.
                let threatened = properties
                    .iter()
                    .any(|property| touches_a_variable(&property.value));
                let register = match dst {
                    Some(wanted) if threatened && wanted.0 < self.first_temp => self.alloc(),
                    _ => self.destination(dst),
                };

                let slots = u16::try_from(properties.len()).unwrap_or(u16::MAX);
                self.emit(
                    at,
                    Op::NewObject {
                        dst: register,
                        slots,
                    },
                );

                // One store per property, in source order, which is the order they enumerate in.
                // A duplicate name stores twice and the second value wins, which is what the
                // language says and what falls out of doing this with stores rather than with a
                // single instruction that takes a list.
                for property in properties {
                    let mark = self.next_temp;
                    let value = self.expr(&property.value)?;
                    self.release(mark);
                    let key = self.constant(&property.name.name);
                    let cache = self.cache();
                    self.emit(
                        property.span.start,
                        Op::SetProp {
                            obj: register,
                            key,
                            value,
                            cache,
                        },
                    );
                }
                Ok(register)
            }
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
        ExprKind::Object { properties } => properties
            .iter()
            .any(|property| writes_a_variable(&property.value)),
    }
}

/// Whether evaluating this expression can read or write a variable's own register.
///
/// The stronger form of the question above, and the one an object literal has to ask, because a
/// literal writes its destination before its values run. Reading is a hazard there and is not one
/// anywhere else: every other expression produces its value before anything is written, so an
/// operand that reads a variable reads what was in it.
///
/// Conservative in the same way, and deliberately not narrowed to "reads the variable this is being
/// assigned to". Lowering has a register here and not a name, so telling the two apart would mean
/// threading the assignment target down through every expression, and what it would buy is one
/// move instruction on `let o = { a: somethingElse }`. A move is worth a great deal less than the
/// machinery to avoid it.
///
/// A function expression is not a hazard even though it can capture, because a captured variable
/// lives in a cell rather than in a register, so making a closure never reads the register a
/// literal would be building into.
fn touches_a_variable(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Number(_)
        | ExprKind::String(_)
        | ExprKind::Boolean(_)
        | ExprKind::Null
        | ExprKind::This
        | ExprKind::Function(_) => false,
        ExprKind::Ident(_) | ExprKind::Assign { .. } | ExprKind::Update { .. } => true,
        ExprKind::Unary { operand, .. } => touches_a_variable(operand),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            touches_a_variable(left) || touches_a_variable(right)
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            touches_a_variable(test)
                || touches_a_variable(consequent)
                || touches_a_variable(alternate)
        }
        ExprKind::Field { object, .. } => touches_a_variable(object),
        ExprKind::Index { object, index } => {
            touches_a_variable(object) || touches_a_variable(index)
        }
        ExprKind::Call { callee, arguments } => {
            touches_a_variable(callee) || arguments.iter().any(touches_a_variable)
        }
        ExprKind::Object { properties } => properties
            .iter()
            .any(|property| touches_a_variable(&property.value)),
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
    fn an_object_literal_is_one_new_object_and_one_store_per_property() {
        // Three instructions where one would do, on purpose. A store is what takes the shape
        // transition, so building a literal out of stores means a literal and an object grown a
        // property at a time reach the same shape and neither needs its own code path.
        let blueprint = lowered("let o = { a: 1, b: 2 };");
        assert_code(
            &code(&blueprint),
            "
            new_object r0, 2
            load_int r1, 1
            set_prop r0, k0, r1, ic0
            load_int r1, 2
            set_prop r0, k1, r1, ic1
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn an_empty_object_literal_is_one_instruction() {
        let blueprint = lowered("f({});");
        assert!(
            code(&blueprint).contains("new_object r1, 0"),
            "{}",
            code(&blueprint)
        );
    }

    #[test]
    fn a_property_value_that_writes_a_variable_does_not_build_into_that_variable() {
        // `x = {a: (x = 1)}` would otherwise build the object in `x`, let the inner assignment
        // overwrite it with a number, and then store a property on the number. The temporary and
        // the move at the end are what stop that, and they only appear when a value could write.
        let hazard = code(&lowered("var x = 0; x = { a: (x = 1) };"));
        assert!(
            hazard.contains("new_object r1, 1"),
            "the object should be built in a temporary, not in r0, {hazard}"
        );
        let safe = code(&lowered("var x = 0; x = { a: 1 };"));
        assert!(
            safe.contains("new_object r0, 1"),
            "with nothing that can write, the object goes straight into the variable, {safe}"
        );
    }

    #[test]
    fn a_property_value_that_only_reads_a_variable_does_not_build_into_that_variable_either() {
        // `x = {a: x}` is the case a test for writes alone lets through. Building into `x` would
        // hand the property the half made object rather than the value the line started with, and
        // the differential harness found it as a false circular reference in the printed output.
        let hazard = code(&lowered("var x = 0; x = { a: x };"));
        assert!(
            hazard.contains("new_object r1, 1"),
            "the object should be built in a temporary, not in r0, {hazard}"
        );
        // A read through a property is the same hazard, since the object it reads through is the
        // variable being built into.
        let field = code(&lowered("var x = 0; x = { a: x.b };"));
        assert!(
            field.contains("new_object r1, 1"),
            "a field read is a read, {field}"
        );
    }

    #[test]
    fn a_duplicate_name_is_two_stores_and_the_second_one_wins() {
        // Falls out of lowering to stores rather than to an instruction that takes a list, and it
        // is the behaviour the language specifies, so it is worth a test rather than a comment.
        let blueprint = lowered("let o = { a: 1, a: 2 };");
        let text = code(&blueprint);
        assert_eq!(
            text.matches("set_prop").count(),
            2,
            "both stores are emitted, {text}"
        );
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
    fn a_for_loop_runs_its_head_once_and_its_update_at_the_bottom() {
        // The head is the first `load_int r1, 0` and it is outside the loop, since the back edge at
        // the end goes to @2 and not to @1. The update is the `add r1, r1, r2` just above the back
        // edge, which is where the language puts it and not where the source does.
        assert_code(
            &code(&lowered(
                "let s = 0; for (let i = 0; i < 5; i = i + 1) { s = s + i; }",
            )),
            "
            load_int r0, 0
            load_int r1, 0
            load_int r2, 5
            less r2, r1, r2, ic0
            jump_if_false r2, @9
            add r0, r0, r1, ic1
            load_int r2, 1
            add r1, r1, r2, ic2
            loop_back_edge @2, ic3
            load_undefined r2
            return r2
            ",
        );
    }

    #[test]
    fn a_for_loop_with_no_test_has_no_comparison_at_all() {
        // An absent test is not a test against `true`. There is no comparison and no forward jump
        // in this listing, so `for (;;)` is a tighter loop than `while (true)` written out, and it
        // needs no implicit return either because nothing can reach past the back edge.
        assert_code(
            &code(&lowered("let s = 0; for (;;) { s = s + 1; }")),
            "
            load_int r0, 0
            load_int r1, 1
            add r0, r0, r1, ic0
            loop_back_edge @1, ic1
            ",
        );
    }

    #[test]
    fn a_continue_in_a_for_loop_lands_on_the_update_and_a_break_lands_past_it() {
        // The two go to different places and both of them matter. The `continue` at @5 goes to @6,
        // the update, because skipping it would leave the loop variable where it was and the loop
        // would never end. The `break` goes to @9, past the back edge.
        assert_code(
            &code(&lowered(
                "let s = 0; for (let i = 0; i < 5; i = i + 1) { continue; }",
            )),
            "
            load_int r0, 0
            load_int r1, 0
            load_int r2, 5
            less r2, r1, r2, ic0
            jump_if_false r2, @9
            jump @6
            load_int r2, 1
            add r1, r1, r2, ic1
            loop_back_edge @2, ic2
            load_undefined r2
            return r2
            ",
        );
        assert_code(
            &code(&lowered(
                "let s = 0; for (let i = 0; i < 5; i = i + 1) { break; }",
            )),
            "
            load_int r0, 0
            load_int r1, 0
            load_int r2, 5
            less r2, r1, r2, ic0
            jump_if_false r2, @9
            jump @9
            load_int r2, 1
            add r1, r1, r2, ic1
            loop_back_edge @2, ic2
            load_undefined r2
            return r2
            ",
        );
    }

    #[test]
    fn a_do_while_asks_nothing_before_the_first_iteration() {
        // The body is at the top with no comparison above it, which is the whole point of the form.
        // The test is at @3 and the back edge below it goes to @1, the first instruction of the
        // body, so the condition is asked once per iteration and never before the first.
        assert_code(
            &code(&lowered("let s = 0; do { s = s + 1; } while (s < 5);")),
            "
            load_int r0, 0
            load_int r1, 1
            add r0, r0, r1, ic0
            load_int r1, 5
            less r1, r0, r1, ic1
            jump_if_false r1, @7
            loop_back_edge @1, ic2
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_continue_in_a_do_while_goes_to_the_test_and_not_past_it() {
        // The `continue` at @3 goes to @4, which is the first instruction of the condition rather
        // than the back edge below it. Landing on the back edge would make `do { continue; } while
        // (false)` an endless loop, which is the one way this can be wrong and still look right.
        assert_code(
            &code(&lowered(
                "let s = 0; do { s = s + 1; continue; } while (s < 5);",
            )),
            "
            load_int r0, 0
            load_int r1, 1
            add r0, r0, r1, ic0
            jump @4
            load_int r1, 5
            less r1, r0, r1, ic1
            jump_if_false r1, @8
            loop_back_edge @1, ic2
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_labelled_break_leaves_every_loop_between_it_and_the_one_it_names() {
        // The `break outer` at @4 goes to @7, which is past the outer back edge at @6 and not past
        // the inner one at @5. An unlabelled `break` in the same place would go to @6, so the label
        // is worth exactly one frame of the walk outwards, and that is the whole feature.
        assert_code(
            &code(&lowered("outer: while (a) { while (b) { break outer; } }")),
            "
            load_global r0, k0, ic0
            jump_if_false r0, @7
            load_global r0, k1, ic1
            jump_if_false r0, @6
            jump @7
            loop_back_edge @2, ic2
            loop_back_edge @0, ic3
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn a_labelled_continue_lands_on_the_update_of_the_loop_it_names() {
        // @6 goes to @8, the `to_number` that starts the outer update, rather than to @7 which is
        // the inner back edge or to @10 which is the outer one. A labelled `continue` has to find
        // the continue target of a specific frame, and that target is not where its `break` goes.
        assert_code(
            &code(&lowered(
                "outer: for (let i = 0; i < 2; i++) { while (b) { continue outer; } }",
            )),
            "
            load_int r0, 0
            load_int r1, 2
            less r1, r0, r1, ic0
            jump_if_false r1, @11
            load_global r1, k0, ic1
            jump_if_false r1, @8
            jump @8
            loop_back_edge @4, ic2
            to_number r1, r0, ic3
            inc r0, r1, ic4
            loop_back_edge @1, ic5
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_label_on_something_that_is_not_a_loop_costs_nothing_until_a_break_uses_it() {
        // A labelled block is a frame that only collects jumps, so it emits no instruction of its
        // own and the `break done` at @2 is a plain jump to the end of the block at @5. This is the
        // early exit an if with no else would otherwise need a flag for.
        assert_code(
            &code(&lowered("done: { if (a) { break done; } f(); }")),
            "
            load_global r0, k0, ic0
            jump_if_false r0, @3
            jump @5
            load_global r0, k1, ic1
            call r0, r0, r1, 0, ic2
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn a_chain_of_labels_all_name_the_same_statement() {
        // Both names are on the one loop, so `break a` is the same jump `break b` would be, and
        // neither of them builds a frame of its own around the other.
        assert_code(
            &code(&lowered("a: b: while (x) { break a; }")),
            "
            load_global r0, k0, ic0
            jump_if_false r0, @4
            jump @4
            loop_back_edge @0, ic1
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn two_labelled_breaks_through_one_finally_travel_as_two_different_tokens() {
        // The case an ordinary completion token cannot survive. Both breaks route through the same
        // `finally`, and by the time the dispatch after it runs, the frame each one was aimed at is
        // gone, so the token is the only thing left that says which. `break a` sets 5 at @6 and
        // `break b` sets 6 at @9, and the dispatch tells them apart at @20 to @22: 5 goes to @25
        // and out past the outer back edge, and 6 falls through to @23 and out past the inner one.
        //
        // Nothing was added to the dispatch a plain `finally` emits. The two comparisons here are
        // throw and one label, because normal is the falsy token the `jump_if_false` at @16 lets
        // out and the last arm needs no comparison, so a `finally` with no labelled jump through it
        // compares exactly the numbers it always did.
        assert_code(
            &code(&lowered(
                "a: while (p) { b: while (q) { try { if (r) break a; else break b; } finally { g(); } } }",
            )),
            "
            load_global r0, k0, ic0
            jump_if_false r0, @28
            load_global r0, k1, ic1
            jump_if_false r0, @27
            load_global r2, k2, ic2
            jump_if_false r2, @9
            load_int r0, 5
            jump @14
            jump @11
            load_int r0, 6
            jump @14
            load_int r0, 0
            jump @14
            load_int r0, 1
            load_global r2, k3, ic3
            call r2, r2, r3, 0, ic4
            jump_if_false r0, @26
            load_int r2, 1
            strict_equal r3, r0, r2, ic5
            jump_if_true r3, @24
            load_int r2, 5
            strict_equal r3, r0, r2, ic6
            jump_if_true r3, @25
            jump @27
            throw r1
            jump @28
            loop_back_edge @2, ic7
            loop_back_edge @0, ic8
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn a_switch_compares_every_case_first_and_then_lays_the_bodies_out_in_order() {
        assert_code(
            &code(&lowered(
                "let x = 2; switch (x) { case 1: x = 10; case 2: x = 20; }",
            )),
            "
            load_int r0, 2
            move r1, r0
            load_int r2, 1
            strict_equal r3, r1, r2, ic0
            jump_if_true r3, @9
            load_int r2, 2
            strict_equal r3, r1, r2, ic1
            jump_if_true r3, @10
            jump @11
            load_int r0, 10
            load_int r0, 20
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn falling_out_of_one_clause_falls_into_the_next_and_a_break_leaves_the_switch() {
        // The two jump targets are the whole test. The `break` at @7 goes to @9, which is past the
        // default clause rather than into it, and the default at @8 is reached from @5 by falling
        // off the end of the comparisons rather than by a jump of its own.
        assert_code(
            &code(&lowered(
                "let x = 1; switch (x) { case 1: x = 10; break; default: x = 20; }",
            )),
            "
            load_int r0, 1
            move r1, r0
            load_int r2, 1
            strict_equal r3, r1, r2, ic0
            jump_if_true r3, @6
            jump @8
            load_int r0, 10
            jump @9
            load_int r0, 20
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_default_in_the_middle_is_still_compared_against_last() {
        // The comparison for `case 2` is emitted even though `default` was written before it, and
        // the miss jump at @8 goes to @10, which is the default's body sitting between the two
        // case bodies. Every other jump in the lowerer is patched to wherever the next instruction
        // is going to land, and this is the one that is not, which is what `patch_to` is for.
        assert_code(
            &code(&lowered(
                "let x = 2; switch (x) { case 1: x = 10; default: x = 20; case 2: x = 30; }",
            )),
            "
            load_int r0, 2
            move r1, r0
            load_int r2, 1
            strict_equal r3, r1, r2, ic0
            jump_if_true r3, @9
            load_int r2, 2
            strict_equal r3, r1, r2, ic1
            jump_if_true r3, @11
            jump @10
            load_int r0, 10
            load_int r0, 20
            load_int r0, 30
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn continue_jumps_to_the_back_edge_and_not_to_the_top_of_the_loop() {
        // Going to the top directly would skip the back edge, and the back edge is the instruction
        // that counts iterations for tiering, so a loop whose body always continued would look
        // cold no matter how long it ran.
        assert_code(
            &code(&lowered(
                "let i = 0; while (i < 10) { i = i + 1; continue; }",
            )),
            "
            load_int r0, 0
            load_int r1, 10
            less r1, r0, r1, ic0
            jump_if_false r1, @8
            load_int r1, 1
            add r0, r0, r1, ic1
            jump @7
            loop_back_edge @1, ic2
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_break_in_a_loop_leaves_the_loop_and_not_just_the_iteration() {
        assert_code(
            &code(&lowered("let i = 0; while (i < 10) { break; }")),
            "
            load_int r0, 0
            load_int r1, 10
            less r1, r0, r1, ic0
            jump_if_false r1, @6
            jump @6
            loop_back_edge @1, ic1
            load_undefined r1
            return r1
            ",
        );
    }

    #[test]
    fn a_continue_inside_a_switch_inside_a_loop_leaves_both() {
        // A switch catches `break` and does not catch `continue`, so the two statements in this
        // body go to different places. The `break` at @13 goes to @14, which is past the switch
        // and still inside the loop, so `i = i + 1` runs. The `continue` at @12 goes to @16, the
        // back edge, so it does not.
        assert_code(
            &code(&lowered(
                "let i = 0; while (i < 10) { switch (i) { case 1: continue; case 2: break; } i = i + 1; }",
            )),
            "
            load_int r0, 0
            load_int r1, 10
            less r1, r0, r1, ic0
            jump_if_false r1, @17
            move r1, r0
            load_int r2, 1
            strict_equal r3, r1, r2, ic1
            jump_if_true r3, @12
            load_int r2, 2
            strict_equal r3, r1, r2, ic2
            jump_if_true r3, @13
            jump @14
            jump @16
            jump @14
            load_int r1, 1
            add r0, r0, r1, ic3
            loop_back_edge @1, ic4
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
    fn entering_a_try_costs_nothing() {
        // The claim the whole design rests on. There is no instruction for entering the protected
        // block and none for leaving it, so a `try` around a hot loop runs at the speed of the loop.
        // The only instruction the shape adds is the jump over the handler, and a program that does
        // not throw is going to run that jump exactly once.
        let blueprint = lowered("try { f(); } catch (e) { g(); }");
        assert_code(
            &code(&blueprint),
            "
            load_global r1, k0, ic0
            call r1, r1, r2, 0, ic1
            jump @5
            load_global r1, k1, ic2
            call r1, r1, r2, 0, ic3
            load_undefined r1
            return r1
            ",
        );
        assert_eq!(blueprint.handlers.len(), 1);
        let handler = blueprint.handlers[0];
        assert_eq!(
            (handler.start.0, handler.end.0, handler.target.0),
            (0, 2, 3),
            "the range covers the protected block and stops before the jump over the handler"
        );
    }

    #[test]
    fn the_handler_range_stops_before_the_handler() {
        // Which is what stops a throw inside a `catch` from being caught by the `catch` it is in.
        // The handler's own instructions are outside every range that names it.
        let blueprint = lowered("try { f(); } catch (e) { throw e; }");
        let handler = blueprint.handlers[0];
        assert!(
            handler.target.0 >= handler.end.0,
            "the handler starts at or after the end of the range it handles, {handler:?}"
        );
    }

    #[test]
    fn a_nested_try_lands_earlier_in_the_table_than_the_one_around_it() {
        // Order is the whole of the search rule, so this is the test that says innermost wins. The
        // entry goes in when the block it protects has been lowered, and the inner block finishes
        // first, which is what puts it first without anything comparing two ranges.
        let blueprint = lowered("try { try { f(); } catch (a) { g(); } } catch (b) { h(); }");
        assert_eq!(blueprint.handlers.len(), 2);
        let [inner, outer] = [blueprint.handlers[0], blueprint.handlers[1]];
        assert!(
            outer.start <= inner.start && inner.end <= outer.end,
            "the inner range is inside the outer one, {inner:?} and {outer:?}"
        );
    }

    #[test]
    fn a_try_with_nothing_in_it_gets_no_entry() {
        // An empty protected block cannot throw, so an entry for it could never fire. `verify`
        // rejects an empty range for the same reason, which is why this is not merely tidiness.
        assert!(lowered("try {} catch (e) { f(); }").handlers.is_empty());
    }

    #[test]
    fn a_catch_with_no_binding_still_names_a_register() {
        // The search stores the thrown value before it knows whether anybody wanted it, so there
        // has to be somewhere for it to land. A register nothing reads is cheaper than a second
        // kind of table entry that says the value is not wanted.
        let blueprint = lowered("try { f(); } catch { g(); }");
        assert_eq!(blueprint.handlers.len(), 1);
        assert!(blueprint.handlers[0].register.0 < blueprint.frame_size);
    }

    #[test]
    fn a_caught_value_goes_straight_into_the_slot_it_is_bound_to() {
        // No copy at the top of the clause, because a parameter nothing captures is an ordinary
        // frame slot and the search can write into it. The first instruction of the handler is the
        // first instruction of the body.
        let blueprint = lowered("try { f(); } catch (e) { g(e); }");
        let handler = blueprint.handlers[0];
        assert_eq!(
            handler.register,
            katsu_ir::Register(0),
            "the slot the binding got, {}",
            code(&blueprint)
        );
        let at = handler.target.0 as usize;
        assert!(
            format!("{}", blueprint.code[at]).starts_with("load_global"),
            "the clause starts with its body, {}",
            code(&blueprint)
        );
    }

    #[test]
    fn a_captured_catch_parameter_arrives_in_a_temporary_and_is_copied_into_its_cell() {
        // The other of the two cases, and the same two a function parameter has. A captured name
        // lives in a cell rather than a slot, and the search cannot write into a cell, so the value
        // lands in a register and the first instruction of the handler moves it across.
        let blueprint = lowered("try { f(); } catch (e) { function g() { return e; } }");
        let handler = blueprint.handlers[0];
        let at = handler.target.0 as usize;
        assert!(
            format!("{}", blueprint.code[at]).starts_with("store_upvalue"),
            "the clause starts by putting the caught value in its cell, {}",
            code(&blueprint)
        );
    }

    #[test]
    fn throw_is_one_instruction_and_ends_the_code_it_is_last_in() {
        // No implicit return after it, because `throw` is a terminator and the block is over.
        let blueprint = lowered("throw 1;");
        assert_code(
            &code(&blueprint),
            "
            load_int r0, 1
            throw r0
            ",
        );
    }

    #[test]
    fn a_finally_that_only_the_normal_path_reaches_costs_two_instructions_of_dispatch() {
        // The shape the overwhelming majority of `finally` clauses have. Nothing inside the block
        // returns or breaks or continues, so the only completions that can arrive are normal and
        // throw, and the dispatch is the `jump_if_false` that lets the normal one out plus the
        // `throw` that is the only other thing it could have been. There is no comparison at all,
        // because a token that is not zero can only be the one remaining kind.
        let blueprint = lowered("try { f(); } finally { g(); }");
        assert_code(
            &code(&blueprint),
            "
            load_global r2, k0, ic0
            call r2, r2, r3, 0, ic1
            load_int r0, 0
            jump @5
            load_int r0, 1
            load_global r2, k1, ic2
            call r2, r2, r3, 0, ic3
            jump_if_false r0, @9
            throw r1
            load_undefined r0
            return r0
            ",
        );
        assert_eq!(blueprint.handlers.len(), 1);
        let handler = blueprint.handlers[0];
        assert_eq!(
            (handler.start.0, handler.end.0, handler.target.0),
            (0, 2, 4),
            "the range covers the block and lands on the instruction that sets the token"
        );
        assert_eq!(
            handler.register,
            katsu_ir::Register(1),
            "the search stores the thrown value in the payload register the dispatch rethrows"
        );
    }

    #[test]
    fn a_finally_around_a_return_asks_about_the_return_as_well() {
        // One more kind can arrive, so the dispatch grows the one comparison that tells the two
        // abrupt kinds apart, and the `return` inside the block becomes a move into the payload and
        // a jump into the body rather than a `return` instruction.
        let blueprint = lowered("function f() { try { return 1; } finally { g(); } }");
        let inner = &blueprint.blueprints[0];
        assert_code(
            &code(inner),
            "
            load_int r2, 1
            move r1, r2
            load_int r0, 2
            jump @7
            load_int r0, 0
            jump @7
            load_int r0, 1
            load_global r2, k0, ic0
            call r2, r2, r3, 0, ic1
            jump_if_false r0, @15
            load_int r2, 1
            strict_equal r3, r0, r2, ic2
            jump_if_true r3, @14
            return r1
            throw r1
            load_undefined r0
            return r0
            ",
        );
    }

    #[test]
    fn a_try_catch_finally_is_lowered_as_a_try_catch_inside_a_try_finally() {
        // Which is how the standard defines it, and it gets the table order right on its own: the
        // catch's entry is pushed when its protected block finishes, and the finally's when the
        // whole `try` and `catch` finishes, so a throw in the block finds the catch and a throw in
        // the catch finds the finally.
        let blueprint = lowered("try { f(); } catch (e) { g(); } finally { h(); }");
        assert_eq!(blueprint.handlers.len(), 2);
        let [catch, finally] = [blueprint.handlers[0], blueprint.handlers[1]];
        assert!(
            finally.start <= catch.start && catch.end <= finally.end,
            "the catch's range is inside the finally's, {catch:?} and {finally:?}"
        );
        assert!(
            catch.target < finally.end,
            "the finally's range covers the catch clause too, {catch:?} and {finally:?}"
        );
    }

    #[test]
    fn a_finally_around_an_empty_block_is_just_its_body() {
        // Nothing can throw and nothing can leave, so the token, the table entry and the dispatch
        // would all be machinery around a body that simply runs. `try {} finally { f(); }` is a
        // call and nothing else.
        let blueprint = lowered("try {} finally { f(); }");
        assert!(blueprint.handlers.is_empty());
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
    fn a_break_inside_a_finally_leaves_the_loop_rather_than_routing_back_in() {
        // The frame is popped before the body is lowered, so a `break` written in the body is
        // lowered against the loop outside the construct. Getting this backwards is what would turn
        // `try { } finally { break; }` into a loop that never ends.
        let blueprint = lowered("while (f()) { try { g(); } finally { break; } }");
        assert!(
            !code(&blueprint).contains("strict_equal"),
            "the break left rather than routing through the dispatch, {}",
            code(&blueprint)
        );
    }

    #[test]
    fn a_return_through_two_finallys_is_re_issued_by_the_inner_dispatch() {
        // Nesting needs nothing that knows about nesting. The inner dispatch emits the return the
        // same way an ordinary statement would, and because the outer frame is still on the stack
        // at that point, it routes into the outer body on its own.
        let blueprint =
            lowered("function f() { try { try { return 1; } finally { g(); } } finally { h(); } }");
        let inner = &blueprint.blueprints[0];
        assert_eq!(inner.handlers.len(), 2, "one entry each, {}", code(inner));
        let [first, second] = [inner.handlers[0], inner.handlers[1]];
        let listing = code(inner);
        assert!(
            listing.contains(&format!(
                "move r{}, r{}",
                second.register.0, first.register.0
            )),
            "the inner arm hands the returned value to the outer payload, {listing}"
        );
        assert_eq!(
            listing.matches("return r").count(),
            2,
            "one return for the outer dispatch's arm and one for the implicit end, {listing}"
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
