//! The YAML block at the top of every test262 file, and what the runner needs out of it.
//!
//! Every test in the suite carries a comment block between `/*---` and `---*/` saying how it wants
//! to be run. Ignoring it does not give you a slightly wrong pass rate, it gives you a meaningless
//! one: a file expected to be rejected counts as a failure when it is rejected, a file needing
//! `assert.js` fails on the first line, and a file marked `onlyStrict` runs in the wrong mode.
//! `INTERPRETING.md` in the suite is the normative description and this follows it.
//!
//! # Why this is hand written rather than a YAML dependency
//!
//! The block is YAML, but the part of YAML it uses is four keys, two of which hold a flow sequence
//! of bare words and one of which is a two key nested map. A full YAML parser is a large dependency
//! carrying a large specification, in a workspace where document 16 treats the dependency count as
//! a budget, in exchange for handling constructs that never appear here. What it does have to
//! handle is the `info: |` literal block, which is prose that can contain absolutely anything
//! including lines that look like keys, and that is handled by indentation the same way YAML would.
//!
//! The failure mode of getting this wrong is a wrong pass rate rather than a crash, which is the
//! reason it has its own tests against blocks copied out of the real suite.

/// What one test file says about how it wants to be run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Metadata {
    /// The one line summary, used when a failure is reported.
    pub(crate) description: String,
    /// Harness files to prepend, beyond the two every test gets.
    pub(crate) includes: Vec<String>,
    /// Everything in `flags`, kept as parsed booleans rather than as strings.
    pub(crate) flags: Flags,
    /// Set when the file is supposed to be rejected rather than run.
    pub(crate) negative: Option<Negative>,
    /// Language features the test needs, which is how a test for something we have not built is
    /// told apart from a test we simply fail.
    pub(crate) features: Vec<String>,
}

/// The `flags` list, which is a fixed vocabulary rather than free text.
///
/// Seven booleans rather than an enum or a bitfield, because that is what the suite defines: a set
/// of independent words, several of which appear together on the same test. Collapsing them into
/// something tidier would mean inventing combinations the suite does not have.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the suite's own vocabulary is a set of flags"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    /// Run only with `"use strict"` in front.
    pub(crate) only_strict: bool,
    /// Run only without it.
    pub(crate) no_strict: bool,
    /// Run exactly the bytes in the file, with no harness and no strict wrapper.
    pub(crate) raw: bool,
    /// Parse as a module rather than as a script.
    pub(crate) module: bool,
    /// Completion is reported by calling `$DONE`, so the file is not finished when it returns.
    pub(crate) is_async: bool,
    /// Uses `$262.createRealm`, `$262.detachArrayBuffer` or `$262.agent`.
    pub(crate) needs_host_api: bool,
    /// Generates its own source at runtime, so it can be enormous.
    pub(crate) generated: bool,
}

/// A test that is supposed to fail, and the way it is supposed to fail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Negative {
    /// When the failure has to happen: `parse`, `resolution` or `runtime`.
    pub(crate) phase: Phase,
    /// The constructor name of the error, for example `SyntaxError`.
    pub(crate) kind: String,
}

/// Which stage a negative test expects to fail in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Before anything runs. A syntax error or an early error.
    Parse,
    /// While linking a module graph.
    Resolution,
    /// While executing.
    Runtime,
}

impl Flags {
    /// Whether the file should be run once in sloppy mode.
    ///
    /// A test with neither `onlyStrict` nor `noStrict` runs in both modes and has to pass in both,
    /// which is why this and [`Flags::wants_strict`] are two questions rather than one enum. `raw`
    /// means the bytes run exactly as written, so it is sloppy by definition.
    pub(crate) const fn wants_sloppy(self) -> bool {
        self.raw || !self.only_strict
    }

    /// Whether the file should be run once with `"use strict"` prepended.
    pub(crate) const fn wants_strict(self) -> bool {
        !self.raw && !self.no_strict && !self.module
    }
}

/// Pull the block out of a source file, if it has one.
///
/// A file with no block is not a test. The suite's own fixtures are the usual case and they are
/// excluded by name as well, but a file with no metadata is skipped rather than guessed at either
/// way, because guessing produces a number that is wrong in an invisible direction.
pub(crate) fn parse(source: &str) -> Option<Metadata> {
    let start = source.find("/*---")? + "/*---".len();
    let end = source[start..].find("---*/")? + start;
    Some(from_block(&source[start..end]))
}

