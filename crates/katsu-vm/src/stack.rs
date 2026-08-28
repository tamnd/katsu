//! The JavaScript stack, which is ours and not Rust's.
//!
//! Spec 5.4 gives three reasons for allocating our own, and all three are load bearing. Root
//! scanning has to find every live reference, and a contiguous region of values with a known extent
//! is a walk rather than a conservative guess. A program that recurses fifty thousand deep has to
//! raise a `RangeError` and not fault, which means the limit has to be ours to choose. And
//! generators, on stack replacement and deoptimisation all construct frames from outside the
//! interpreter, which is writing into an array here and is close to impossible on the Rust stack.
//!
//! The region is address space we reserve once and commit as the stack grows. Reserving is free and
//! committing is what the memory budget in spec 02.3 counts, so a program that never nests deeply
//! pays for the pages it touched and not for the depth it could have reached. Pages are never given
//! back on the way down, because a program that recursed once will usually recurse again and the
//! syscall to hand a page back is more expensive than the page.
//!
//! # Where the frame header went
//!
//! Spec 5.4 draws the frame with its header inline, the saved program counter and the caller's frame
//! pointer sitting in the region next to the values. That is what a C engine does and there is a good
//! reason for it there: it is one allocation and one pointer to follow.
//!
//! Here the header lives in a separate vector and only values live in the region. The reason is the
//! first of the three above. If the header is inline then every slot in the region is a value except
//! the ones that are not, and the root scanner has to walk frame by frame, read each blueprint to
//! learn the frame size, and skip the right number of words at the right offsets. Get that wrong by
//! one and the collector either traces a saved program counter as if it were a pointer or misses a
//! live object, and both of those are the kind of bug that shows up as a crash somewhere else three
//! days later. With the split, `roots` is a slice and there is nothing to get wrong.
//!
//! The cost is a second allocation and a second cache line touched per call. That is a real cost and
//! it is measured rather than assumed, in `benches/stack.rs`. If it ever matters more than the
//! safety does, the header can move back into the region and the scanner can learn to walk frames,
//! and the tests here will not change.

use katsu_ir::Register;
use katsu_platform::{Reservation, ReservationError, page_size, round_up_to_page};

use crate::Value;

/// Bytes of address space reserved for the stack.
///
/// Eight megabytes, which is one mebislot at eight bytes each. Reserving costs address space and
/// nothing else, so this is sized for the deepest recursion we are willing to allow rather than for
/// the memory a typical program uses.
const RESERVED_BYTES: usize = 8 << 20;

/// How much is committed at a time when the stack grows past what it has.
///
/// Sixty four kilobytes, which is eight thousand slots and roughly a thousand small frames. Small
/// enough that a program which never calls anything costs one chunk, large enough that a recursive
/// program is not making a syscall every few frames.
const COMMIT_CHUNK: usize = 64 << 10;

/// How many frames may be live at once before the stack is declared exhausted.
///
/// Node raises `RangeError: Maximum call stack size exceeded` at around eleven thousand frames on a
/// default thread stack. This is in the same range on purpose. A program written against Node that
/// recurses to just under its limit should not fail here, and a program that runs away should fail
/// at a comparable point rather than after eating a hundred times the memory.
///
/// It is a count and not a byte total because that is the number a JavaScript program can reason
/// about. The byte total is bounded too, by the reservation, and running out of that raises the same
/// error.
const MAX_DEPTH: usize = 10_000;

/// Why an operation on the stack could not be performed.
#[derive(Debug, thiserror::Error)]
pub enum StackError {
    /// The address space for the stack could not be reserved at startup.
    #[error("could not reserve the JavaScript stack")]
    Reserve(#[from] ReservationError),
    /// Too many frames, or a frame too large for what is left.
    ///
    /// This is the `RangeError` a JavaScript program sees, and it is an ordinary error here rather
    /// than a panic because a program is allowed to catch it and carry on.
    #[error("maximum call stack size exceeded")]
    Overflow,
}

/// The bookkeeping for one live call, everything about a frame that is not a value.
///
/// The interpreter reads and writes these directly, which is why the fields are public. There is
/// nothing to encapsulate: a frame is four numbers and an invariant that the stack maintains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Index into the value region where this frame's register zero lives.
    pub base: usize,
    /// How many value slots this frame owns, which is the blueprint's frame size.
    pub size: u16,
    /// The instruction in the caller to resume at when this frame returns.
    ///
    /// Meaningless for the bottom frame, which returns to the embedder rather than to bytecode.
    pub return_pc: u32,
    /// The register in the caller's frame that this frame's return value is written into.
    pub return_to: Register,
}

