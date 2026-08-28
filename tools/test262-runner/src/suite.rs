//! Finding the tests, loading the harness, and building the exact source each case runs.
//!
//! `INTERPRETING.md` in the suite is the normative description of all of this and it is short. The
//! parts that matter here: a test is every `.js` file under `test/` that is not a `_FIXTURE.js`,
//! every test that is not `raw` gets `assert.js` and `sta.js` prepended followed by whatever it
//! lists in `includes`, and a test with neither `onlyStrict` nor `noStrict` is two cases rather than
//! one, because it has to pass in both modes.
//!
//! The concatenation is not a detail. The harness is ordinary JavaScript, so a test's first line of
//! its own is roughly the two hundredth line the engine sees, and until the engine can run
//! `assert.js` every non `raw` test in the suite reports the same first missing construct. That
//! looks like a bug in the runner and it is not, it is the honest shape of the number today.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::metadata::{self, Metadata};

/// The two files every non `raw` test gets, in this order, before anything it asks for itself.
const ALWAYS: [&str; 2] = ["assert.js", "sta.js"];

/// How long any one case gets before the watchdog stops it.
///
/// Generous, because a debug build of an interpreter is slow and a test that takes two seconds is a
/// test we want the real result of. What this is defending against is a program that never returns
/// at all, and those do not take five seconds, they take forever.
pub(crate) const TIMEOUT_SECONDS: u64 = 5;

/// One test file, read and understood, ready to be turned into one or two cases.
#[derive(Clone, Debug)]
pub(crate) struct Test {
    /// Path relative to the suite root, which is what goes in the expectations file.
    ///
    /// Relative rather than absolute so the file means the same thing on every machine. An
    /// expectations file full of `/Users/somebody/...` is one nobody else can use.
    pub(crate) name: String,
    /// The bytes of the file itself, without any harness.
    pub(crate) source: String,
    /// What its own block said about how to run it.
    pub(crate) meta: Metadata,
}

/// One thing to actually run: a complete source string and the mode it belongs to.
#[derive(Clone, Debug)]
pub(crate) struct Case {
    /// The test this came from, with the mode appended when there are two.
    pub(crate) name: String,
    /// Everything the engine is given, harness and all.
    pub(crate) source: String,
}

/// The harness files, read once and shared by every case.
///
/// A `BTreeMap` because there are thirty four of them, they are read once at startup, and the
/// lookups are one per `includes` entry. A hash map would be the reflex and would be measuring
/// nothing.
#[derive(Debug, Default)]
pub(crate) struct Harness {
    files: BTreeMap<String, String>,
}

impl Harness {
    /// Read every `.js` file in the suite's `harness` directory.
    ///
    /// All of them rather than the ones we turn out to need, because there are thirty four and the
    /// alternative is a lazy read behind a lock on the hot path of a fifty thousand case run.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory is missing or unreadable, which means the checkout is not
    /// a test262 checkout and nothing after this would work either.
    pub(crate) fn load(root: &Path) -> Result<Harness> {
        let directory = root.join("harness");
        let mut files = BTreeMap::new();
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("cannot read the harness at {}", directory.display()))?
        {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "js") {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                files.insert(name, std::fs::read_to_string(&path)?);
            }
        }
        Ok(Harness { files })
    }

    /// How many files were loaded, which is the only thing worth asserting about a directory read.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing was loaded, which means the checkout is wrong.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The text of one harness file.
    fn get(&self, name: &str) -> Option<&str> {
        self.files.get(name).map(String::as_str)
    }
}