/// Read the keys we care about out of the block.
///
/// Line oriented, because the only nesting that matters is one level deep. A line that starts in
/// column zero opens a key and anything indented under it belongs to that key, which is exactly
/// what keeps the `info: |` prose from being read as keys of its own.
fn from_block(block: &str) -> Metadata {
    let mut meta = Metadata::default();
    let mut key = String::new();
    let mut nested: Vec<(String, String)> = Vec::new();

    for line in block.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if indented {
            // Belongs to whatever key is open. Only two of them have anything worth reading out of
            // their continuation lines, and `info` is prose that is deliberately thrown away.
            match key.as_str() {
                "negative" => {
                    if let Some((name, value)) = split_key(trimmed) {
                        nested.push((name.to_owned(), value.trim().to_owned()));
                    }
                }
                "includes" | "features" | "flags" => {
                    if let Some(item) = trimmed.strip_prefix("- ") {
                        push_list(&mut meta, &key, item.trim());
                    }
                }
                _ => {}
            }
            continue;
        }

        let Some((name, value)) = split_key(trimmed) else {
            continue;
        };
        name.clone_into(&mut key);
        let value = value.trim();

        match name {
            // A description can be a literal block too, in which case the text arrives on the
            // following indented lines and this is empty. That is fine, it is only used in a
            // failure message.
            "description" => value
                .trim_matches('>')
                .trim()
                .clone_into(&mut meta.description),
            "includes" | "features" | "flags" => {
                for item in flow_sequence(value) {
                    push_list(&mut meta, name, item);
                }
            }
            _ => {}
        }
    }

    meta.negative = negative_from(&nested);
    meta
}

/// Split `key: value`, rejecting anything that is not a key so prose does not become one.
///
/// The check is that everything before the colon looks like an identifier, which is true of every
/// key in the suite and false of the English sentences in an `info` block that happen to contain a
/// colon. Prose is already excluded by indentation, so this is the second of two guards.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let (name, rest) = line.split_once(':')?;
    let name = name.trim();
    let is_key = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    is_key.then_some((name, rest))
}