/// A contiguous region of value slots, carved into frames.
///
/// Not `Clone`, and deliberately not `Default`, because constructing one asks the operating system
/// for address space and that can fail. A stack is created once per isolate and lives as long as it
/// does.
#[derive(Debug)]
pub struct Stack {
    region: Reservation,
    /// Slots that have been committed and are therefore safe to read and write.
    committed: usize,
    /// The first slot not owned by any frame.
    top: usize,
    /// One entry per live call, innermost last.
    frames: Vec<Frame>,
}

impl Stack {
    /// Reserve the address space for a stack and commit the first chunk of it.
    ///
    /// # Errors
    ///
    /// Returns [`StackError::Reserve`] if the operating system refuses the reservation, which in
    /// practice means the process is out of address space and nothing else will work either.
    pub fn new() -> Result<Stack, StackError> {
        let region = Reservation::reserve(RESERVED_BYTES, page_size())?;
        let mut stack = Stack {
            region,
            committed: 0,
            top: 0,
            frames: Vec::new(),
        };
        // One chunk up front, so that the first call does not pay for a syscall on a path that is
        // otherwise a pointer bump.
        stack.commit_through(COMMIT_CHUNK / size_of::<Value>())?;
        Ok(stack)
    }

    /// How many frames are live.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Whether there is no frame at all, which is true before the first call and after the last
    /// return.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The innermost frame, or `None` if nothing is running.
    #[must_use]
    pub fn current(&self) -> Option<&Frame> {
        self.frames.last()
    }

    /// The innermost frame, mutably, so the interpreter can save a program counter into it.
    #[must_use]
    pub fn current_mut(&mut self) -> Option<&mut Frame> {
        self.frames.last_mut()
    }

    /// Every frame, outermost first, which is the order a stack trace prints in reverse.
    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// Bytes this stack has committed, which is what the memory budget counts.
    ///
    /// The reservation is not in this number. Address space is free and counting it would make the
    /// idle figure in spec 02.3 look eight megabytes worse than the process actually is.
    #[must_use]
    pub fn committed_bytes(&self) -> usize {
        self.committed * size_of::<Value>()
    }

    /// Every value slot that belongs to a live frame.
    ///
    /// This is the root set, and the reason the header is not stored inline. A collector walks this
    /// slice and every element in it is a value, with no frame boundaries to respect and no words to
    /// skip. See the note at the top of this module.
    #[must_use]
    pub fn roots(&self) -> &[Value] {
        // SAFETY: slots below `top` belong to a live frame, every one of them was written by `push`
        // before the frame became current, and `top` never exceeds `committed`.
        unsafe { std::slice::from_raw_parts(self.slots(), self.top) }
    }

    /// Push a frame of `size` slots, filling it from `args`.
    ///
    /// The first `args.len()` slots take the argument values, in the order the caller evaluated
    /// them, because the calling convention in spec 4.5 puts argument `n` in register `n`. Every
    /// remaining slot becomes `undefined`, which covers both a call that passed fewer arguments than
    /// the function declares and the ordinary registers above the parameters.
    ///
    /// Initialising the whole frame is not optional and it is not only about correctness of a read
    /// before a write. A slot left holding whatever the previous frame put there is a pointer the
    /// collector would trace to an object that is no longer reachable, which is how a dead object
    /// stays alive and, once the collector moves things, how a stale pointer gets followed.
    ///
    /// # Errors
    ///
    /// Returns [`StackError::Overflow`] if the frame would exceed the depth limit or run past the
    /// reservation, and [`StackError::Reserve`] if committing the pages for it fails.
    pub fn push(
        &mut self,
        size: u16,
        args: &[Value],
        return_pc: u32,
        return_to: Register,
    ) -> Result<(), StackError> {
        if self.frames.len() >= MAX_DEPTH {
            return Err(StackError::Overflow);
        }
        let size_slots = usize::from(size);
        debug_assert!(
            args.len() <= size_slots,
            "a call passed {} arguments into a frame of {size} slots, which means lowering sized \
             the frame without counting the parameters",
            args.len()
        );
        let base = self.top;
        let end = base
            .checked_add(size_slots)
            .filter(|end| *end <= self.capacity())
            .ok_or(StackError::Overflow)?;
        self.commit_through(end)?;

        // SAFETY: `base..end` is inside the region and committed, so every slot is mapped writable,
        // and nothing else holds a reference into the region while this runs.
        let slots = unsafe { std::slice::from_raw_parts_mut(self.slots().add(base), size_slots) };
        let (arguments, rest) = slots.split_at_mut(args.len());
        arguments.copy_from_slice(args);
        rest.fill(Value::UNDEFINED);

        self.top = end;
        self.frames.push(Frame {
            base,
            size,
            return_pc,
            return_to,
        });
        Ok(())
    }

