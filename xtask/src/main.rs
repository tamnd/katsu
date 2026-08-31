//! Repository automation.
//!
//! Every architectural rule in the specification that is not mechanically enforced will be
//! violated within a year, so the ones that matter get a test. This is where those tests
//! live. See `spec/16-package-layout.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    /// Run benchmarks on one of the reference machines from `spec/15-benchmarks.md`.
    Bench(BenchArgs),
    /// List the reference machines and say whether each one is reachable.
    Machines,
    /// Check that the workspace is in a state crates.io will accept.
    Release(ReleaseArgs),
    /// Upload every crate that is not on crates.io yet, in dependency order.
    Publish(PublishArgs),
}

#[derive(Debug, clap::Args)]
struct ReleaseArgs {
    /// The tag being released, so that `v0.1.7` and a workspace at 0.1.6 is a failure rather
    /// than a release of the wrong version. Omitted outside the release workflow.
    #[arg(long)]
    tag: Option<String>,
}

#[derive(Debug, clap::Args)]
struct PublishArgs {
    /// The tag being released, checked the way `cargo xtask release` checks it before anything
    /// is uploaded.
    #[arg(long)]
    tag: Option<String>,
    /// Say what would be uploaded and in what order, and upload nothing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, clap::Args)]
struct BenchArgs {
    /// Which reference machine to run on. See `cargo xtask machines`.
    #[arg(long, default_value = "m4")]
    machine: String,
    /// Which crate's benchmarks to run.
    #[arg(long, short = 'p', default_value = "katsu-vm")]
    package: String,
    /// Passed through to criterion, so `--filter encode` runs only the encode group.
    #[arg(long)]
    filter: Option<String>,
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
    version: String,
    description: Option<String>,
    license: Option<String>,
    readme: Option<String>,
    /// The registries this crate may go to. `None` is every registry, and an empty list is
    /// `publish = false`, which is how cargo says a crate stays in this repository.
    publish: Option<Vec<String>>,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    kind: Option<String>,
    /// The version requirement as cargo parsed it, so `version = "0.1.6"` arrives as `^0.1.6`.
    req: String,
    /// Set for a dependency that names a directory. Every edge between two crates in this
    /// workspace has one, and nothing outside the workspace does.
    path: Option<String>,
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
        Task::Bench(args) => bench(&args),
        Task::Machines => {
            list_machines();
            Ok(())
        }
        Task::Release(args) => check_release(args.tag.as_deref()),
        Task::Publish(args) => publish(&args),
    }
}

/// A machine benchmarks may run on.
///
/// The roster is in the source rather than in a config file because a benchmark result is only
/// worth anything if the machine it came from is pinned down, and a machine described in a file
/// somebody can edit without review is not pinned down.
struct Machine {
    /// The name used on the command line and in any published result.
    name: &'static str,
    /// The ssh host, or `None` for the machine this process is running on.
    host: Option<&'static str>,
    /// Wrapped around the remote command, for hosts where ssh does not land in a usable shell.
    /// `gamingpc` is a Windows box, so ssh arrives at cmd.exe and the Linux side is one hop
    /// further in.
    shell: Option<&'static str>,
    /// Whether the benchmark runs against Windows itself rather than a Linux shell on it.
    ///
    /// This changes everything about the remote side: the script is PowerShell rather than bash,
    /// the checkout lives on a drive letter, and pinning is `start /affinity` rather than
    /// `taskset`. It is a separate field rather than a guess from the host name because
    /// `gamingpc` and `gamingpc-win` are the same host and only one of them is Windows.
    windows: bool,
    /// The core benchmarks are pinned to, where pinning is available.
    pin: Option<u8>,
    /// What goes next to a number taken here.
    caveat: &'static str,
}

/// The reference machines from `spec/15-benchmarks.md` 15.5.
///
/// `server1`, `server2` and `server3` are deliberately absent. They are shared cloud instances
/// running under permanent load, they are useful for checking that something builds on a machine
/// we do not control, and a timing taken on one of them would be noise wearing a result's
/// clothing. Adding them here would make it too easy to publish that noise by accident.
const MACHINES: &[Machine] = &[
    Machine {
        name: "m4",
        host: None,
        shell: None,
        windows: false,
        pin: None,
        caveat: "a laptop, so a long suite will thermally throttle and the tail of it is not \
                 comparable with the head",
    },
    Machine {
        name: "gamingpc",
        host: Some("gamingpc"),
        shell: Some("wsl -d Ubuntu -- bash -c"),
        windows: false,
        pin: Some(4),
        caveat: "WSL2, which is a virtual machine, and turbo is not pinned yet",
    },
    // The same physical box as `gamingpc`, running Windows itself. Having both means a number
    // can be attributed to an operating system rather than to hardware, which is how the
    // quadratic commit pattern in the heap was found.
    Machine {
        name: "gamingpc-win",
        host: Some("gamingpc"),
        shell: None,
        windows: true,
        // Mask rather than core number: 3 is the two threads of the first performance core.
        // A 13900K has efficiency cores too and an unpinned run lands on one about half the
        // time, which halves every number and looks like a regression.
        pin: Some(3),
        caveat: "Windows with Defender live scanning on, and turbo is not pinned yet",
    },
];

