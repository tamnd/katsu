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
    /// Run benchmarks on one of the reference machines from `spec/15-benchmarks.md`.
    Bench(BenchArgs),
    /// List the reference machines and say whether each one is reachable.
    Machines,
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
        Task::Bench(args) => bench(&args),
        Task::Machines => {
            list_machines();
            Ok(())
        }
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

#[cfg(test)]
mod tests {
    use super::{MACHINES, base64, machine, unix_script, windows_script};

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
        let script = unix_script(linux, "abc123", "katsu-gc", " -- allocate");
        assert!(script.contains("taskset -c 4 cargo bench -p katsu-gc -- allocate"));
        assert!(script.contains("git checkout -q --detach abc123"));

        let windows = machine("gamingpc-win").unwrap();
        let script = windows_script(windows, "abc123", "katsu-gc", " -- allocate");
        // The mask is hexadecimal because that is what `start /affinity` reads it as, and 3 is
        // the two threads of the first performance core.
        assert!(script.contains("start /wait /b /affinity 3 cmd /c cargo bench -p katsu-gc"));
        assert!(script.contains("git checkout -q --detach abc123"));
        assert!(
            !script.contains("taskset"),
            "the windows script should not be reaching for a linux tool"
        );
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
