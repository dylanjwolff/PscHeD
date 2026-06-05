#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
static STATIC_LIB: OnceLock<PathBuf> = OnceLock::new();
static BUILD_ID: AtomicUsize = AtomicUsize::new(0);

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
    let build_id = BUILD_ID.fetch_add(1, Ordering::Relaxed);
    let out = temp_dir().join(format!("{name}-{build_id}"));
    let src = root.join("c-examples").join("fork_cases.c");
    let lib = static_lib();

    let status = Command::new("clang")
        .args(["-g", "-O0", "-Wall", "-Wextra", "-DRSCHED"])
        .arg(format!("-D{}", fork_case_define(name)))
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

fn fork_case_define(name: &str) -> &'static str {
    match name {
        "fork_counter" => "CASE_FORK_COUNTER",
        "fork_threads" => "CASE_FORK_THREADS",
        "fork_execv" => "CASE_FORK_EXECV",
        "fork_dfs_count" => "CASE_FORK_DFS_COUNT",
        other => panic!("unknown fork example: {other}"),
    }
}

fn run_with_env(program: &Path, envs: &[(&str, &str)]) -> RunOutput {
    let mut cmd = Command::new("timeout");
    cmd.arg("--kill-after=5s")
        .arg(format!("{}s", TIMEOUT.as_secs()))
        .arg(program);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn timeout for {}: {e}", program.display()));
    RunOutput {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        timed_out: out.status.code() == Some(124) || out.status.code() == Some(137),
    }
}

#[test]
fn dfs_exhausts_fork_interleavings() {
    let program = build_example("fork_dfs_count");
    let output = run_with_env(&program, &[]);
    assert!(
        !output.timed_out,
        "fork_dfs_count timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.status.success(),
        "fork_dfs_count failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    assert!(
        output.stdout.contains("mask=0x3"),
        "fork_dfs_count did not exhaust all process interleavings\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
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
fn fork_execv_smoke_test() {
    let program = build_example("fork_execv");
    let _ = exec_last_from_output(0, run_with_env(&program, &[("RSCHED_SCHEDULER", "dfs")]));
}