/// The items of a `[a, b, c]` flow sequence, or nothing if the value is not one.
fn flow_sequence(value: &str) -> impl Iterator<Item = &str> {
    value
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

/// Put one list item where it belongs.
fn push_list(meta: &mut Metadata, key: &str, item: &str) {
    match key {
        "includes" => meta.includes.push(item.to_owned()),
        "features" => meta.features.push(item.to_owned()),
        "flags" => set_flag(&mut meta.flags, item),
        _ => {}
    }
}

/// Turn one word of the `flags` vocabulary into a field.
///
/// An unknown flag is ignored rather than treated as an error. The suite adds flags over time and
/// a runner that refuses to run when it meets a new one is a runner that breaks on every update,
/// which is a worse failure than running a test in a mode that is slightly too permissive.
fn set_flag(flags: &mut Flags, word: &str) {
    match word {
        "onlyStrict" => flags.only_strict = true,
        "noStrict" => flags.no_strict = true,
        "raw" => flags.raw = true,
        "module" => flags.module = true,
        "async" => flags.is_async = true,
        "CanBlockIsFalse" | "CanBlockIsTrue" => flags.needs_host_api = true,
        "generated" => flags.generated = true,
        _ => {}
    }
}

/// Assemble the `negative` map, which is only a negative test if it has both halves.
fn negative_from(nested: &[(String, String)]) -> Option<Negative> {
    let find = |wanted: &str| {
        nested
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, value)| value.as_str())
    };
    let phase = match find("phase")? {
        "parse" => Phase::Parse,
        "resolution" => Phase::Resolution,
        "runtime" => Phase::Runtime,
        // A phase we do not know is not a phase we can check, and inventing one would decide the
        // outcome of every test carrying it.
        _ => return None,
    };
    Some(Negative {
        phase,
        kind: find("type")?.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Phase, parse};

    #[test]
    fn a_file_with_no_block_is_not_a_test() {
        assert!(parse("var x = 1;\n").is_none());
    }

    #[test]
    fn the_ordinary_shape_is_a_description_and_nothing_else() {
        let meta = parse("// Copyright\n/*---\nesid: sec-addition-operator\ndescription: adds\n---*/\nvar x = 1 + 1;\n")
            .expect("should have a block");
        assert_eq!(meta.description, "adds");
        assert!(meta.includes.is_empty());
        assert!(meta.negative.is_none());
        // Neither flag set means both modes, which is the default the whole suite relies on.
        assert!(meta.flags.wants_sloppy());
        assert!(meta.flags.wants_strict());
    }

    #[test]
    fn a_flow_sequence_is_read_and_so_is_a_block_sequence() {
        let inline = parse("/*---\nincludes: [assert.js, compareArray.js]\n---*/\n")
            .expect("should have a block");
        assert_eq!(inline.includes, ["assert.js", "compareArray.js"]);
        let block = parse("/*---\nincludes:\n  - assert.js\n  - compareArray.js\n---*/\n")
            .expect("should have a block");
        assert_eq!(block.includes, inline.includes);
    }

    #[test]
    fn the_flag_vocabulary_becomes_fields() {
        let meta =
            parse("/*---\nflags: [onlyStrict, async]\n---*/\n").expect("should have a block");
        assert!(meta.flags.only_strict);
        assert!(meta.flags.is_async);
        assert!(!meta.flags.wants_sloppy());
        assert!(meta.flags.wants_strict());
    }

    #[test]
    fn raw_means_the_bytes_as_written_and_therefore_sloppy_only() {
        let meta = parse("/*---\nflags: [raw]\n---*/\n").expect("should have a block");
        assert!(meta.flags.raw);
        assert!(meta.flags.wants_sloppy());
        assert!(!meta.flags.wants_strict());
    }

    #[test]
    fn a_flag_we_have_never_heard_of_is_ignored_rather_than_fatal() {
        // The suite adds flags over time. A runner that refuses to run when it meets a new one
        // breaks on every update, which is worse than running one test too permissively.
        let meta = parse("/*---\nflags: [somethingNewIn2027, onlyStrict]\n---*/\n")
            .expect("should have a block");
        assert!(meta.flags.only_strict);
    }

    #[test]
    fn a_negative_test_carries_both_the_phase_and_the_error() {
        let meta = parse("/*---\nnegative:\n  phase: parse\n  type: SyntaxError\n---*/\n")
            .expect("should have a block");
        let negative = meta.negative.expect("should be negative");
        assert_eq!(negative.phase, Phase::Parse);
        assert_eq!(negative.kind, "SyntaxError");
    }

    #[test]
    fn half_a_negative_map_is_not_a_negative_test() {
        // Rather than defaulting the missing half, because a default here silently decides the
        // outcome of the test rather than reporting that the file is malformed.
        let meta = parse("/*---\nnegative:\n  phase: parse\n---*/\n").expect("should have a block");
        assert!(meta.negative.is_none());
    }

    #[test]
    fn prose_in_an_info_block_is_not_read_as_keys() {
        // The one thing a hand written parser has to get right. `info` is free text copied out of
        // the specification, it is full of colons, and reading it as keys would set flags at random.
        let source = "/*---\ndescription: something\ninfo: |\n  Step 3: if the thing is a thing\n  flags: [onlyStrict]\n  negative:\n    phase: parse\n    type: SyntaxError\nfeatures: [Symbol]\n---*/\n";
        let meta = parse(source).expect("should have a block");
        assert_eq!(meta.description, "something");
        assert!(!meta.flags.only_strict, "prose set a flag");
        assert!(meta.negative.is_none(), "prose made this a negative test");
        assert_eq!(meta.features, ["Symbol"]);
    }

    #[test]
    fn a_real_block_copied_out_of_the_suite() {
        let source = "// Copyright (C) 2015 the V8 project authors. All rights reserved.\n// This code is governed by the BSD license found in the LICENSE file.\n/*---\nesid: sec-array.prototype.copywithin\ndescription: >\n  Array.prototype.copyWithin.length value and descriptor.\ninfo: |\n  17 ECMAScript Standard Built-in Objects:\n    Every built-in function object, including constructors, has a length property\nincludes: [propertyHelper.js]\nfeatures: [Array.prototype.copyWithin]\n---*/\n\nverifyProperty(Array.prototype.copyWithin, 'length', {\n  value: 2\n});\n";
        let meta = parse(source).expect("should have a block");
        assert_eq!(meta.includes, ["propertyHelper.js"]);
        assert_eq!(meta.features, ["Array.prototype.copyWithin"]);
        assert!(meta.negative.is_none());
        // The description is a folded block, so it arrives on the next line and this is empty.
        // Only used in failure messages, so empty is acceptable and asserting it is not silence.
        assert_eq!(meta.description, "");
    }
}
