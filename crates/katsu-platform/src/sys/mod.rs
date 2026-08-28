//! The four virtual memory operations, once per operating system.
//!
//! Everything above this module works in terms of reserving address space and committing pages
//! inside it, which is the same idea everywhere. The calls that implement it are not the same idea
//! anywhere, so they live here and nowhere else, and the split is by file rather than by `cfg`
//! sprinkled through one function, because a function with three branches in it is a function that
//! only ever gets read on one platform.
//!
//! The seam is deliberately four functions and no more.
//!
//! `page_size` is what permission granularity is measured in. `reserve` takes address space with
//! nothing behind it and a base aligned the way the caller asked. `release` gives the whole thing
//! back. `commit` puts readable and writable pages behind part of it, and `decommit` takes them
//! away again without giving up the address space.
//!
//! Two invariants hold on every platform and the layers above depend on both. A freshly committed
//! page reads as zero, which is what lets a zero slot mean the integer zero with no initialisation
//! pass. And a decommit followed by a commit gives back zeroes rather than the old contents, so a
//! collector that recycles a block cannot see a dead object's bytes. Neither is free on either
//! platform and the notes in each backend say what it costs to get them.

#[cfg(unix)]
mod posix;
#[cfg(unix)]
pub(crate) use posix::{commit, decommit, page_size, release, reserve};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{commit, decommit, page_size, release, reserve};

#[cfg(not(any(unix, windows)))]
compile_error!(
    "katsu-platform has a virtual memory backend for unix and for windows, and this is neither. \
     Adding one means implementing reserve, release, commit and decommit in src/sys, which is \
     four functions, rather than weakening anything above them."
);