fn machine(name: &str) -> Result<&'static Machine> {
    MACHINES.iter().find(|m| m.name == name).with_context(|| {
        let known: Vec<&str> = MACHINES.iter().map(|m| m.name).collect();
        format!(
            "no reference machine named {name}. Known machines: {}",
            known.join(", ")
        )
    })
}

fn list_machines() {
    for machine in MACHINES {
        let reachable = match machine.host {
            None => "local".to_string(),
            Some(host) => {
                let ok = Command::new("ssh")
                    .args([
                        "-o",
                        "BatchMode=yes",
                        "-o",
                        "ConnectTimeout=8",
                        host,
                        "exit",
                    ])
                    .status()
                    .is_ok_and(|status| status.success());
                if ok {
                    "reachable".to_string()
                } else {
                    format!("unreachable via ssh {host}")
                }
            }
        };
        println!("{:<14} {:<28} {}", machine.name, reachable, machine.caveat);
    }
}

fn bench(args: &BenchArgs) -> Result<()> {
    let machine = machine(&args.machine)?;

    let filter = args.filter.as_deref().unwrap_or("");
    let Some(host) = machine.host else {
        let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        command.args(["bench", "-p", &args.package]);
        if !filter.is_empty() {
            command.args(["--", filter]);
        }
        eprintln!(
            "bench: running on {} locally, {}",
            machine.name, machine.caveat
        );
        let status = command.status().context("running cargo bench")?;
        if !status.success() {
            bail!("cargo bench failed");
        }
        return Ok(());
    };

    // A remote run checks out a commit by hash, so the commit has to be somewhere the remote can
    // fetch it from. Refusing here is better than benchmarking whatever the remote happens to have
    // checked out and reporting it against the hash in the working tree.
    let commit = git(&["rev-parse", "HEAD"])?;
    if !git(&["status", "--porcelain"])?.is_empty() {
        eprintln!(
            "bench: the working tree has uncommitted changes and they will not be measured, \
             because the remote checks out {commit} by hash"
        );
    }
    if git(&["branch", "-r", "--contains", &commit])
        .unwrap_or_default()
        .is_empty()
    {
        bail!(
            "commit {commit} is not on any remote branch, so {host} cannot fetch it. \
             Push first, or run with --machine m4."
        );
    }

    // A filter is a regex, so the useful ones contain `|`, and a remote run is a line of shell
    // rather than an argument vector. Quoting is each script's own job below. What cannot be
    // quoted safely on both sides is a quote character itself, so that is refused here instead of
    // being mangled into a filter that silently matches something else.
    if filter.contains('"') {
        bail!("a benchmark filter cannot contain a double quote, and {filter} does");
    }
    let script = if machine.windows {
        windows_script(machine, &commit, &args.package, filter)
    } else {
        unix_script(machine, &commit, &args.package, filter)
    };

    eprintln!(
        "bench: running on {} at {commit}, {}",
        machine.name, machine.caveat
    );
    run_remote(host, machine.shell, &script, machine.windows)
}

/// The bash side of a remote run.
fn unix_script(machine: &Machine, commit: &str, package: &str, filter: &str) -> String {
    let pin = machine
        .pin
        .map_or(String::new(), |core| format!("taskset -c {core} "));
    // Single quotes, because a filter is a regex and bash would read the `|` in one as a pipeline.
    // The escape is the standard one for closing the quote, adding a literal quote and opening
    // again, which is the only way to get a quote inside single quotes in bash.
    let filter_arg = if filter.is_empty() {
        String::new()
    } else {
        format!(" -- '{}'", filter.replace('\'', "'\\''"))
    };
    format!(
        "set -e\n\
         export PATH=\"$HOME/.cargo/bin:$PATH\"\n\
         mkdir -p ~/katsu-bench-checkout\n\
         cd ~/katsu-bench-checkout\n\
         test -d katsu || git clone -q https://github.com/tamnd/katsu.git\n\
         cd katsu\n\
         git fetch -q origin\n\
         git checkout -q --detach {commit}\n\
         echo \"machine: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs), \
         $(nproc) threads\"\n\
         echo \"load: $(cut -d' ' -f1-3 /proc/loadavg)\"\n\
         echo \"toolchain: $(rustc --version)\"\n\
         echo \"commit: $(git log --oneline -1)\"\n\
         {pin}cargo bench -p {package}{filter_arg}\n"
    )
}

