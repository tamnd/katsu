//! The Node.js compatible layer.
//!
//! Take an unmodified npm package with a normal dependency tree, run it, and it works.
//! Same CommonJS and ESM semantics, same `node_modules` resolution, same `node:*` modules,
//! same `process`, same streams, same `Buffer`. This is the layer that kills projects like
//! this, and it kills them slowly: there is no single hard problem, there are eleven
//! hundred small ones. See `spec/10-node-compat.md`.

/// The Node.js line we track for compatibility.
///
/// The active LTS, not the current release, because that is what production runs.
pub const TARGET_NODE_LTS: &str = "24.x";

/// The Node-API version this build implements.
///
/// Node-API stopped being strictly additive at version 9, so 8 is the version with the
/// widest addon compatibility and it is what we default to. See `spec/10-node-compat.md`.
pub const NODE_API_VERSION: u32 = 8;

/// How completely one `node:` module is implemented.
///
/// Published per module in the compatibility table, with every failure listed. A module
/// reported as `Partial` names what is missing rather than hiding behind a percentage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Implemented, and passing the relevant part of Node's own test suite.
    Full,
    /// Usable, with named gaps.
    Partial,
    /// Present so that `require` does not throw, but every function throws on use. Never
    /// a silent no op, because a silent no op is worse than an absence.
    Stub,
    /// Not implemented.
    Missing,
}

/// Whether this build can load native `.node` addons.
///
/// Off by default. Loading arbitrary native code into the process is a security decision
/// the embedder makes, not one we make for them. See `spec/14-quality-bar.md`.
#[must_use]
pub const fn addons_enabled() -> bool {
    cfg!(feature = "addons")
}