/// The commit the checkout is on, read out of git's own files rather than by running git.
///
/// The suite gains and renames tests every week, and the expectations file is a set of names out of
/// it, so a run against a different revision produces a diff that looks exactly like a regression
/// until somebody works out that it is not. Recording the revision makes that one glance instead.
///
/// Read directly because shelling out needs git installed, and a release build of this runner has
/// no other reason to need it. Handles both shapes a checkout comes in: a `ref:` line pointing at a
/// branch, and a detached head, which is what pinning a revision in CI produces.
#[must_use]
pub(crate) fn revision(root: &Path) -> String {
    let git = root.join(".git");
    let Ok(head) = std::fs::read_to_string(git.join("HEAD")) else {
        return String::new();
    };
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref: ") else {
        // Detached, so the file holds the commit itself.
        return head.to_owned();
    };
    if let Ok(text) = std::fs::read_to_string(git.join(reference)) {
        return text.trim().to_owned();
    }
    // The ref has been packed, which is what a fresh clone looks like before anything writes to it.
    let Ok(packed) = std::fs::read_to_string(git.join("packed-refs")) else {
        return String::new();
    };
    packed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find(|(_, name)| *name == reference)
        .map(|(commit, _)| commit.to_owned())
        .unwrap_or_default()
}

/// Whether a path under `test/` is a test at all.
///
/// `_FIXTURE.js` files are imported by module tests and are not tests themselves, and running one
/// on its own produces a failure that means nothing. The suite names them by convention and
/// `INTERPRETING.md` says to exclude them by that convention.
#[must_use]
#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "every test in the suite is named .js in lower case, and a file named otherwise is not one"
)]
pub(crate) fn is_test(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".js") && !name.ends_with("_FIXTURE.js")
}

/// Why a test is not being run, or `None` if it is.
///
/// Every one of these is a thing the engine genuinely cannot do rather than a thing we would rather
/// not measure, and each one stays counted in the total with its reason printed. A skip that
/// disappears from the denominator is how a conformance number becomes decoration.
#[must_use]
pub(crate) fn skip_reason(meta: &Metadata, name: &str, source: &str) -> Option<&'static str> {
    if name.starts_with("intl402/") {
        return Some("needs Intl, which is milestone M8");
    }
    if name.starts_with("staging/") {
        // The suite's own README says staging is a holding area for tests that have not been
        // reviewed yet, so a failure there is as likely to be the test's fault as ours.
        return Some("staging is not part of the conformance suite");
    }
    if meta.flags.module {
        return Some("needs a module system, which is milestone M3");
    }
    if meta.flags.is_async {
        // These report completion by calling `$DONE` from a job, so without an event loop the
        // runner cannot tell a pass from a test that simply returned.
        return Some("needs $DONE and an event loop, which is milestone M2");
    }
    if meta.flags.needs_host_api || source.contains("$262") {
        return Some("needs the $262 host API");
    }
    None
}

/// Read one test file and understand it, or say why it is not a test.
///
/// # Errors
///
/// Returns an error only if the file cannot be read. A file that is not valid UTF-8 is one of
/// those, and there are a handful in the suite that are deliberately encoded oddly.
pub(crate) fn read(root: &Path, path: &Path) -> Result<Option<Test>> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let Some(meta) = metadata::parse(&source) else {
        // No block means no instructions, and running it anyway means guessing at whether it is
        // supposed to succeed. There are none of these under `test/` today and the check is here so
        // that if one appears it is skipped loudly rather than scored at random.
        return Ok(None);
    };
    Ok(Some(Test {
        name: relative(root, path),
        source,
        meta,
    }))
}

/// The path an expectations file should call this test.
///
/// Always forward slashes, including on Windows, because the same expectations file is read on all
/// three platforms and a backslash in it would make every path miss.
fn relative(root: &Path, path: &Path) -> String {
    let suffix = path.strip_prefix(root.join("test")).unwrap_or(path);
    suffix.to_string_lossy().replace('\\', "/")
}

