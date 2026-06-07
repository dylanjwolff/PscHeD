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
    let dir = std::env::temp_dir().join(format!("rsched-task-creation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create task creation test temp dir");
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
        .expect("spawn cargo build for task creation staticlib");
    assert!(status.success(), "cargo build --lib failed");

    let path = target_dir.join("debug").join("librsched.a");
    assert!(
        path.exists(),
        "expected rsched staticlib at {}",
        path.display()
    );
    path
}

fn build_example(backend: &str) -> PathBuf {
    let root = repo_root();
    let build_id = BUILD_ID.fetch_add(1, Ordering::Relaxed);
    let out = temp_dir().join(format!("dfs-{backend}-{build_id}"));
    let src = root.join("c-examples").join("interleavings.c");
    let lib = static_lib();

    let status = Command::new("clang")
        .args([
            "-g",
            "-O0",
            "-Wall",
            "-Wextra",
            "-DRSCHED",
            "-DSTANDALONE_DFS_COUNT",
        ])
        .arg(format!("-DTASK_BACKEND_{}", backend.to_ascii_uppercase()))
        .arg(format!("-I{}", root.join("include").display()))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .args(["-Wl,--start-group"])
        .arg(&lib)
        .args(["-Wl,--end-group", "-lpthread", "-ldl", "-lm"])
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke clang for {}: {e}", src.display()));
    assert!(status.success(), "failed to build {}", src.display());
    out
}

fn run(program: &Path) -> RunOutput {
    let out = Command::new("timeout")
        .arg("--kill-after=5s")
        .arg(format!("{}s", TIMEOUT.as_secs()))
        .arg(program)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn timeout for {}: {e}", program.display()));
    RunOutput {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        timed_out: out.status.code() == Some(124) || out.status.code() == Some(137),
    }
}

fn assert_dfs_coverage(backend: &str, expected_mask: &str) {
    let output = run(&build_example(backend));
    assert!(
        !output.timed_out,
        "{backend} task creation timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.status.success(),
        "{backend} task creation failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    assert!(
        output.stdout.contains(expected_mask),
        "{backend} task creation did not exhaust expected interleavings\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
}

#[test]
fn dfs_exhausts_pthread_create_interleavings() {
    assert_dfs_coverage("pthread", "mask=0x3f");
}

#[test]
fn dfs_exhausts_clone_interleavings() {
    assert_dfs_coverage("clone", "mask=0x3");
}
