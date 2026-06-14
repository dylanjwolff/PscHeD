#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

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
    let dir = std::env::temp_dir().join(format!("rsched-seccomp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create seccomp test temp dir");
    dir
}

fn static_lib() -> PathBuf {
    STATIC_LIB.get_or_init(build_static_lib).clone()
}

fn build_static_lib() -> PathBuf {
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
        .expect("spawn cargo build for seccomp staticlib");
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
    let src = root.join("c-examples").join("seccomp_cases.c");
    let lib = static_lib();

    let status = Command::new("clang")
        .args(["-g", "-Wall", "-Wextra", "-DRSCHED"])
        .arg(format!("-D{}", seccomp_case_define(name)))
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

fn seccomp_case_define(name: &str) -> &'static str {
    match name {
        "seccomp_ok" => "CASE_SECCOMP_OK",
        "seccomp_raw_futex" => "CASE_SECCOMP_RAW_FUTEX",
        other => panic!("unknown seccomp example: {other}"),
    }
}

fn run_with_timeout(program: &Path, envs: &[(&str, &str)]) -> RunOutput {
    let mut cmd = Command::new("timeout");
    cmd.arg("--kill-after=5s")
        .arg(format!("{}s", TIMEOUT.as_secs()))
        .arg(program);
    cmd.env_remove("LD_LIBRARY_PATH");
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
fn seccomp_allows_rsched_internal_futexes() {
    let program = build_example("seccomp_ok");
    let output = run_with_timeout(&program, &[("RSCHED_SECCOMP", "1")]);
    assert!(
        !output.timed_out,
        "seccomp_ok timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.status.success(),
        "seccomp_ok failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    assert!(
        output.stdout.contains("counter = 1"),
        "seccomp_ok did not complete expected work\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
}

#[test]
fn seccomp_rejects_raw_futex_outside_rsched() {
    let program = build_example("seccomp_raw_futex");
    let output = run_with_timeout(&program, &[("RSCHED_SECCOMP", "1")]);
    assert!(
        !output.timed_out,
        "seccomp_raw_futex timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        !output.status.success(),
        "seccomp_raw_futex unexpectedly exited cleanly\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
    assert!(
        output
            .stderr
            .contains("rsched: intercepted raw futex/clone syscall outside rsched"),
        "seccomp_raw_futex did not print expected diagnostic\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
}