/// The PowerShell side of a remote run.
///
/// Pinning goes through `cmd /c start /affinity`, because that is the only way to set affinity on
/// a process from the outside before it starts, and the mask is inherited by the benchmark binary
/// cargo spawns. `/wait /b` keeps it in this console so its output comes back over ssh instead of
/// opening a window on somebody's desktop.
///
/// The cargo line goes into a batch file rather than into that command directly. A filter is a
/// regex with a `|` in it, the pinned form nests cmd inside cmd inside PowerShell, and cmd's rule
/// for which layer strips which quotes is not something to rely on three levels deep. Inside a
/// batch file a double quoted argument is passed through untouched, which is all we need.
fn windows_script(machine: &Machine, commit: &str, package: &str, filter: &str) -> String {
    let filter_arg = if filter.is_empty() {
        String::new()
    } else {
        format!(" -- \"{filter}\"")
    };
    // The batch file's one line is built as a PowerShell single quoted string, where the only
    // character with a meaning is the quote itself, and a doubled quote is a literal one.
    let path = r"C:\katsu-bench-checkout\bench.cmd";
    let line = format!("cargo bench -p {package}{filter_arg}").replace('\'', "''");
    let run = match machine.pin {
        Some(mask) => format!("cmd /c \"start /wait /b /affinity {mask:x} cmd /c {path}\""),
        None => format!("cmd /c {path}"),
    };
    let pinned = format!("Set-Content -Path {path} -Encoding ASCII -Value '{line}'\n{run}");
    format!(
        // Progress records come back over ssh as raw CLIXML in the middle of the results, so
        // they are turned off rather than filtered out afterwards.
        "$ErrorActionPreference = 'Stop'\n\
         $ProgressPreference = 'SilentlyContinue'\n\
         $env:PATH = \"$env:USERPROFILE\\.cargo\\bin;$env:PATH\"\n\
         New-Item -ItemType Directory -Force -Path C:\\katsu-bench-checkout | Out-Null\n\
         Set-Location C:\\katsu-bench-checkout\n\
         if (-not (Test-Path katsu)) {{ git clone -q https://github.com/tamnd/katsu.git }}\n\
         Set-Location katsu\n\
         git fetch -q origin\n\
         git checkout -q --detach {commit}\n\
         Write-Output \"machine: $((Get-CimInstance Win32_Processor).Name), \
         $env:NUMBER_OF_PROCESSORS threads\"\n\
         Write-Output \"toolchain: $(rustc --version)\"\n\
         Write-Output \"commit: $(git log --oneline -1)\"\n\
         {pinned}\n"
    )
}

/// Ship a script to a remote host and run it.
///
/// The script is base64 encoded rather than quoted because `gamingpc` routes through cmd.exe on
/// the way to a Linux shell, and getting a multi line script with quotes and pipes through two
/// layers of unrelated quoting rules intact is not a problem worth solving twice. PowerShell has
/// the same encoding built in as `-EncodedCommand`, which wants UTF-16 rather than UTF-8.
fn run_remote(host: &str, shell: Option<&str>, script: &str, windows: bool) -> Result<()> {
    let remote = if windows {
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        format!(
            "powershell -NoProfile -NonInteractive -EncodedCommand {}",
            base64(&utf16)
        )
    } else {
        let encoded = base64(script.as_bytes());
        let inner =
            format!("echo {encoded} | base64 -d > /tmp/xtask-bench.sh; bash /tmp/xtask-bench.sh");
        match shell {
            Some(shell) => format!("{shell} \"{inner}\""),
            None => inner,
        }
    };

    let status = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host])
        .arg(remote)
        .status()
        .with_context(|| format!("running ssh {host}"))?;
    if !status.success() {
        bail!("the benchmark run on {host} failed");
    }
    Ok(())
}

/// Standard base64, no line breaks.
///
/// Twelve lines against a dependency that would be in the tree forever for this one use.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The workspace as cargo sees it, which is the only description of it that cannot drift.
fn metadata() -> Result<Metadata> {
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
    serde_json::from_slice(&output.stdout).context("parsing cargo metadata")
}

fn check_layers() -> Result<()> {
    let ranks: BTreeMap<&str, u8> = LAYERS.iter().copied().collect();
    let metadata = metadata()?;

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

/// Check that the workspace is in a state crates.io will accept.
///
/// A publish is the one operation in this repository that cannot be taken back. A version on
/// crates.io can be yanked but never replaced, so every mistake worth catching has to be caught
/// before the upload rather than after it, and the release workflow runs this before it publishes
/// anything. Every rule here is one that produced a bad release somewhere else first.
///
/// The version an internal dependency asks for is the rule that matters most. Cargo resolves a
/// path dependency by path when it builds the workspace and by version when somebody else builds
/// the published crate, so a stale version requirement is invisible here and wrong everywhere
/// else. Requiring it to be exactly the version being released means the two resolutions cannot
/// disagree.
fn check_release(tag: Option<&str>) -> Result<()> {
    let metadata = metadata()?;
    let members: Vec<&Package> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .collect();
    let names: Vec<&str> = members
        .iter()
        .map(|package| package.name.as_str())
        .collect();

    // An empty registry list is `publish = false`. Those crates are the tools and the repository
    // automation, they are not part of the product, and they are checked only for the one thing
    // that would break the publish of everything else.
    let (published, held): (Vec<&&Package>, Vec<&&Package>) = members
        .iter()
        .partition(|package| goes_to_a_registry(package));
    if published.is_empty() {
        bail!("no crate in this workspace is publishable, which cannot be right");
    }

    let mut problems = Vec::new();

    // One version across the workspace, because the crates are released in lockstep and a reader
    // who sees katsu 0.1.7 has no way to know which katsu-vm went into it otherwise.
    let version = published[0].version.clone();
    for package in &published {
        if package.version != version {
            problems.push(format!(
                "{} is at {} and {} is at {}, and the workspace releases in lockstep",
                package.name, package.version, published[0].name, version
            ));
        }
        // crates.io rejects an upload without either of these, and finding that out from a failed
        // release job means the tag is already pushed.
        if package
            .description
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            problems.push(format!("{} has no description", package.name));
        }
        if package.license.as_deref().unwrap_or("").trim().is_empty() {
            problems.push(format!("{} has no license", package.name));
        }
        // Not required by the registry, and the crates.io page for a crate without one is a
        // title and a single sentence, which is not what somebody deciding whether to depend
        // on this needs to see.
        if package.readme.as_deref().unwrap_or("").trim().is_empty() {
            problems.push(format!("{} has no readme", package.name));
        }
    }

    let wanted = format!("^{version}");
    for package in &published {
        for dependency in &package.dependencies {
            if dependency.kind.as_deref() == Some("dev") || dependency.path.is_none() {
                continue;
            }
            if !names.contains(&dependency.name.as_str()) {
                continue;
            }
            if held.iter().any(|held| held.name == dependency.name) {
                problems.push(format!(
                    "{} depends on {}, which is not published, so the upload would not resolve",
                    package.name, dependency.name
                ));
            }
            if dependency.req != wanted {
                problems.push(format!(
                    "{} asks for {} {} and the workspace is at {version}",
                    package.name, dependency.name, dependency.req
                ));
            }
        }
    }

    // The tag is what triggers the release, so it is the one input nothing else can check.
    if let Some(tag) = tag {
        let expected = format!("v{version}");
        if tag != expected {
            problems.push(format!(
                "the tag is {tag} and the workspace is at {version}, which would publish {expected}"
            ));
        }
    }

    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        bail!(
            "{} problem(s) between this workspace and crates.io",
            problems.len()
        );
    }

    println!(
        "release: {} crates at {version}, {} held back, ready to publish",
        published.len(),
        held.len()
    );
    Ok(())
}

