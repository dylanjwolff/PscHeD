#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

//! Semaphores, timed waits, rwlocks, pthread_once, and sleeps in
//! source-integration mode. Each case runs every DFS schedule of
//! c-examples/sync_cases.c and checks its invariant in each.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static STATIC_LIB: OnceLock<PathBuf> = OnceLock::new();

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-sync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create sync test temp dir");
    dir
}

fn static_lib() -> PathBuf {
    STATIC_LIB
        .get_or_init(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
            let target_dir = temp_dir().join("target");
            let mut command = Command::new(cargo);
            command
                .current_dir(repo_root())
                .env("CARGO_TARGET_DIR", &target_dir)
                .args(["build", "-p", "rsched", "--lib"]);
            if cfg!(feature = "coro") {
                command.args(["--features", "coro"]);
            }
            assert!(
                command.status().expect("spawn cargo").success(),
                "cargo build --lib failed"
            );
            target_dir.join("debug").join("librsched.a")
        })
        .clone()
}

/// Builds and runs one case; returns the number of DFS schedules it ran.
fn run_case(case: &str) -> usize {
    let root = repo_root();
    let binary = temp_dir().join(case);
    let status = Command::new("clang")
        .args(["-g", "-O0", "-Wall", "-Wextra", "-Werror", "-DRSCHED"])
        .arg(format!("-D{case}"))
        .arg(format!("-I{}", root.join("include").display()))
        .arg("-o")
        .arg(&binary)
        .arg(root.join("c-examples/sync_cases.c"))
        .arg("-Wl,--start-group")
        .arg(static_lib())
        .args(["-Wl,--end-group", "-lpthread", "-ldl", "-lm"])
        .status()
        .expect("invoke clang");
    assert!(status.success(), "failed to build {case}");
    let output = Command::new("timeout")
        .args(["--kill-after=5s", "60s"])
        .arg(&binary)
        .output()
        .expect("spawn sync case");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{case} failed with {} (124 means a schedule hung)\nstdout:\n{stdout}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    stdout
        .strip_prefix("runs=")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|runs| runs.parse().ok())
        .unwrap_or_else(|| panic!("no run count in {stdout}"))
}

#[test]
fn sem_wait_blocks_until_post() {
    assert!(run_case("CASE_SYNC_SEM") > 1);
}

#[test]
fn sem_timedwait_expires_after_its_deadline() {
    assert!(run_case("CASE_SYNC_SEM_TIMEDWAIT") >= 1);
}

#[test]
fn cond_timedwait_wakes_or_expires_after_its_deadline() {
    // Some schedules signal before the wait, others only after it expires.
    assert!(run_case("CASE_SYNC_COND_TIMEDWAIT") > 1);
}

#[test]
fn rwlock_writer_excludes_readers() {
    assert!(run_case("CASE_SYNC_RWLOCK") > 1);
}

#[test]
fn pthread_once_runs_init_once() {
    assert!(run_case("CASE_SYNC_ONCE") > 1);
}

#[test]
fn sleeping_task_lets_others_run() {
    assert!(run_case("CASE_SYNC_SLEEP") > 1);
}

#[test]
fn dfs_runs_past_its_former_depth_limit() {
    assert_eq!(run_case("CASE_SYNC_DEEP_DFS"), 1);
}
