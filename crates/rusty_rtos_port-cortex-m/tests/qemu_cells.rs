//! The QEMU cells, run as host tests so `cargo mutants` can judge the
//! ports by them.
//!
//! # Why this file exists
//!
//! `rusty_rtos_port-cortex-m` and `rusty_rtos_port-riscv` had **no tests at
//! all**, and mutating them on the host measures nothing: their source is
//! arch-gated, so on x86-64 the mutation lands in code that is not
//! compiled, the build succeeds, the tests pass, and every mutant is
//! scored MISSED. Measured: 87 survivors in the cortex-m crate and 47 in
//! the riscv one, every one of them that artefact.
//!
//! The cells under `firmware/` DO execute that code, on its own
//! architecture, and each already ends in `debug::exit(EXIT_SUCCESS)` — so
//! they are gates rather than something to watch. What they are not is
//! reachable from `cargo test`, and `cargo mutants` runs nothing else
//! (`--test-tool` takes `cargo` or `nextest`, and that is the whole list).
//!
//! # A broken port HANGS, so the deadline is not optional
//!
//! Proved, not assumed: pointing `PENDSVSET` at the wrong bit makes the
//! switch cell run for ever rather than fail — PendSV never fires, no
//! switch happens, and nothing reaches the semihosting exit. QEMU has no
//! self-terminate. A cell run without a deadline therefore hangs the whole
//! mutation run, which is exactly what the kernel's time-stopping mutants
//! did to `cargo mutants` (see `docs/HOLES.md`, H4).
//!
//! So this launches QEMU **directly**, as a child it can kill, rather than
//! through `cargo run` — killing `cargo` on Windows leaves QEMU orphaned.
//! A cell that overruns is a FAILURE, which is the right verdict: a port
//! that stops switching is detected.
//!
//! # Why they are gated on an environment variable
//!
//! A QEMU cell costs seconds and this crate's own suite costs
//! milliseconds; making every `cargo test` pay for six QEMU boots is a tax
//! on every unrelated run. `tools/mutants-qemu.sh` sets the variable and
//! says so in its output.
//!
//! # The cells that also need the kernel
//!
//! `switch` depends on `rusty_rtos_port-cortex-m` alone. The others also
//! pull in `rusty_rtos_kernel`, so they must not be run while THAT
//! repository is being mutated in place: a mutated kernel would fail them
//! and the failure would be scored against the port.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Set this to run the cells. See the module note.
const GATE: &str = "KAIROS_QEMU_CELLS";

/// The deadline covers the QEMU RUN only -- the build happens before it,
/// undeadlined -- and a healthy cell boots and finishes in three to four
/// seconds. Twenty is generous for that and four and a half times cheaper
/// than ninety when a mutant HANGS, which is the common case here: a port
/// that stops switching never reaches the semihosting exit, so nearly
/// every caught mutant is caught by this timeout rather than by a failed
/// assertion.
const DEADLINE: Duration = Duration::from_secs(20);

fn enabled() -> bool {
    std::env::var_os(GATE).is_some()
}

fn firmware(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("firmware")
        .join(name)
}

/// The value of `key = "..."` in a cargo config, first occurrence.
fn field(text: &str, key: &str) -> Option<String> {
    let at = text.find(key)?;
    let rest = &text[at + key.len()..];
    let open = rest.find('"')?;
    let rest = &rest[open + 1..];
    let close = rest.find('"')?;
    Some(rest[..close].to_owned())
}

/// Build the cell, then run its QEMU command as a direct child under a
/// deadline. Answers whether it passed, and what it said.
fn cell(name: &str) -> (bool, String) {
    let dir = firmware(name);
    assert!(dir.is_dir(), "no such cell: {}", dir.display());

    // A nested cargo inherits the outer one's build environment, and a
    // leaked target or RUSTFLAGS silently retargets the cell.
    let mut build = Command::new(env!("CARGO"));
    build
        .args(["build", "--release"])
        .current_dir(&dir)
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS");
    let built = build.output().expect("cargo build");
    if !built.status.success() {
        return (false, String::from_utf8_lossy(&built.stderr).into_owned());
    }

    // The cell's own config is the single source of truth for both the
    // target triple and the QEMU command line.
    let cfg = std::fs::read_to_string(dir.join(".cargo").join("config.toml"))
        .expect("the cell has a .cargo/config.toml");
    let target = field(&cfg, "target = ").expect("a [build] target");
    let runner = field(&cfg, "runner = ").expect("a runner");

    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("Cargo.toml");
    let bin = field(&manifest, "name = ").expect("a package name");

    let elf = dir.join("target").join(&target).join("release").join(&bin);
    assert!(elf.is_file(), "no ELF at {}", elf.display());

    let mut parts = runner.split_whitespace();
    let program = parts.next().expect("a runner program");
    let log = std::env::temp_dir().join(format!("kairos-cell-{name}.log"));
    let sink = std::fs::File::create(&log).expect("a log file");
    let errs = sink.try_clone().expect("a second handle");

    let mut child = Command::new(program)
        .args(parts)
        .arg(&elf)
        .stdout(Stdio::from(sink))
        .stderr(Stdio::from(errs))
        .spawn()
        .unwrap_or_else(|e| panic!("{program} did not start: {e}"));

    let start = Instant::now();
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break Some(status),
            None if start.elapsed() >= DEADLINE => {
                // A port that stops switching never reaches the exit.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    let text = std::fs::read_to_string(&log).unwrap_or_default();
    match status {
        Some(s) => (s.success() && text.contains("RESULT: PASS"), text),
        None => (
            false,
            format!("TIMED OUT after {DEADLINE:?} -- the cell never exited\n{text}"),
        ),
    }
}

fn require(name: &str) {
    if !enabled() {
        return;
    }
    let (ok, text) = cell(name);
    assert!(ok, "{name} did not pass:\n{}", tail(&text));
}

/// The last few lines, which is where a cell puts its verdict.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(14);
    lines[from..].join("\n")
}

/// The PendSV context switch on a Cortex-M3. Depends on the cortex-m port
/// and nothing else, so it is the one cell that can run while the kernel
/// repository is busy.
#[test]
fn cortex_m_switch_cell_passes() {
    require("mps2-an385-qemu-switch");
}

/// The SysTick-driven preemptive schedule.
#[test]
fn cortex_m_preempt_cell_passes() {
    require("mps2-an385-qemu-preempt");
}

/// Tickless idle on the real SysTick.
#[test]
fn cortex_m_tickless_cell_passes() {
    require("mps2-an385-qemu-tickless");
}

/// The kernel itself, on the cortex-m port.
///
/// IGNORED: this cell does not compile in this checkout. It is the only
/// one that names its siblings by GIT URL, so it resolves a second
/// `rusty_rtos_core` alongside the path-patched one the port uses, and
/// `CortexMPort` then implements a different `Port` from the one the
/// kernel expects. See `docs/HOLES.md`, H8 -- the fix is an owner's
/// choice between path dependencies and a git patch table.
#[test]
#[ignore = "does not compile in this checkout; see docs/HOLES.md H8"]
fn cortex_m_kernel_cell_passes() {
    require("mps2-an385-qemu-kernel");
}
