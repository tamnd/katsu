//! Repository automation.
//!
//! Every architectural rule in the specification that is not mechanically enforced will be
//! violated within a year, so the ones that matter get a test. This is where those tests
//! live. See `spec/16-package-layout.md`.

use std::collections::BTreeMap;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

/// Repository automation for katsu.
#[derive(Debug, Parser)]
#[command(name = "xtask", about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Check that no crate depends on a crate in a layer above it.
    Layers,
}

/// The layer stack from `spec/03-architecture.md`, deepest first.
///
/// A crate may depend on any crate with a strictly lower rank and on nothing else in the
/// workspace. Crates in the same layer may not depend on each other either, because a
/// cycle within a layer is how a layer stops being a layer.
const LAYERS: &[(&str, u8)] = &[
    ("katsu-platform", 0),
    ("katsu-macros", 0),
    ("katsu-ir", 1),
    ("katsu-gc", 2),
    ("katsu-parse", 2),
    ("katsu-stencils", 2),
    ("katsu-vm", 3),
    ("katsu-builtins", 4),
    ("katsu-jit", 4),
    ("katsu-loop", 4),
    ("katsu-aot", 5),
    ("katsu-api", 5),
    ("katsu-node", 6),
    ("katsu-runtime", 7),
    ("katsu", 8),
];

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    kind: Option<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Task::Layers => check_layers(),
    }
}

fn check_layers() -> Result<()> {
    let ranks: BTreeMap<&str, u8> = LAYERS.iter().copied().collect();

    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;

    let mut violations = Vec::new();
    let mut checked = 0usize;

    for package in &metadata.packages {
        if !metadata.workspace_members.contains(&package.id) {
            continue;
        }
        let Some(&rank) = ranks.get(package.name.as_str()) else {
            // Tools and xtask sit outside the stack on purpose. They are allowed to depend
            // on anything, because nothing depends on them.
            continue;
        };
        checked += 1;

        for dependency in &package.dependencies {
            if dependency.kind.as_deref() == Some("dev") {
                continue;
            }
            let Some(&dependency_rank) = ranks.get(dependency.name.as_str()) else {
                continue;
            };
            if dependency_rank >= rank {
                violations.push(format!(
                    "{} (layer {}) depends on {} (layer {})",
                    package.name, rank, dependency.name, dependency_rank
                ));
            }
        }
    }

    if checked != LAYERS.len() {
        bail!(
            "expected {} crates in the layer stack, found {}. \
             A new crate needs a rank in xtask/src/main.rs.",
            LAYERS.len(),
            checked
        );
    }

    if violations.is_empty() {
        println!("layers: {checked} crates checked, dependency direction is downward");
        return Ok(());
    }

    for violation in &violations {
        eprintln!("  {violation}");
    }
    bail!(
        "{} upward dependency edge(s). The layer stack is in spec/03-architecture.md \
         and it is a rule, not a suggestion.",
        violations.len()
    )
}