/// Whether cargo will send this crate to a registry at all.
///
/// An empty registry list is how `publish = false` arrives in the metadata, and that is how the
/// tools and the repository automation say they stay in this repository.
fn goes_to_a_registry(package: &Package) -> bool {
    package.publish.as_ref().is_none_or(|to| !to.is_empty())
}

/// The months of an HTTP date, in the order the format names them.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The longest a rate limit is allowed to hold the release up before it is treated as something
/// other than a rate limit. crates.io refills one new crate slot every ten minutes, so a sane wait
/// is never more than that plus a little, and a much longer one means the message was misread.
const LONGEST_WAIT: Duration = Duration::from_mins(45);

/// How long one crate may sit in the rate limit before the release stops rather than waits.
///
/// A wait that goes past this is not a queue any more. Two refills and a bit is enough for every
/// shape of the limit we have actually seen, and something that outlasts it is an account level
/// limit that no amount of patience clears, which is worth being told about rather than sitting
/// through for the rest of the job's timeout.
const LONGEST_STALL: Duration = Duration::from_mins(60);

/// How often crates.io lets one account create a crate name that nobody has published before.
///
/// Published at <https://crates.io/docs/rate-limits> as a burst of five and then one every ten
/// minutes. The limit for a new version of a crate that already exists is a burst of thirty and one
/// a minute, which fifteen crates never come close to.
const NEW_NAME_REFILL: Duration = Duration::from_mins(10);

/// How far past a deadline to ask, so that a clock a second out of step is not the reason a release
/// fails.
const SLACK: Duration = Duration::from_secs(15);