impl Test {
    /// Build the one or two cases this test turns into.
    ///
    /// # Errors
    ///
    /// Returns an error if the test lists an `includes` file the harness does not have, which means
    /// the checkout is inconsistent rather than that the test failed.
    pub(crate) fn cases(&self, harness: &Harness) -> Result<Vec<Case>> {
        if self.meta.flags.raw {
            // `raw` means exactly the bytes in the file. Prepending anything at all, even a
            // comment, changes what is being tested, because these are the tests about what a
            // program looks like at its very first character.
            return Ok(vec![Case {
                name: self.name.clone(),
                source: self.source.clone(),
            }]);
        }

        let mut prelude = String::new();
        for name in ALWAYS.iter().copied().chain(
            self.meta
                .includes
                .iter()
                .map(String::as_str)
                .filter(|name| !ALWAYS.contains(name)),
        ) {
            let text = harness.get(name).with_context(|| {
                format!(
                    "{} includes {name}, which the harness does not have",
                    self.name
                )
            })?;
            prelude.push_str(text);
            prelude.push('\n');
        }

        let mut cases = Vec::with_capacity(2);
        if self.meta.flags.wants_sloppy() {
            cases.push(Case {
                name: self.mode_name("sloppy"),
                source: format!("{prelude}{}", self.source),
            });
        }
        if self.meta.flags.wants_strict() {
            // The directive goes at the very top, in front of the harness, because a directive
            // prologue is only a directive prologue at the start of the program. Putting it after
            // the harness would make it an ordinary string expression and the case would silently
            // run in sloppy mode, which is a failure that looks exactly like a pass.
            cases.push(Case {
                name: self.mode_name("strict"),
                source: format!("\"use strict\";\n{prelude}{}", self.source),
            });
        }
        Ok(cases)
    }

