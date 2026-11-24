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

/// Builds one case of c-examples/io_cases.c and runs it with `env`,
/// returning its stdout.
fn run_case(case: &str, env: &[(&str, &str)]) -> String {
    run_case_with_stderr(case, env).0
}

/// Like [`run_case`], also returning stderr.
fn run_case_with_stderr(case: &str, env: &[(&str, &str)]) -> (String, String) {
    run_source_case("io_cases.c", case, env)
}

/// Builds one case of the C file `source` in c-examples and runs it with
/// `env`, returning its stdout and stderr.
fn run_source_case(source: &str, case: &str, env: &[(&str, &str)]) -> (String, String) {
    let root = repo_root();
    // Tests run in parallel, so a case run with a different env gets its own
    // binary.
    let suffix: String = env
        .iter()
        .map(|(key, value)| format!("-{key}={value}"))
        .collect();
    let binary = temp_dir().join(format!("{case}{suffix}"));
    let status = Command::new("clang")
        .args(["-g", "-O0", "-Wall", "-Wextra", "-Werror", "-DRSCHED"])
        .arg(format!("-D{case}"))
        .arg(format!("-I{}", root.join("include").display()))
        .arg("-o")
        .arg(&binary)
        .arg(root.join("c-examples").join(source))
        .arg("-Wl,--start-group")
        .arg(static_lib())
        .args(["-Wl,--end-group", "-lpthread", "-ldl", "-lm"])
        .status()
        .unwrap_or_else(|error| panic!("invoke clang for {source}: {error}"));
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
    (stdout, String::from_utf8_lossy(&output.stderr).into_owned())
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
fn sleep_in_one_process_expires_while_another_waits_on_io() {
    let stdout = run_case("CASE_IO_SLEEPING_WRITER_PROCESS", &[]);
    assert!(stdout.contains("traces=1: E"), "{stdout}");
}

#[test]
fn thread_exit_in_a_child_process_hands_the_turn_back() {
    let stdout = run_case("CASE_IO_THREAD_EXIT_IN_CHILD", &[]);
    assert!(stdout.contains("traces=1: ET"), "{stdout}");
}

#[test]
fn epoll_wait_blocks_the_task_instead_of_the_kernel() {
    let stdout = run_case("CASE_IO_EPOLL_THREADS", &[]);
    assert!(stdout.contains("traces=2"), "{stdout}");
}

#[test]
fn poll_timeout_waits_while_another_task_can_run() {
    let stdout = run_case("CASE_IO_TIMED_POLL_THREADS", &[]);
    assert!(stdout.contains("traces=1: R"), "{stdout}");
}

#[test]
fn signal_from_another_process_interrupts_a_blocked_read() {
    let stdout = run_case("CASE_IO_SIGNAL_INTERRUPTS_READ", &[]);
    assert!(stdout.contains("traces=1: S"), "{stdout}");
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

#[test]
fn blocked_waiters_are_not_runnable() {
    // Only the main thread and the helper yield, BLOCKED_YIELDS (6) times
    // each per schedule. A worker that busy-waited on the empty pipe would
    // add yields of its own while it waits.
    let (stdout, stderr) = run_case_with_stderr("CASE_IO_BLOCKED_WAITER", &[("RSCHED_LOG", "1")]);
    assert!(stdout.contains("traces=1: E"), "{stdout}");
    let runs: usize = stdout
        .strip_prefix("runs=")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|runs| runs.parse().ok())
        .unwrap_or_else(|| panic!("no run count in {stdout}"));
    let yields = stderr
        .lines()
        .filter(|line| line.contains("] SchedYield"))
        .count();
    assert_eq!(yields, runs * 2 * 6, "a blocked waiter yielded:\n{stdout}");
}

/// Runs one case of c-examples/io_edge_cases.c, which prints "ok".
fn assert_edge_case(case: &str) {
    assert_edge_case_with(case, &[]);
}

fn assert_edge_case_with(case: &str, env: &[(&str, &str)]) {
    let (stdout, _) = run_source_case("io_edge_cases.c", case, env);
    assert_eq!(stdout, "ok\n", "{case}");
}

#[test]
fn exit_does_not_wait_for_its_own_blocked_readers() {
    assert_edge_case("CASE_IO_EDGE_EXIT_WITH_BLOCKED_READER");
}

#[test]
fn poll_without_descriptors_blocks() {
    assert_edge_case("CASE_IO_EDGE_POLL_NO_FDS");
}

#[test]
fn poll_on_deeply_nested_epolls_completes() {
    assert_edge_case("CASE_IO_EDGE_NESTED_EPOLL");
}

#[test]
fn program_cannot_close_reserved_descriptors() {
    assert_edge_case("CASE_IO_EDGE_RESERVED_FDS_SURVIVE");
}

#[test]
fn seccomp_keeps_reserved_descriptors_from_raw_close() {
    assert_edge_case_with(
        "CASE_IO_EDGE_RESERVED_FDS_SURVIVE",
        &[("RSCHED_SECCOMP", "1")],
    );
}

#[test]
fn io_on_a_negative_descriptor_fails_with_ebadf() {
    assert_edge_case("CASE_IO_EDGE_BAD_FD");
}

#[test]
fn kernel_wait_releases_the_global_lock() {
    assert_edge_case("CASE_IO_EDGE_UNPARKED_THREAD_NEEDS_LOCK");
}
