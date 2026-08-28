//! `katsu check`, which type checks a program by asking the TypeScript compiler to do it.
//!
//! We are not a type checker and we are not going to become one. `katsu run` strips types the way
//! every other runtime does, so a program that never runs `tsc` never learns that its types are
//! wrong, and this is the command that closes that gap without us writing a checker.
//!
//! # Finding `tsc`
//!
//! Four places, in this order, and the order is the point.
//!
//! `KATSU_TSC` wins outright, because somebody who has set it has a reason and no amount of
//! searching is going to beat knowing.
//!
//! Then `node_modules/.bin/tsc`, walking up from the file being checked. This is above a `tsc` on
//! the path on purpose. A project pins its TypeScript version in its `package.json` and its types
//! only check against that version, so a globally installed `tsc` of a different version is not a
//! substitute for it, it is a different answer to the same question. Every other tool in this
//! ecosystem resolves this way and a runtime that did not would be the surprising one.
//!
//! Then `tsc` on the path, for somebody who installed it globally and has no project around them.
//!
//! Then nothing, and the error names all three places rather than saying it could not find it.
//!
//! # Which files get checked
//!
//! If there is a `tsconfig.json` at or above the entry, that project is checked, because that file
//! is where a project says what its types mean and checking one file with the default settings
//! would report errors the project does not have and miss errors it does. If there is no
//! `tsconfig.json`, the named file is checked on its own.
//!
//! Either way `--noEmit` goes on, because this command answers a question and does not produce
//! anything.
//!
//! The compiler runs in the directory you are standing in rather than in the project root, even
//! when a project is being checked. `tsc` prints the file in an error relative to where it was
//! started, so this is what makes the path in the error resolve from where you are, which is what a
//! terminal and an editor both need in order to open it. Running from the project root instead
//! would print paths that are shorter and that neither of them can follow.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Where the TypeScript compiler was found, kept apart from running it so the search can be tested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tsc {
    /// The program to run.
    pub(crate) program: PathBuf,
    /// Why it was chosen, for the message printed when `KATSU_LOG` asks for it.
    pub(crate) found: Found,
}

/// Which of the three places a compiler came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// `KATSU_TSC` named it.
    Environment,
    /// A `node_modules/.bin` at or above the entry.
    Project,
    /// The path.
    Path,
}

/// The file name `tsc` installs itself under, which is not the same on Windows.
///
/// npm writes a `.cmd` shim next to the extensionless shell script, and `Command` on Windows will
/// not run the shell script. Naming the shim is the whole fix and it is one line rather than a
/// dependency.
const BIN: &str = if cfg!(windows) { "tsc.cmd" } else { "tsc" };

/// Look for a TypeScript compiler for `entry`.
///
/// `env` is the value of `KATSU_TSC` and `path` decides whether a bare `tsc` exists, both passed in
/// rather than read here so that the search is a function of its arguments and can be tested on a
/// machine that has TypeScript installed and on one that does not.
pub(crate) fn find(entry: &Path, env: Option<OsString>, on_path: bool) -> Option<Tsc> {
    if let Some(named) = env.filter(|value| !value.is_empty()) {
        return Some(Tsc {
            program: PathBuf::from(named),
            found: Found::Environment,
        });
    }
    if let Some(local) = ancestors(entry)
        .map(|directory| directory.join("node_modules").join(".bin").join(BIN))
        .find(|candidate| candidate.is_file())
    {
        return Some(Tsc {
            program: local,
            found: Found::Project,
        });
    }
    on_path.then(|| Tsc {
        program: PathBuf::from("tsc"),
        found: Found::Path,
    })
}

/// The nearest `tsconfig.json` at or above `entry`, which is the project the entry belongs to.
pub(crate) fn project_of(entry: &Path) -> Option<PathBuf> {
    ancestors(entry)
        .map(|directory| directory.join("tsconfig.json"))
        .find(|candidate| candidate.is_file())
}

/// The arguments to run the compiler with.
///
/// Split out because it is the part worth asserting on: checking a project and checking a loose
/// file are different commands and getting them the wrong way round is the mistake this makes
/// impossible to make quietly.
pub(crate) fn arguments(entry: &Path, project: Option<&Path>) -> Vec<OsString> {
    let mut args = vec![OsString::from("--noEmit")];
    match project {
        Some(config) => {
            args.push(OsString::from("--project"));
            args.push(config.into());
        }
        None => args.push(entry.into()),
    }
    args
}

