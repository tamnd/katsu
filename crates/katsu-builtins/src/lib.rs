//! The ECMAScript builtins.
//!
//! `Object`, `Array`, `String`, `Math`, `JSON`, `Map`, `Set`, `Promise`, `RegExp` and the
//! rest. This is where most of the test262 pass rate in `spec/14-quality-bar.md` comes
//! from, and it is more surface area than cleverness. Regular expressions go through
//! `regress` rather than a hand written engine.

/// Whether ECMA-402, the internationalization API, is compiled into this build.
///
/// Reported honestly rather than stubbed. `spec/17-open-questions.md` Q10 is explicit that
/// we never ship an `Intl` that returns plausible wrong answers, because a locale aware
/// API that silently ignores the locale is worse than one that is absent.
#[must_use]
pub const fn intl_available() -> bool {
    cfg!(feature = "full-intl")
}

/// The ECMAScript language version this build targets.
#[must_use]
pub const fn language_edition() -> &'static str {
    "ECMAScript 2026"
}
