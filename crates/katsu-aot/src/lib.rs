//! The ahead of time emitter: a JavaScript or TypeScript program in, a Rust crate out.
//!
//! Your class does not become a `struct`. The emitted Rust contains the operations the
//! optimizing JIT would have emitted, against the same object model, linked against the
//! same runtime, so the semantics are the real ones and not a typed subset of them. This
//! is the most misunderstandable thing in the design and `spec/09-aot-mode.md` is blunt
//! about it.

/// How much the compiler knows about one operation before the program runs.
///
/// TypeScript annotations are evidence, not proof. TypeScript is deliberately unsound, so
/// an annotation moves an operation from `Dynamic` to `Speculated` and never to `Proven`.
/// See `spec/09-aot-mode.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Certainty {
    /// The type follows from the program itself and needs no guard.
    Proven,
    /// The type is likely, and the emitted code guards and falls back if it is wrong.
    Speculated,
    /// Nothing useful is known. The emitted code does the full dynamic operation.
    Dynamic,
}

/// Why an ahead of time compiled binary still has to embed the interpreter.
///
/// Without a deoptimization target, ahead of time compilation is either conservative and
/// slow or fast and wrong. Embedding the interpreter is what lets it be fast and correct.
#[must_use]
pub const fn embeds_interpreter() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{Certainty, embeds_interpreter};

    #[test]
    fn an_annotation_is_never_proof() {
        // This is a documentation test in the form of an assertion. If someone ever adds a
        // path that promotes an annotation to Proven, spec/09-aot-mode.md says it is wrong.
        assert_ne!(Certainty::Speculated, Certainty::Proven);
        assert!(embeds_interpreter());
    }
}
