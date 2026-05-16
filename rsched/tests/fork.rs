#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
static STATIC_LIB: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug)]
struct RunOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-fork-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fork test temp dir");
    dir
}

fn static_lib() -> PathBuf {
    STATIC_LIB.get_or_init(build_static_lib).clone()
}

fn build_static_lib() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join("target");
    let status = Command::new(cargo)
        .current_dir(repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "--lib"])
        .status()
        .expect("spawn cargo build for fork staticlib");
    assert!(status.success(), "cargo build --lib failed");

    let path = target_dir.join("debug").join("librsched.a");
    assert!(
        path.exists(),
        "expected rsched staticlib at {}",
        path.display()
    );
    path
}

fn build_example(name: &str) -> PathBuf {
    let root = repo_root();
    let out = temp_dir().join(name);
    let src = root.join("c-examples").join(format!("{name}.c"));
    let lib = static_lib();

    let status = Command::new("clang")
        .args(["-g", "-O0", "-Wall", "-Wextra", "-DRSCHED"])
        .arg(format!("-I{}", root.join("include").display()))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg("-L")
        .arg(lib.parent().expect("staticlib should have parent"))
        .args(["-Wl,--start-group"])
        .arg(&lib)
        .args(["-Wl,--end-group", "-lpthread", "-ldl", "-lm"])
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke clang for {}: {e}", src.display()));
    assert!(status.success(), "failed to build {}", src.display());
    out
}

fn run_with_seed(program: &Path, seed: u64) -> RunOutput {
    let out = Command::new("timeout")
        .arg("--kill-after=5s")
        .arg(format!("{}s", TIMEOUT.as_secs()))
        .arg(program)
        .env("RANDOM_SEED", seed.to_string())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn timeout for {}: {e}", program.display()));
    RunOutput {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        timed_out: out.status.code() == Some(124) || out.status.code() == Some(137),
    }
}

fn trace_from_output(seed: u64, output: RunOutput) -> String {
    assert!(
        !output.timed_out,
        "fork_counter seed {seed} timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.status.success(),
        "fork_counter seed {seed} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    let trace = output
        .stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("trace="))
        .unwrap_or_else(|| {
            panic!(
                "fork_counter seed {seed} did not print trace\nstdout:\n{}\nstderr:\n{}",
                output.stdout, output.stderr
            )
        });
    assert_eq!(
        trace.len(),
        6,
        "fork_counter seed {seed} produced wrong trace length: {trace:?}"
    );
    assert!(
        trace.chars().all(|c| c == 'P' || c == 'C'),
        "fork_counter seed {seed} produced invalid trace: {trace:?}"
    );
    trace.to_owned()
}

#[test]
fn fork_is_deterministic_per_seed() {
    let program = build_example("fork_counter");
    for seed in 0..10 {
        let a = trace_from_output(seed, run_with_seed(&program, seed));
        let b = trace_from_output(seed, run_with_seed(&program, seed));
        assert_eq!(a, b, "seed {seed}: first trace={a} second trace={b}");
    }
}

#[test]
fn fork_explores_multiple_process_interleavings() {
    let program = build_example("fork_counter");
    let mut traces = HashSet::new();
    for seed in 0..30 {
        traces.insert(trace_from_output(seed, run_with_seed(&program, seed)));
    }
    assert!(
        traces.len() > 3,
        "expected >3 distinct process traces across 30 seeds, got {}: {:?}",
        traces.len(),
        traces
    );
}

fn last_from_output(seed: u64, output: RunOutput) -> i32 {
    assert!(
        !output.timed_out,
        "fork_threads seed {seed} timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.status.success(),
        "fork_threads seed {seed} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    let last = output
        .stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("last="))
        .unwrap_or_else(|| {
            panic!(
                "fork_threads seed {seed} did not print last\nstdout:\n{}\nstderr:\n{}",
                output.stdout, output.stderr
            )
        });
    let last = last
        .parse::<i32>()
        .unwrap_or_else(|e| panic!("fork_threads seed {seed} bad last={last:?}: {e}"));
    assert!(
        (1..=4).contains(&last),
        "fork_threads seed {seed} produced invalid last={last}"
    );
    last
}

#[test]
fn fork_threads_is_deterministic_per_seed() {
    let program = build_example("fork_threads");
    for seed in 0..10 {
        let a = last_from_output(seed, run_with_seed(&program, seed));
        let b = last_from_output(seed, run_with_seed(&program, seed));
        assert_eq!(a, b, "seed {seed}: first last={a} second last={b}");
    }
}

#[test]
fn fork_threads_explores_each_final_writer() {
    let program = build_example("fork_threads");
    let mut final_writers = HashSet::new();
    for seed in 0..120 {
        final_writers.insert(last_from_output(seed, run_with_seed(&program, seed)));
    }
    let expected = HashSet::from([1, 2, 3, 4]);
    assert_eq!(
        final_writers, expected,
        "expected every task id to be final writer across seeds, got {final_writers:?}"
    );
}

fn exec_last_from_output(seed: u64, output: RunOutput) -> i32 {
    assert!(
        !output.timed_out,
        "fork_execv seed {seed} timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.status.success(),
        "fork_execv seed {seed} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    let last = output
        .stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("last="))
        .unwrap_or_else(|| {
            panic!(
                "fork_execv seed {seed} did not print last\nstdout:\n{}\nstderr:\n{}",
                output.stdout, output.stderr
            )
        });
    let last = last
        .parse::<i32>()
        .unwrap_or_else(|e| panic!("fork_execv seed {seed} bad last={last:?}: {e}"));
    assert!(
        last == 1 || last == 2,
        "fork_execv seed {seed} produced invalid last={last}"
    );
    last
}

#[test]
fn fork_execv_is_deterministic_per_seed() {
    let program = build_example("fork_execv");
    for seed in 0..10 {
        let a = exec_last_from_output(seed, run_with_seed(&program, seed));
        let b = exec_last_from_output(seed, run_with_seed(&program, seed));
        assert_eq!(a, b, "seed {seed}: first last={a} second last={b}");
    }
}

#[test]
fn fork_execv_explores_both_final_writers() {
    let program = build_example("fork_execv");
    let mut final_writers = HashSet::new();
    for seed in 0..60 {
        final_writers.insert(exec_last_from_output(seed, run_with_seed(&program, seed)));
    }
    let expected = HashSet::from([1, 2]);
    assert_eq!(
        final_writers, expected,
        "expected both parent and execed child as final writer across seeds, got {final_writers:?}"
    );
}