    /// Pop the innermost frame and return it, or `None` if there was nothing to pop.
    ///
    /// The slots are not cleared here. They stop being roots the moment `top` moves down, which is
    /// what liveness means, and the next `push` overwrites all of them before anything can read one.
    /// Clearing as well would be paying twice for the same guarantee.
    pub fn pop(&mut self) -> Option<Frame> {
        let frame = self.frames.pop()?;
        self.top = frame.base;
        Some(frame)
    }

    /// Read a register in the innermost frame.
    ///
    /// # Panics
    ///
    /// Panics if there is no frame, or in a debug build if the register is past the frame's size.
    /// Both are lowering bugs rather than program errors: the verifier in `katsu-ir` rejects a
    /// blueprint whose registers exceed its frame size, so reaching either of these means bytecode
    /// got here without being verified.
    #[must_use]
    pub fn get(&self, register: Register) -> Value {
        let index = self.slot_of(register);
        // SAFETY: `slot_of` checked the register against the current frame, and every slot below
        // `top` is committed and initialised.
        unsafe { self.slots().add(index).read() }
    }

    /// Write a register in the innermost frame.
    ///
    /// # Panics
    ///
    /// As [`Stack::get`].
    pub fn set(&mut self, register: Register, value: Value) {
        let index = self.slot_of(register);
        // SAFETY: as `get`, and `&mut self` means nothing else is reading the region.
        unsafe { self.slots().add(index).write(value) };
    }

    /// A run of registers in the innermost frame, which is how a call collects its arguments.
    ///
    /// Consecutive rather than gathered, because that is the constraint the calling convention puts
    /// on the register allocator, and taking a slice here is what makes `push` a copy instead of a
    /// loop.
    ///
    /// # Panics
    ///
    /// Panics if there is no frame, or in a debug build if the run leaves the frame.
    #[must_use]
    pub fn range(&self, first: Register, count: u16) -> &[Value] {
        let frame = self.frames.last().expect("no frame is running");
        let start = usize::from(first.0);
        let end = start + usize::from(count);
        debug_assert!(
            end <= usize::from(frame.size),
            "registers {first}..+{count} leave a frame of {} slots",
            frame.size
        );
        // SAFETY: the run is inside the current frame, which is inside the committed region, and
        // every slot in a live frame was initialised by `push`.
        unsafe {
            std::slice::from_raw_parts(self.slots().add(frame.base + start), usize::from(count))
        }
    }

    /// The region, as the array of values it is.
    ///
    /// The one place the byte pointer from the reservation becomes a value pointer. A reservation
    /// comes back page aligned, and a page is at least four kilobytes on every platform we build
    /// for, so the base is aligned for anything. Doing the cast once here means that argument is
    /// written down once instead of five times, and the assertion in `new` is what turns a future
    /// platform that breaks it into a failure at startup rather than a misaligned load later.
    #[allow(clippy::cast_ptr_alignment)]
    const fn slots(&self) -> *mut Value {
        self.region.base().cast::<Value>()
    }

    /// The absolute slot index a register in the innermost frame names.
    fn slot_of(&self, register: Register) -> usize {
        let frame = self.frames.last().expect("no frame is running");
        debug_assert!(
            register.0 < frame.size,
            "{register} is past a frame of {} slots, which a verified blueprint cannot produce",
            frame.size
        );
        frame.base + usize::from(register.0)
    }

    /// How many slots the reservation holds.
    fn capacity(&self) -> usize {
        self.region.len() / size_of::<Value>()
    }

    /// Make sure slots up to `slots` are committed, rounding up to whole chunks.
    fn commit_through(&mut self, slots: usize) -> Result<(), StackError> {
        if slots <= self.committed {
            return Ok(());
        }
        let wanted = round_up_to_chunk(slots * size_of::<Value>()).min(self.region.len());
        self.region.commit(0, wanted)?;
        self.committed = wanted / size_of::<Value>();
        Ok(())
    }
}

/// Round a byte count up to a whole commit chunk, and then to a whole page.
///
/// Both roundings, because the chunk is a policy about how often we call the kernel and the page is
/// what the kernel actually works in. On a machine with sixteen kilobyte pages the chunk is already
/// a multiple of the page, and on one with a page larger than the chunk the page rounding is what
/// keeps the request legal.
fn round_up_to_chunk(bytes: usize) -> usize {
    let chunks = bytes.div_ceil(COMMIT_CHUNK) * COMMIT_CHUNK;
    round_up_to_page(chunks)
}