/// Upload every crate that is not on crates.io yet, in dependency order, one at a time.
///
/// `cargo publish --workspace` is the better command and it is what CI dry runs, because it works
/// the order out from the dependency graph and verifies the whole set builds as if it were already
/// published before it uploads any of it. What it cannot do is survive crates.io's rate limit: a
/// user account may publish a burst of five crate names and then one every ten minutes, so a first
/// release of fifteen crates stops partway through with a 429 and no way to resume, which is
/// exactly what happened to `v0.1.7`. Cargo has no flag for skipping what is already uploaded, so
/// the question is asked per crate here instead, and a re-run finishes what a stopped run started.
///
/// This is slow on a first release and free afterwards. Once every name exists the limit is the
/// one for new versions of existing crates, which is a burst of thirty, and fifteen crates never
/// reach it.
///
/// The pace between new names is deliberate rather than reactive. See [`pace`] for what the
/// registry taught us about asking early.
fn publish(args: &PublishArgs) -> Result<()> {
    // Every rule this checks is one that cannot be undone once an upload has happened, so it runs
    // before the first one rather than being trusted from an earlier job.
    check_release(args.tag.as_deref())?;

    let metadata = metadata()?;
    let members: Vec<&Package> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .collect();
    let order = publish_order(&members)?;
    println!(
        "publish: {} crates, in this order: {}",
        order.len(),
        order
            .iter()
            .map(|package| package.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut uploaded = 0usize;
    let mut skipped = 0usize;
    let mut last_new_name: Option<SystemTime> = None;
    for package in order {
        if on_crates_io(&package.name, &package.version)? {
            println!(
                "publish: {} {} is already on crates.io",
                package.name, package.version
            );
            skipped += 1;
            continue;
        }
        // A name nobody has published before is the expensive kind of upload, and it is worth one
        // extra request to find out which kind this is, because the two limits are two orders of
        // magnitude apart and pacing a new version the way a new name has to be paced would turn a
        // two minute release into a two hour one.
        let new_name = !name_on_crates_io(&package.name)?;
        if args.dry_run {
            let kind = if new_name {
                "a new name"
            } else {
                "a new version"
            };
            println!(
                "publish: {} {} would be uploaded, {kind}",
                package.name, package.version
            );
            uploaded += 1;
            continue;
        }
        if new_name && let Some(previous) = last_new_name {
            pace(previous);
        }
        upload(&package.name)?;
        if new_name {
            last_new_name = Some(SystemTime::now());
        }
        uploaded += 1;
    }

    let verb = if args.dry_run {
        "to upload"
    } else {
        "uploaded"
    };
    println!("publish: {uploaded} {verb}, {skipped} already there");
    Ok(())
}

/// The publishable crates, in an order where every crate comes after everything it depends on.
///
/// Taken from the dependency graph rather than from the layer table above it. The layer table is a
/// rule about what may depend on what and this is a fact about what does, and uploading in an order
/// derived from the rule would put an upload at the mercy of the rule being right.
fn publish_order<'a>(members: &[&'a Package]) -> Result<Vec<&'a Package>> {
    let publishable: Vec<&&Package> = members
        .iter()
        .filter(|package| goes_to_a_registry(package))
        .collect();
    let names: BTreeSet<&str> = publishable
        .iter()
        .map(|package| package.name.as_str())
        .collect();

    // What each crate is waiting for. A build dependency counts and a dev dependency does not,
    // because a dev dependency is not part of what somebody downloading the crate resolves.
    let mut waiting: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for package in &publishable {
        let blockers = package
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind.as_deref() != Some("dev"))
            .map(|dependency| dependency.name.as_str())
            .filter(|name| names.contains(name))
            .collect();
        waiting.insert(package.name.as_str(), blockers);
    }

    let mut order = Vec::with_capacity(publishable.len());
    let mut done: BTreeSet<&str> = BTreeSet::new();
    while done.len() < publishable.len() {
        // Alphabetical among everything that is ready, so two runs of the same workspace upload in
        // the same order and a stopped run resumes where the log says it stopped.
        let ready: Option<&str> = waiting
            .iter()
            .filter(|(name, blockers)| !done.contains(*name) && blockers.is_subset(&done))
            .map(|(name, _)| *name)
            .next();
        let Some(name) = ready else {
            let stuck: Vec<&str> = waiting
                .keys()
                .filter(|name| !done.contains(*name))
                .copied()
                .collect();
            bail!(
                "these crates depend on each other in a cycle, so there is no order to upload \
                 them in: {}",
                stuck.join(", ")
            );
        };
        done.insert(name);
        order.push(
            **publishable
                .iter()
                .find(|package| package.name == name)
                .expect("the name came from this list"),
        );
    }
    Ok(order)
}

/// Ask crates.io whether a version is already there.
///
/// Through curl rather than an HTTP client, because the only alternative is a dependency tree in
/// the repository automation for one request, and curl is on every machine this runs on. A status
/// that is neither 200 nor 404 stops the release, since a registry that cannot answer the question
/// is not a registry to start uploading to.
fn on_crates_io(name: &str, version: &str) -> Result<bool> {
    ask_crates_io(&format!("https://crates.io/api/v1/crates/{name}/{version}"))
}

/// Ask crates.io whether a crate name exists at all, whoever owns it.
///
/// This decides which of the two rate limits the upload is subject to, which decides how long to
/// wait before the one after it. It is a different question from whether the version is there: a
/// crate that stopped halfway through a release has some names taken and some free, and the free
/// ones are the slow ones.
fn name_on_crates_io(name: &str) -> Result<bool> {
    ask_crates_io(&format!("https://crates.io/api/v1/crates/{name}"))
}

/// One question to the registry, answered as a yes or a no.
fn ask_crates_io(url: &str) -> Result<bool> {
    // crates.io asks for one request a second and for a user agent that says who is calling.
    std::thread::sleep(Duration::from_secs(1));
    let output = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-A",
            "katsu release automation (https://github.com/tamnd/katsu)",
            url,
        ])
        .output()
        .context("running curl")?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "200" => Ok(true),
        "404" => Ok(false),
        other => bail!("crates.io answered {other} for {url}, which is not an answer"),
    }
}

/// Wait for crates.io to refill the slot the next new crate name needs.
///
/// Waiting only when the registry says to is not enough, because a request it refuses costs the
/// same token as one it accepts. Every attempt `v0.1.8` made the moment the message said it may
/// moved the next deadline ten minutes further out, three runs in a row, and the only reading of
/// the bucket in `rate_limiter.rs` that produces that is one where a refusal spends the token that
/// had just arrived. Asking early does not only fail, it takes the slot away from the attempt after
/// it, so a run that keeps asking never publishes anything again.
///
/// Ten minutes a crate is the rate the registry publishes, so this is the fastest a first release
/// of a workspace this size can honestly go. Everything after the first release is a new version
/// rather than a new name and none of this runs.
fn pace(previous: SystemTime) {
    let since = SystemTime::now()
        .duration_since(previous)
        .unwrap_or(Duration::ZERO);
    let left = (NEW_NAME_REFILL + SLACK).saturating_sub(since);
    if left.is_zero() {
        return;
    }
    println!(
        "publish: waiting {} seconds for crates.io to refill a new crate name slot",
        left.as_secs()
    );
    std::thread::sleep(left);
}