    /// The name for one mode, with the mode left off when there is only ever one.
    ///
    /// A test that runs in both modes has two entries in the expectations file, because it can pass
    /// in one and fail in the other and a single entry would have to pick a lie.
    fn mode_name(&self, mode: &str) -> String {
        if self.meta.flags.wants_sloppy() && self.meta.flags.wants_strict() {
            format!("{} ({mode})", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// Every test file under the suite's `test/` directory, in a stable order.
///
/// Sorted, because the run is parallel and the report is not, and a report whose rows move between
/// runs cannot be diffed.
///
/// # Errors
///
/// Returns an error if the `test` directory cannot be walked.
pub(crate) fn find(root: &Path) -> Result<Vec<PathBuf>> {
    let directory = root.join("test");
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&directory)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| is_test(path))
        .collect();
    if paths.is_empty() {
        anyhow::bail!(
            "no tests under {}. Is this a test262 checkout?",
            directory.display()
        );
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Harness, Test, is_test, skip_reason};
    use crate::metadata;

    fn harness() -> Harness {
        let mut harness = Harness::default();
        harness
            .files
            .insert("assert.js".to_owned(), "// assert".to_owned());
        harness
            .files
            .insert("sta.js".to_owned(), "// sta".to_owned());
        harness
            .files
            .insert("compareArray.js".to_owned(), "// compareArray".to_owned());
        harness
    }

    fn test(source: &str) -> Test {
        Test {
            name: "language/thing.js".to_owned(),
            source: source.to_owned(),
            meta: metadata::parse(source).expect("should have a block"),
        }
    }

    #[test]
    fn the_revision_is_read_from_a_branch_a_detached_head_and_a_packed_ref() {
        let temporary = tempfile::tempdir().expect("temp dir");
        let root = temporary.path();
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("refs").join("heads")).expect("mkdir");

        // Nothing there at all, which is a tarball rather than a clone. Not an error, because the
        // revision is a convenience and refusing to run without it would be worse than not knowing.
        assert_eq!(super::revision(root), "");

        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").expect("write");
        std::fs::write(git.join("refs").join("heads").join("main"), "abc123\n").expect("write");
        assert_eq!(super::revision(root), "abc123");

        // What a fresh clone looks like before anything writes a loose ref.
        std::fs::remove_file(git.join("refs").join("heads").join("main")).expect("remove");
        std::fs::write(
            git.join("packed-refs"),
            "# pack-refs with: peeled\ndef456 refs/heads/main\n",
        )
        .expect("write");
        assert_eq!(super::revision(root), "def456");

        // What pinning a revision in CI produces.
        std::fs::write(git.join("HEAD"), "0123456789abcdef\n").expect("write");
        assert_eq!(super::revision(root), "0123456789abcdef");
    }

    #[test]
    fn a_fixture_is_not_a_test() {
        assert!(is_test(Path::new("test/language/module-code/x.js")));
        assert!(!is_test(Path::new(
            "test/language/module-code/instn-star_FIXTURE.js"
        )));
        assert!(!is_test(Path::new("test/README.md")));
    }

    #[test]
    fn an_ordinary_test_becomes_two_cases_one_per_mode() {
        // Both modes have to pass and they are different programs, so they are two entries rather
        // than one. A single entry would have to report a pass in one mode and a failure in the
        // other as one thing.
        let cases = test("/*---\ndescription: x\n---*/\nvar x = 1;\n")
            .cases(&harness())
            .expect("should build");
        assert_eq!(cases.len(), 2);
        assert!(cases[0].name.ends_with("(sloppy)"));
        assert!(cases[1].name.ends_with("(strict)"));
    }

    #[test]
    fn the_use_strict_directive_goes_before_the_harness_and_not_after() {
        // The failure this guards against is invisible: after the harness it is an ordinary string
        // expression, the case runs sloppy, and a strict mode test passes for the wrong reason.
        let cases = test("/*---\nflags: [onlyStrict]\n---*/\nvar x = 1;\n")
            .cases(&harness())
            .expect("should build");
        assert_eq!(cases.len(), 1);
        assert!(cases[0].source.starts_with("\"use strict\";\n"));
        assert!(cases[0].source.contains("// assert"));
    }

    #[test]
    fn a_single_mode_test_keeps_its_plain_name() {
        let cases = test("/*---\nflags: [noStrict]\n---*/\nvar x = 1;\n")
            .cases(&harness())
            .expect("should build");
        assert_eq!(cases[0].name, "language/thing.js");
    }

    #[test]
    fn raw_gets_nothing_prepended_at_all() {
        let cases = test("/*---\nflags: [raw]\n---*/\nvar x = 1;\n")
            .cases(&harness())
            .expect("should build");
        assert_eq!(cases.len(), 1);
        assert!(!cases[0].source.contains("// assert"));
        assert!(cases[0].source.starts_with("/*---"));
    }

    #[test]
    fn includes_come_after_the_two_every_test_gets_and_are_not_repeated() {
        let cases = test("/*---\nincludes: [compareArray.js, assert.js]\n---*/\n")
            .cases(&harness())
            .expect("should build");
        let source = &cases[0].source;
        let assert = source.find("// assert").expect("assert.js");
        let sta = source.find("// sta").expect("sta.js");
        let compare = source.find("// compareArray").expect("compareArray.js");
        assert!(assert < sta && sta < compare);
        assert_eq!(source.matches("// assert").count(), 1);
    }

    #[test]
    fn a_missing_include_is_a_broken_checkout_and_not_a_failed_test() {
        let error = test("/*---\nincludes: [nosuch.js]\n---*/\n")
            .cases(&harness())
            .expect_err("should not build");
        assert!(error.to_string().contains("nosuch.js"), "{error}");
    }

    #[test]
    fn the_skips_are_the_things_the_engine_cannot_do_and_each_says_which() {
        let meta = metadata::parse("/*---\nflags: [module]\n---*/\n").expect("block");
        assert!(skip_reason(&meta, "language/x.js", "").is_some());

        let meta = metadata::parse("/*---\ndescription: x\n---*/\n").expect("block");
        assert!(skip_reason(&meta, "intl402/x.js", "").is_some());
        assert!(skip_reason(&meta, "staging/x.js", "").is_some());
        assert!(skip_reason(&meta, "language/x.js", "$262.createRealm()").is_some());
        // The ordinary case, which is most of the suite, is not skipped for anything.
        assert!(skip_reason(&meta, "language/x.js", "var x = 1;").is_none());
    }
}