#[cfg(test)]
mod tests {
    use katsu_ir::Register;

    use super::{COMMIT_CHUNK, MAX_DEPTH, Stack, StackError};
    use crate::Value;

    /// A frame with no arguments, which is what most of these tests want.
    fn push(stack: &mut Stack, size: u16) {
        stack
            .push(size, &[], 0, Register(0))
            .expect("the stack should have room");
    }

    #[test]
    fn a_fresh_stack_has_committed_one_chunk_and_nothing_more() {
        let stack = Stack::new().expect("should reserve");
        assert_eq!(stack.committed_bytes(), COMMIT_CHUNK);
        assert_eq!(stack.depth(), 0);
        assert!(stack.is_empty());
        assert!(stack.roots().is_empty());
    }

    #[test]
    fn a_frame_starts_out_undefined_rather_than_holding_what_was_there_before() {
        // The property root scanning depends on. A slot left holding the previous frame's pointer
        // is an object the collector would keep alive and, once it moves things, a pointer it would
        // follow after the object it named had gone.
        let mut stack = Stack::new().expect("should reserve");
        push(&mut stack, 4);
        for slot in 0..4 {
            stack.set(
                Register(slot),
                Value::from_pointer(0x1000 + u64::from(slot)),
            );
        }
        stack.pop();

        push(&mut stack, 4);
        for slot in 0..4 {
            assert_eq!(
                stack.get(Register(slot)),
                Value::UNDEFINED,
                "r{slot} came back holding the dead frame's value"
            );
        }
    }

    #[test]
    fn arguments_land_in_the_registers_the_calling_convention_names() {
        let mut stack = Stack::new().expect("should reserve");
        let args = [
            Value::from_i32(10),
            Value::from_i32(20),
            Value::from_i32(30),
        ];
        stack.push(6, &args, 0, Register(0)).expect("should push");

        assert_eq!(stack.get(Register(0)), Value::from_i32(10));
        assert_eq!(stack.get(Register(1)), Value::from_i32(20));
        assert_eq!(stack.get(Register(2)), Value::from_i32(30));
        // Everything past the arguments, which covers both the registers and a parameter the caller
        // did not pass.
        assert_eq!(stack.get(Register(3)), Value::UNDEFINED);
        assert_eq!(stack.get(Register(5)), Value::UNDEFINED);
    }

    #[test]
    fn a_register_reads_back_what_was_written_to_it() {
        let mut stack = Stack::new().expect("should reserve");
        push(&mut stack, 3);
        stack.set(Register(1), Value::from_double(1.5));
        assert_eq!(stack.get(Register(1)), Value::from_double(1.5));
        assert_eq!(stack.get(Register(0)), Value::UNDEFINED);
    }

    #[test]
    fn registers_are_relative_to_the_frame_and_not_to_the_region() {
        // Two frames both writing r0 have to be writing different slots, which is the entire point
        // of a frame base.
        let mut stack = Stack::new().expect("should reserve");
        push(&mut stack, 2);
        stack.set(Register(0), Value::from_i32(1));

        push(&mut stack, 2);
        stack.set(Register(0), Value::from_i32(2));
        assert_eq!(stack.get(Register(0)), Value::from_i32(2));

        stack.pop();
        assert_eq!(stack.get(Register(0)), Value::from_i32(1));
    }