/// Upload one crate, waiting out a rate limit rather than failing the release on one.
///
/// There is no attempt count, because a count is the wrong thing to spend: what the release can
/// afford is time, and [`LONGEST_STALL`] says how much of it one crate may have. Giving up after
/// three tries meant giving up thirty minutes into a limit that refills every ten, which is a
/// failure with the answer already in sight.
fn upload(name: &str) -> Result<()> {
    let mut before: Option<SystemTime> = None;
    let mut waited = Duration::ZERO;
    let mut attempt = 0;
    loop {
        attempt += 1;
        println!("publish: uploading {name}");
        let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["publish", "-p", name, "--locked"])
            .output()
            .with_context(|| format!("running cargo publish -p {name}"))?;
        // Cargo says everything worth reading on stderr, including the progress of the upload and
        // the wait for the index, so it goes through whether this worked or not.
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let Some(until) = rate_limited_until(&stderr) else {
            bail!("publishing {name} failed");
        };
        let wait = backoff(until, before, SystemTime::now());
        before = Some(until);
        if wait > LONGEST_WAIT {
            bail!(
                "crates.io asked us to wait {} minutes before uploading {name}, which is longer \
                 than a rate limit should ever be",
                wait.as_secs() / 60
            );
        }
        waited += wait;
        if waited > LONGEST_STALL {
            bail!(
                "crates.io has refused {name} for {} minutes without letting it through, which is \
                 longer than the limit it names takes to refill, so this is an account level limit \
                 rather than a queue and waiting is not going to clear it. help@crates.io is the \
                 address that raises one.",
                waited.as_secs() / 60
            );
        }
        println!(
            "publish: crates.io is rate limiting new crates, waiting {} seconds before attempt {} \
             at {name}",
            wait.as_secs(),
            attempt + 1
        );
        std::thread::sleep(wait);
    }
}

/// How long to wait after a refusal, given the deadline crates.io just named and the one it named
/// before it.
///
/// The first refusal is waited out to the deadline and no further, since the deadline is the
/// registry's own answer to when the slot is there. A second refusal means the deadline was not the
/// whole story, and the gap between the two is the refill period the registry is working to, so the
/// wait after it is a whole period longer. That is what breaks the loop described in [`pace`]: two
/// periods of not asking is what it takes to get back a token that repeated asking has been
/// spending.
///
/// The period comes out of the two deadlines rather than a constant, because the same message
/// carries both limits and they are ten minutes apart and one minute apart.
fn backoff(until: SystemTime, before: Option<SystemTime>, now: SystemTime) -> Duration {
    let extra = match before {
        None => Duration::ZERO,
        Some(before) => match until.duration_since(before) {
            // A deadline that did not move says nothing about the period, so fall back to the
            // slower of the two limits, which is the one a stuck release is always stuck on.
            Ok(gap) if !gap.is_zero() => gap,
            _ => NEW_NAME_REFILL,
        },
    };
    until.duration_since(now).unwrap_or(Duration::ZERO) + extra + SLACK
}

/// The time crates.io said to try again, out of the message it says it in.
///
/// The message is `You have published too many new crates in a short period of time. Please try
/// again after Mon, 31 Aug 2026 12:56:01 GMT and see https://crates.io/docs/rate-limits for more
/// details.` Reading the time out of it beats sleeping a guessed interval, because the same limit
/// refills every ten minutes for a new crate and every minute for a new version of one that exists.
fn rate_limited_until(stderr: &str) -> Option<SystemTime> {
    let (_, after) = stderr.split_once("Please try again after ")?;
    let (date, _) = after.split_once(" GMT")?;
    Some(UNIX_EPOCH + Duration::from_secs(http_date(date)?))
}

