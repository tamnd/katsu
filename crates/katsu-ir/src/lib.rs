//! Bytecode, function blueprints and the opcode description language.
//!
//! This crate is the bottom of the pipeline that everything else agrees on. The parser lowers into
//! it, the interpreter executes it, the stencil generator derives tier 1 from it, and the ahead of
//! time emitter reads it. See `spec/05-interpreter.md` for the bytecode design and
//! `spec/04-frontend.md` for how a program gets here.
//!
//! Four modules, and the split is the one the consumers ask for. `op` is the instruction set,
//! `constant` is the per function pool that instructions name by index, `position` is which byte of
//! the source each instruction came from, and `blueprint` is the four of those bolted together into
//! the thing a closure is stamped out of. Everything is re-exported at the root, because a consumer
//! wants `katsu_ir::Op` and does not care which file it is written in.

mod blueprint;
mod constant;
mod op;
mod position;

pub use blueprint::{BlueprintError, FunctionBlueprint, Handler};
pub use constant::{ConstIndex, Constant, ConstantPool};
pub use op::{AccessorHalf, BlueprintIndex, CacheIndex, CodeOffset, Op, Register};
pub use position::SourcePositions;

/// The version of the bytecode format itself, independent of the crate version.
///
/// Every cache file and every snapshot carries it. A mismatch means the artifact is discarded and
/// regenerated, silently and correctly, never loaded optimistically. See
/// `spec/16-package-layout.md`.
///
/// Version 2 is the first one with a constant pool, a source position table and nested blueprints in
/// it. Version 1 was the eight opcode sketch that shipped in 0.0.1 and nothing ever wrote it to
/// disk, but bumping it costs nothing and pretending the format did not change costs a confusing
/// afternoon the first time somebody has an old artifact.
pub const BYTECODE_FORMAT_VERSION: u32 = 2;
