#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
static PRELOAD_LIB: OnceLock<PathBuf> = OnceLock::new();

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
    let dir = std::env::temp_dir().join(format!(
        "rsched-binary-instrumentation-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create binary instrumentation test temp dir");
    dir
}

fn preload_lib() -> PathBuf {
    PRELOAD_LIB.get_or_init(build_preload_cdylib).clone()
}

fn build_preload_cdylib() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join("target-preload");
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

fn run_timeout(program: &Path, args: &[OsString], envs: &[(&str, OsString)]) -> RunOutput {
    let mut cmd = Command::new("timeout");
    cmd.arg("--kill-after=5s")
        .arg(format!("{}s", TIMEOUT.as_secs()))
        .arg(program)
        .args(args);
    cmd.env_remove("LD_LIBRARY_PATH");
    for (key, value) in envs {
        cmd.env(key, value);
    }

    let out = cmd.output().unwrap_or_else(|e| {
        panic!(
            "failed to spawn timeout for {}: {e}",
            program.to_string_lossy()
        )
    });
    RunOutput {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        timed_out: out.status.code() == Some(124) || out.status.code() == Some(137),
    }
}

fn write_atomic_example() -> PathBuf {
    let src = temp_dir().join("binary_atomic.c");
    std::fs::write(
        &src,
        r#"
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>

static _Atomic int counter;

static void *worker(void *arg) {
    (void)arg;
    atomic_fetch_add_explicit(&counter, 1, memory_order_seq_cst);
    return NULL;
}

int main(void) {
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    atomic_fetch_add_explicit(&counter, 1, memory_order_seq_cst);
    pthread_join(t, NULL);
    printf("counter=%d\n", atomic_load_explicit(&counter, memory_order_seq_cst));
    return 0;
}
"#,
    )
    .expect("write binary atomic example");
    src
}

fn build_atomic_example() -> PathBuf {
    let src = write_atomic_example();
    let out = temp_dir().join("binary_atomic");
    let status = Command::new("clang")
        .args(["-g", "-O0", "-Wall", "-Wextra"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg("-lpthread")
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke clang for {}: {e}", src.display()));
    assert!(status.success(), "failed to build {}", src.display());
    out
}

#[test]
fn e9patch_binary_atomic_instrumentation_schedules_memops() {
    let root = repo_root();
    let bininst_dir = root.join("binary-instrumentation");
    let e9tool = bininst_dir.join("e9patch").join("e9tool");
    if !e9tool.exists() {
        eprintln!(
            "skipping binary instrumentation smoke test; {} is missing",
            e9tool.display()
        );
        return;
    }

    let original = build_atomic_example();
    let instrumented_dir = temp_dir().join("instrumented");
    let instrumented = instrumented_dir.join("binary_atomic.inst");
    let script = bininst_dir.join("instrument.sh");
    let output = Command::new("timeout")
        .arg("--kill-after=5s")
        .arg(format!("{}s", TIMEOUT.as_secs()))
        .arg("bash")
        .arg(&script)
        .arg(&original)
        .current_dir(&bininst_dir)
        .env("OUT_DIR", &instrumented_dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", script.display()));
    assert!(
        output.status.success(),
        "binary instrumentation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        instrumented.exists(),
        "expected instrumented binary at {}",
        instrumented.display()
    );
    let instrument_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        instrument_log.contains("num_patched           = 2 / 2"),
        "expected both lock-prefixed atomics to be patched\n{instrument_log}"
    );
    assert!(
        instrumented_dir.join("libc.so.6").exists(),
        "default recursive mode should instrument libc into {}\n{instrument_log}",
        instrumented_dir.display()
    );

    let preload = preload_lib();
    let run = run_timeout(
        &instrumented,
        &[],
        &[
            ("LD_PRELOAD", preload.as_os_str().to_owned()),
            ("LD_LIBRARY_PATH", instrumented_dir.as_os_str().to_owned()),
            ("RSCHED_LOG", OsString::from("1")),
        ],
    );
    assert!(
        !run.timed_out,
        "instrumented binary timed out\nstdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
    assert!(
        run.status.success(),
        "instrumented binary failed with status {}\nstdout:\n{}\nstderr:\n{}",
        run.status,
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("counter=2"),
        "instrumented binary produced unexpected output\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("[rsched] MemOp(RW"),
        "instrumented binary did not report rsched memop scheduling points\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
}