/// Seconds since the epoch, from the `Mon, 31 Aug 2026 12:56:01` half of an HTTP date.
///
/// Hand written for the same reason base64 above is: it is twenty lines against a date library
/// that would be in the tree forever for one call, and the answer is checked against known
/// timestamps in the tests.
fn http_date(text: &str) -> Option<u64> {
    let (_, rest) = text.trim().split_once(", ")?;
    let mut fields = rest.split_whitespace();
    let day: i64 = fields.next()?.parse().ok()?;
    let name = fields.next()?;
    let month = i64::try_from(MONTHS.iter().position(|month| *month == name)?).ok()? + 1;
    let year: i64 = fields.next()?.parse().ok()?;
    let mut clock = fields.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let seconds = days_since_epoch(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(seconds).ok()
}

/// Days from 1970-01-01 to a civil date, by Howard Hinnant's algorithm.
///
/// It works by counting from March, which puts the leap day at the end of a year rather than in
/// the middle of one, and by counting in 400 year eras, which is the period the whole calendar
/// repeats on.
fn days_since_epoch(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted = (month + 9) % 12;
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::{
        MACHINES, NEW_NAME_REFILL, SLACK, backoff, base64, days_since_epoch, http_date, machine,
        rate_limited_until, unix_script, windows_script,
    };

    #[test]
    fn base64_matches_the_reference_vectors() {
        // RFC 4648 section 10, which exists precisely so that hand rolled encoders can be
        // checked against something rather than against the author's confidence.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_the_bytes_a_shell_script_actually_contains() {
        // Newlines, quotes and the high bit, which is what a script with a stray non ASCII
        // character in a comment looks like.
        let script = "set -e\necho \"hello | world\"\n# \u{00e9}\n";
        assert_eq!(
            base64(script.as_bytes()),
            "c2V0IC1lCmVjaG8gImhlbGxvIHwgd29ybGQiCiMgw6kK"
        );
    }

    #[test]
    fn the_machine_roster_is_addressable_by_name() {
        for entry in MACHINES {
            assert_eq!(machine(entry.name).unwrap().name, entry.name);
        }
        assert!(
            machine("server3").is_err(),
            "cloud boxes are not reference machines"
        );
    }

    #[test]
    fn the_two_faces_of_one_box_are_separate_entries_on_the_same_host() {
        // Same hardware, different operating system. Keeping them as two names is what lets a
        // published number say which one it came from, which is the only reason the pair is
        // useful at all.
        let linux = machine("gamingpc").unwrap();
        let windows = machine("gamingpc-win").unwrap();
        assert_eq!(linux.host, windows.host);
        assert!(!linux.windows);
        assert!(windows.windows);
    }

    #[test]
    fn each_script_pins_the_way_its_platform_pins() {
        let linux = machine("gamingpc").unwrap();
        let script = unix_script(linux, "abc123", "katsu-gc", "allocate");
        assert!(script.contains("taskset -c 4 cargo bench -p katsu-gc -- 'allocate'"));
        assert!(script.contains("git checkout -q --detach abc123"));

        let windows = machine("gamingpc-win").unwrap();
        let script = windows_script(windows, "abc123", "katsu-gc", "allocate");
        // The mask is hexadecimal because that is what `start /affinity` reads it as, and 3 is
        // the two threads of the first performance core.
        assert!(
            script.contains(r"start /wait /b /affinity 3 cmd /c C:\katsu-bench-checkout\bench.cmd")
        );
        assert!(script.contains("git checkout -q --detach abc123"));
        assert!(
            !script.contains("taskset"),
            "the windows script should not be reaching for a linux tool"
        );
    }

    #[test]
    fn a_filter_reaches_the_remote_as_one_argument_and_not_as_a_pipeline() {
        // Every useful criterion filter is a regex with a `|` in it, and both remote scripts are
        // a line of shell rather than an argument vector. An unquoted one was read as a pipeline,
        // which piped cargo into a program named after the second half of the filter and reported
        // the result as a broken pipe.
        let linux = unix_script(machine("gamingpc").unwrap(), "abc123", "katsu-vm", "a|b");
        assert!(linux.contains("cargo bench -p katsu-vm -- 'a|b'"));

        let windows = windows_script(
            machine("gamingpc-win").unwrap(),
            "abc123",
            "katsu-vm",
            "a|b",
        );
        assert!(windows.contains(r#"-Value 'cargo bench -p katsu-vm -- "a|b"'"#));
    }

    #[test]
    fn an_empty_filter_runs_every_benchmark_in_the_package() {
        let linux = unix_script(machine("gamingpc").unwrap(), "abc123", "katsu-vm", "");
        assert!(linux.contains("cargo bench -p katsu-vm\n"));
        assert!(!linux.contains(" -- "));

        let windows = windows_script(machine("gamingpc-win").unwrap(), "abc123", "katsu-vm", "");
        assert!(windows.contains("-Value 'cargo bench -p katsu-vm'"));
    }

    #[test]
    fn the_epoch_and_a_few_dates_around_it_line_up() {
        // Two of these are the whole point: 2000 is a leap year because it is divisible by 400 and
        // 1900 is not because it is divisible by 100, which is the pair every hand written date
        // routine gets wrong first.
        assert_eq!(days_since_epoch(1970, 1, 1), 0);
        assert_eq!(days_since_epoch(1969, 12, 31), -1);
        assert_eq!(days_since_epoch(2000, 3, 1), 11017);
        assert_eq!(days_since_epoch(2000, 2, 29), 11016);
        assert_eq!(days_since_epoch(1900, 3, 1), -25508);
    }

    #[test]
    fn an_http_date_reads_as_the_timestamp_it_names() {
        // Checked against `date -u -d ... +%s` rather than against arithmetic done here twice.
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00"), Some(0));
        assert_eq!(http_date("Mon, 31 Aug 2026 12:56:01"), Some(1_788_180_961));
        assert_eq!(http_date("Sun, 06 Nov 1994 08:49:37"), Some(784_111_777));
        // Anything else is a message that changed shape, and guessing at a wait from a message we
        // no longer understand is worse than failing the release.
        assert_eq!(http_date("whenever you like"), None);
        assert_eq!(http_date("Mon, 31 Foo 2026 12:56:01"), None);
        assert_eq!(http_date("Mon, 31 Aug 2026 25:56:01"), None);
    }

    #[test]
    fn the_rate_limit_message_gives_up_its_time() {
        // The real message, from the run that stopped v0.1.7 one crate in.
        let stderr = "error: failed to publish katsu-macros v0.1.7 to registry at \
                      https://crates.io\n\nCaused by:\n  the remote server responded with an \
                      error (status 429 Too Many Requests): You have published too many new \
                      crates in a short period of time. Please try again after Mon, 31 Aug 2026 \
                      12:56:01 GMT and see https://crates.io/docs/rate-limits for more details.\n";
        assert_eq!(
            rate_limited_until(stderr),
            Some(UNIX_EPOCH + Duration::from_secs(1_788_180_961))
        );
        // A failure that is not a rate limit has no time in it and must not be waited out.
        assert_eq!(
            rate_limited_until("error: failed to verify package tarball"),
            None
        );
    }

    #[test]
    fn the_first_refusal_is_waited_out_to_the_deadline_and_no_further() {
        let now = UNIX_EPOCH + Duration::from_secs(1_788_180_961);
        let until = now + Duration::from_secs(200);
        assert_eq!(backoff(until, None, now), Duration::from_secs(200) + SLACK);
    }

    #[test]
    fn a_second_refusal_waits_a_whole_refill_longer() {
        // The two deadlines v0.1.8 was given, ten minutes apart, and the moment the run asked
        // again, which was fifteen seconds after the first of them and was refused for it.
        let first = UNIX_EPOCH + Duration::from_secs(1_788_180_961);
        let second = first + NEW_NAME_REFILL;
        let now = first + SLACK;
        // Five hundred and eighty five seconds to the second deadline, then ten minutes past it
        // so that two refills have gone by rather than one, then the slack.
        assert_eq!(
            backoff(second, Some(first), now),
            Duration::from_secs(585) + NEW_NAME_REFILL + SLACK
        );
    }

    #[test]
    fn a_deadline_the_registry_did_not_move_still_costs_a_refill() {
        let deadline = UNIX_EPOCH + Duration::from_secs(1_788_180_961);
        let now = deadline + Duration::from_secs(31);
        // The deadline is already past, so all of the wait is the refill and the slack.
        assert_eq!(
            backoff(deadline, Some(deadline), now),
            NEW_NAME_REFILL + SLACK
        );
    }

    #[test]
    fn a_crate_is_uploaded_after_everything_it_depends_on() {
        let json = serde_json::json!([
            {"id": "a", "name": "katsu", "version": "0.1.7", "description": "d",
             "license": "MIT", "readme": "r", "publish": null, "dependencies": [
                {"name": "katsu-runtime", "kind": null, "req": "^0.1.7", "path": "/w/r"},
                {"name": "anyhow", "kind": null, "req": "^1", "path": null}]},
            {"id": "b", "name": "katsu-runtime", "version": "0.1.7", "description": "d",
             "license": "MIT", "readme": "r", "publish": null, "dependencies": [
                {"name": "katsu-ir", "kind": null, "req": "^0.1.7", "path": "/w/i"}]},
            {"id": "c", "name": "katsu-ir", "version": "0.1.7", "description": "d",
             "license": "MIT", "readme": "r", "publish": null, "dependencies": [
                {"name": "katsu", "kind": "dev", "req": "^0.1.7", "path": "/w/a"}]},
            {"id": "d", "name": "xtask", "version": "0.1.7", "description": null,
             "license": null, "readme": null, "publish": [], "dependencies": []},
        ]);
        let packages: Vec<super::Package> = serde_json::from_value(json).unwrap();
        let members: Vec<&super::Package> = packages.iter().collect();
        let order = super::publish_order(&members).unwrap();
        let names: Vec<&str> = order.iter().map(|package| package.name.as_str()).collect();
        // katsu-ir goes first even though katsu dev depends on it, because a dev dependency is not
        // part of what somebody downloading the crate resolves and counting it would be a cycle.
        // xtask is not there at all, because it is `publish = false`.
        assert_eq!(names, ["katsu-ir", "katsu-runtime", "katsu"]);
    }

    #[test]
    fn a_cycle_is_named_rather_than_looped_on() {
        let json = serde_json::json!([
            {"id": "a", "name": "one", "version": "0.1.7", "description": "d", "license": "MIT",
             "readme": "r", "publish": null, "dependencies": [
                {"name": "two", "kind": null, "req": "^0.1.7", "path": "/w/two"}]},
            {"id": "b", "name": "two", "version": "0.1.7", "description": "d", "license": "MIT",
             "readme": "r", "publish": null, "dependencies": [
                {"name": "one", "kind": null, "req": "^0.1.7", "path": "/w/one"}]},
        ]);
        let packages: Vec<super::Package> = serde_json::from_value(json).unwrap();
        let members: Vec<&super::Package> = packages.iter().collect();
        let error = match super::publish_order(&members) {
            Ok(order) => panic!("a cycle produced an order of {} crates", order.len()),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("one, two"), "{error}");
    }

    #[test]
    fn a_powershell_payload_is_utf16_before_it_is_base64() {
        // PowerShell's -EncodedCommand reads UTF-16LE, so encoding the UTF-8 bytes instead
        // produces a script that decodes to something that looks like it was chopped in half.
        // This is the vector for "Set-Location", which is enough to catch a byte order slip.
        let utf16: Vec<u8> = "Set-Location"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(base64(&utf16), "UwBlAHQALQBMAG8AYwBhAHQAaQBvAG4A");
    }
}
