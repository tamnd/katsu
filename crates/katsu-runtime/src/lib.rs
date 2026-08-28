//! The katsu runtime facade.
//!
//! This is what ahead of time compiled output links against and what an embedder adds to
//! their `Cargo.toml`. Its surface is pinned by every binary ever built with `katsu build`,
//! which is why it is a thin re-export layer rather than a place where logic lives.

pub use katsu_api::{Discard, Error, Interrupt, Isolate, Runtime, Value, jit_enabled};
pub use katsu_macros::export;

#[cfg(feature = "node")]
pub use katsu_node as node;

/// The version of the runtime, as reported by `katsu --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which optional pieces this build was compiled with.
///
/// Printed by `katsu --version --verbose`. A build that cannot say what is in it is a build
/// nobody can file a useful bug against.
#[must_use]
pub fn build_features() -> Vec<&'static str> {
    let mut features = Vec::new();
    if cfg!(feature = "jit") {
        features.push("jit");
    }
    if cfg!(feature = "node") {
        features.push("node");
    }
    if cfg!(feature = "addons") {
        features.push("addons");
    }
    if cfg!(feature = "full-intl") {
        features.push("full-intl");
    }
    if cfg!(feature = "uring") {
        features.push("uring");
    }
    if features.is_empty() {
        features.push("minimal");
    }
    features
}

#[cfg(test)]
mod tests {
    use super::build_features;

    #[test]
    fn a_build_can_always_describe_itself() {
        assert!(!build_features().is_empty());
    }
}
