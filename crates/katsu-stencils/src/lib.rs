//! Build time stencil generation and the shipped copy and patch artifacts.
//!
//! Stencils are extracted at build time from compiled handler bodies, then patched at run
//! time to produce tier 1 code in microseconds. They are shipped as prebuilt artifacts
//! rather than generated on the user's machine, so that `cargo install katsu` does not
//! require a C toolchain. See `spec/06-jit-tiers.md`.
//!
//! Whether this produces competitive code for polymorphic JavaScript operations is open
//! question Q1, the single largest technical risk in the project, and it is answered by
//! the M2 spike before anything is built on top of it.

/// A relocation that has to be filled in when a stencil is stamped into code memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relocation {
    /// An absolute 64 bit address, written little endian at the given offset.
    Abs64 { offset: u32 },
    /// A 32 bit program counter relative displacement, x86-64 call and jump form.
    Rel32 { offset: u32 },
    /// An aarch64 `bl` or `b` immediate, 26 bits shifted left by two.
    Aarch64Branch26 { offset: u32 },
    /// An aarch64 `adrp` page offset, split across the instruction encoding.
    Aarch64AdrpPage21 { offset: u32 },
}

/// One compiled handler body, with the holes left in it that patching fills.
#[derive(Clone, Debug)]
pub struct Stencil {
    /// The opcode this stencil implements, as a stable string name.
    pub opcode: &'static str,
    /// The machine code, verbatim, with the holes still present.
    pub code: &'static [u8],
    /// Where the holes are.
    pub relocations: &'static [Relocation],
}

/// The stencil table for the host architecture.
///
/// Empty until the M2 generator lands. Returning an empty table rather than panicking is
/// deliberate: an interpreter only build is a supported configuration, per the `jit`
/// feature flag in `spec/16-package-layout.md`, and it is what runs on platforms with no
/// writable executable memory.
#[must_use]
pub fn host_stencils() -> &'static [Stencil] {
    &[]
}

/// Whether tier 1 can run on this build.
#[must_use]
pub fn stencils_available() -> bool {
    !host_stencils().is_empty()
}
