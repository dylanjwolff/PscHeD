#![cfg(all(target_os = "linux", target_arch = "x86_64", not(feature = "tsan")))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static PRELOAD_LIB: OnceLock<PathBuf> = OnceLock::new();

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("preload crate should live under rsched/preload")
        .to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-preload-clone-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create preload clone temp dir");
    dir
}

fn preload_lib() -> PathBuf {
    PRELOAD_LIB.get_or_init(build_preload_cdylib).clone()
}

fn build_preload_cdylib() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join("target");
    let status = Command::new(cargo)
        .current_dir(repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "-p", "rsched-preload"])
        .status()
        .expect("spawn cargo build for preload cdylib");
    assert!(status.success(), "cargo build -p rsched-preload failed");

    let path = target_dir.join("debug").join("librsched_preload.so");
    assert!(
        path.exists(),
        "expected preload cdylib at {}",
        path.display()
    );
    path
}

fn build_subject() -> PathBuf {
    let root = repo_root();
    let src = root.join("c-examples").join("interleavings.c");
    let out = temp_dir().join("clone-smoke");
    let lib = preload_lib();
    let lib_dir = lib.parent().expect("preload library should have parent");

    let status = Command::new("clang")
        .args(["-g", "-O0", "-Wall", "-Wextra", "-DSTANDALONE_CLONE_SMOKE"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg(format!("-L{}", lib_dir.display()))
        .arg("-lrsched_preload")
        .arg(format!("-Wl,-rpath,{}", lib_dir.display()))
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke clang for {}: {e}", src.display()));
    assert!(status.success(), "failed to build {}", src.display());
    out
}

#[test]
fn preload_intercepts_direct_clone_syscall() {
    let subject = build_subject();
    let output = Command::new("timeout")
        .args(["--kill-after=5s", "30s"])
        .arg("env")
        .arg(format!("LD_PRELOAD={}", preload_lib().display()))
        .arg(&subject)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", subject.display()));
    assert!(
        output.status.success(),
        "clone smoke failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