/// What to say when there is no compiler anywhere.
///
/// Names all three places rather than saying it could not find one, because "not found" leaves
/// somebody guessing at which of the three they were supposed to have set up.
pub(crate) fn nowhere(entry: &Path) -> String {
    format!(
        "no TypeScript compiler to check {} with. Looked at KATSU_TSC, then for {BIN} in a \
         node_modules/.bin at or above that file, then for tsc on the path. Install it in the \
         project with `npm install --save-dev typescript`, or point KATSU_TSC at one.",
        entry.display()
    )
}

/// The directory holding `entry` and every directory above it.
///
/// A relative path with no parent still has to yield the current directory, or a `katsu check
/// app.ts` run from inside the project finds nothing at all.
fn ancestors(entry: &Path) -> impl Iterator<Item = PathBuf> + use<> {
    let start = entry
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    start
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<Vec<PathBuf>>()
        .into_iter()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{BIN, Found, arguments, find, project_of};

    /// A directory tree with a `node_modules/.bin/tsc` and a `tsconfig.json` two levels up from a
    /// source file, which is the shape of every TypeScript project there is.
    fn project() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("should make a temporary directory");
        let bin = root.path().join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin).expect("should create");
        std::fs::write(bin.join(BIN), "").expect("should write");
        std::fs::write(root.path().join("tsconfig.json"), "{}").expect("should write");
        std::fs::create_dir_all(root.path().join("src").join("deep")).expect("should create");
        root
    }

    #[test]
    fn the_environment_beats_everything_else() {
        // Somebody who set it has a reason, and no amount of searching beats knowing.
        let root = project();
        let entry = root.path().join("src").join("app.ts");
        let found = find(&entry, Some(OsString::from("/opt/tsc")), true).expect("should find one");
        assert_eq!(found.program, PathBuf::from("/opt/tsc"));
        assert_eq!(found.found, Found::Environment);
    }

    #[test]
    fn an_empty_environment_variable_is_not_an_answer() {
        // Which is what an unset variable looks like in a shell script that forwards it, and
        // treating it as a program named the empty string would be a confusing way to fail.
        let root = project();
        let entry = root.path().join("src").join("app.ts");
        let found = find(&entry, Some(OsString::new()), true).expect("should find one");
        assert_eq!(found.found, Found::Project);
    }

    #[test]
    fn the_project_compiler_beats_one_on_the_path() {
        // The important one. A project pins its TypeScript version and its types only check
        // against that version, so a global tsc is a different answer rather than a substitute.
        let root = project();
        let entry = root.path().join("src").join("app.ts");
        let found = find(&entry, None, true).expect("should find one");
        assert_eq!(
            found.program,
            root.path().join("node_modules").join(".bin").join(BIN)
        );
        assert_eq!(found.found, Found::Project);
    }

    #[test]
    fn the_search_walks_up_and_not_only_alongside() {
        let root = project();
        let entry = root.path().join("src").join("deep").join("app.ts");
        let found = find(&entry, None, false).expect("should find one");
        assert_eq!(found.found, Found::Project);
    }

    #[test]
    fn the_path_is_the_last_resort_and_only_if_there_is_one() {
        let root = tempfile::tempdir().expect("should make a temporary directory");
        let entry = root.path().join("app.ts");
        assert_eq!(
            find(&entry, None, true).expect("should find one").found,
            Found::Path
        );
        assert_eq!(find(&entry, None, false), None);
    }

    #[test]
    fn the_project_is_the_nearest_tsconfig_above_the_entry() {
        let root = project();
        let entry = root.path().join("src").join("deep").join("app.ts");
        assert_eq!(project_of(&entry), Some(root.path().join("tsconfig.json")));
    }

    #[test]
    fn a_file_with_no_project_around_it_has_no_project() {
        let root = tempfile::tempdir().expect("should make a temporary directory");
        assert_eq!(project_of(&root.path().join("loose.ts")), None);
    }

    #[test]
    fn a_project_is_checked_as_a_project_and_a_loose_file_as_a_file() {
        // Two different commands, and getting them the wrong way round is the mistake worth making
        // impossible to make quietly: checking one file of a project reports errors the project
        // does not have and misses errors it does.
        let entry = Path::new("src/app.ts");
        let config = Path::new("tsconfig.json");
        assert_eq!(
            arguments(entry, Some(config)),
            vec![
                OsString::from("--noEmit"),
                OsString::from("--project"),
                OsString::from("tsconfig.json")
            ]
        );
        assert_eq!(
            arguments(entry, None),
            vec![OsString::from("--noEmit"), OsString::from("src/app.ts")]
        );
    }
}