    #[test]
    fn a_frame_records_where_to_return_to() {
        let mut stack = Stack::new().expect("should reserve");
        stack.push(2, &[], 7, Register(3)).expect("should push");
        let frame = *stack.current().expect("a frame is running");
        assert_eq!(frame.return_pc, 7);
        assert_eq!(frame.return_to, Register(3));
        assert_eq!(frame.size, 2);
        assert_eq!(stack.pop(), Some(frame));
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn the_root_set_is_every_slot_in_every_live_frame_and_nothing_else() {
        let mut stack = Stack::new().expect("should reserve");
        push(&mut stack, 2);
        stack.set(Register(0), Value::from_i32(1));
        stack.set(Register(1), Value::from_i32(2));
        push(&mut stack, 1);
        stack.set(Register(0), Value::from_i32(3));

        assert_eq!(
            stack.roots(),
            [Value::from_i32(1), Value::from_i32(2), Value::from_i32(3)]
        );

        stack.pop();
        assert_eq!(stack.roots(), [Value::from_i32(1), Value::from_i32(2)]);
        stack.pop();
        assert!(stack.roots().is_empty());
    }

    #[test]
    fn a_run_of_registers_comes_back_as_a_slice_a_call_can_copy() {
        let mut stack = Stack::new().expect("should reserve");
        push(&mut stack, 5);
        for slot in 0..5 {
            stack.set(Register(slot), Value::from_i32(i32::from(slot)));
        }
        assert_eq!(
            stack.range(Register(1), 3),
            [Value::from_i32(1), Value::from_i32(2), Value::from_i32(3)]
        );
        assert_eq!(stack.range(Register(0), 0), []);
    }

    #[test]
    fn a_call_copies_its_arguments_out_of_the_caller_and_into_the_callee() {
        // The shape every call has, written out once, because the two frames live in the same
        // region and the copy between them is the thing most likely to be wrong.
        let mut stack = Stack::new().expect("should reserve");
        push(&mut stack, 4);
        stack.set(Register(2), Value::from_i32(41));
        stack.set(Register(3), Value::from_i32(42));

        let args = stack.range(Register(2), 2).to_vec();
        stack.push(2, &args, 9, Register(0)).expect("should push");

        assert_eq!(stack.get(Register(0)), Value::from_i32(41));
        assert_eq!(stack.get(Register(1)), Value::from_i32(42));
    }

    #[test]
    fn running_out_of_depth_is_an_error_a_program_can_catch_and_not_a_crash() {
        // The reason the stack is ours. On the Rust stack this is a fault the process does not
        // survive, and in JavaScript it is a RangeError that a try block is allowed to catch.
        let mut stack = Stack::new().expect("should reserve");
        for _ in 0..MAX_DEPTH {
            push(&mut stack, 1);
        }
        assert!(matches!(
            stack.push(1, &[], 0, Register(0)),
            Err(StackError::Overflow)
        ));
        assert_eq!(stack.depth(), MAX_DEPTH);

        // And it keeps working afterwards, because nothing was left half pushed.
        stack.pop();
        push(&mut stack, 1);
        assert_eq!(stack.depth(), MAX_DEPTH);
    }

    #[test]
    fn a_frame_too_large_for_the_reservation_is_refused_rather_than_committed() {
        let mut stack = Stack::new().expect("should reserve");
        assert!(matches!(stack.push(u16::MAX, &[], 0, Register(0)), Ok(())));
        stack.pop();
        // The reservation is eight megabytes and a frame is at most sixty five thousand slots, so
        // exhausting it takes many frames rather than one. Pushing the largest frame there is until
        // it refuses gets there without depending on the exact reservation size.
        let mut pushed = 0;
        while stack.push(u16::MAX, &[], 0, Register(0)).is_ok() {
            pushed += 1;
            assert!(pushed < MAX_DEPTH, "the reservation should run out first");
        }
        assert!(pushed > 0);
    }

    #[test]
    fn the_stack_commits_as_it_grows_and_keeps_the_pages_on_the_way_down() {
        let mut stack = Stack::new().expect("should reserve");
        let idle = stack.committed_bytes();

        for _ in 0..8 {
            push(&mut stack, u16::MAX);
        }
        let deep = stack.committed_bytes();
        assert!(deep > idle, "growing the stack should commit pages");

        while stack.pop().is_some() {}
        assert_eq!(
            stack.committed_bytes(),
            deep,
            "pages are kept, because a program that recursed once usually recurses again and \
             handing a page back costs more than the page"
        );
    }

    #[test]
    fn a_stack_can_be_moved_to_another_thread() {
        // An isolate is Send, so everything it owns has to be.
        let mut stack = Stack::new().expect("should reserve");
        push(&mut stack, 2);
        stack.set(Register(0), Value::from_i32(5));
        let value = std::thread::spawn(move || stack.get(Register(0)))
            .join()
            .expect("thread should not panic");
        assert_eq!(value, Value::from_i32(5));
    }

    #[test]
    #[should_panic(expected = "no frame is running")]
    fn reading_a_register_with_nothing_running_fails_loudly() {
        let stack = Stack::new().expect("should reserve");
        let _ = stack.get(Register(0));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "is past a frame")]
    fn a_register_past_the_frame_fails_loudly_in_a_debug_build() {
        // A verified blueprint cannot produce this, so it is an assertion about our own lowering
        // rather than a check on the program being run, and it costs nothing in release.
        let mut stack = Stack::new().expect("should reserve");
        push(&mut stack, 2);
        let _ = stack.get(Register(2));
    }
}
