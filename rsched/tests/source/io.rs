#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

//! File and socket message passing in source-integration mode. Each case runs
//! every DFS schedule of c-examples/io_cases.c; see that file for the traces.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static STATIC_LIB: OnceLock<PathBuf> = OnceLock::new();

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-io-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create io test temp dir");
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
            let status = command
                .status()
                .expect("spawn cargo build for io staticlib");
            assert!(status.success(), "cargo build --lib failed");
            target_dir.join("debug").join("librsched.a")
        })
        .clone()
}

/// Builds one case and runs it with `env`, returning its stdout.
fn run_case(case: &str, env: &[(&str, &str)]) -> String {
    let root = repo_root();
    let binary = temp_dir().join(case);
    let status = Command::new("clang")
        .args(["-g", "-O0", "-Wall", "-Wextra", "-Werror", "-DRSCHED"])
        .arg(format!("-D{case}"))
        .arg(format!("-I{}", root.join("include").display()))
        .arg("-o")
        .arg(&binary)
        .arg(root.join("c-examples/io_cases.c"))
        .arg("-Wl,--start-group")
        .arg(static_lib())
        .args(["-Wl,--end-group", "-lpthread", "-ldl", "-lm"])
        .status()
        .expect("invoke clang for io_cases.c");
    assert!(status.success(), "failed to build {case}");

    let output = Command::new("timeout")
        .args(["--kill-after=5s", "30s"])
        .arg(&binary)
        .envs(env.iter().copied())
        .output()
        .expect("spawn io case");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{case} failed with {} (124 means a schedule hung)\nstdout:\n{stdout}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    stdout
}

#[test]
fn pipe_ping_pong_between_threads() {
    let stdout = run_case("CASE_IO_PIPE_THREADS", &[]);
    assert!(stdout.contains("traces=2"), "{stdout}");
}

#[test]
fn socketpair_ping_pong_between_processes() {
    let stdout = run_case("CASE_IO_SOCKETPAIR_PROCESSES", &[]);
    assert!(stdout.contains("traces=2"), "{stdout}");
}

#[test]
fn tcp_loopback_poll_accept_connect() {
    let stdout = run_case("CASE_IO_TCP_THREADS", &[]);
    assert!(stdout.contains("traces=2"), "{stdout}");
}

#[test]
fn file_writes_are_scheduling_points() {
    let stdout = run_case("CASE_IO_FILE_THREADS", &[]);
    assert!(stdout.contains("traces=6"), "{stdout}");
    assert!(stdout.contains("ABAB"), "{stdout}");
}

#[test]
fn seccomp_routes_io_from_libc_internals() {
    // stdio's internal read and write bypass the rsched.h macros; the seccomp
    // filter traps them and routes them through the model.
    let stdout = run_case("CASE_IO_STDIO_PIPE_THREADS", &[("RSCHED_SECCOMP", "1")]);
    assert!(stdout.contains("traces=1"), "{stdout}");
}
