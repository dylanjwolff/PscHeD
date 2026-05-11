#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
static PRELOAD_LIB: OnceLock<PathBuf> = OnceLock::new();
static TSAN_PRELOAD_LIB: OnceLock<PathBuf> = OnceLock::new();

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
    PRELOAD_LIB
        .get_or_init(|| build_preload_cdylib("target-preload", &[]))
        .clone()
}

fn tsan_preload_lib() -> PathBuf {
    TSAN_PRELOAD_LIB
        .get_or_init(|| build_preload_cdylib("target-preload-tsan", &["--features", "tsan"]))
        .clone()
}

fn build_preload_cdylib(target_name: &str, extra_args: &[&str]) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join(target_name);
    let mut cmd = Command::new(cargo);
    cmd.current_dir(repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "-p", "rsched-preload"])
        .args(extra_args);
    let status = cmd.status().expect("spawn cargo build for preload cdylib");
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

fn write_sanitizer_example(name: &str, body: &str) -> PathBuf {
    let src = temp_dir().join(format!("{name}.c"));
    std::fs::write(&src, body).expect("write sanitizer binary instrumentation example");
    src
}

fn build_sanitizer_example(name: &str, source: &str, sanitizer: &str) -> PathBuf {
    let src = write_sanitizer_example(name, source);
    let out = temp_dir().join(name);
    let mut cmd = Command::new("clang");
    cmd.arg(format!("-fsanitize={sanitizer}"))
        .args(["-g", "-O0", "-Wall", "-Wextra"]);
    match sanitizer {
        "address" => {
            cmd.arg("-shared-libasan");
        }
        "thread" => {
            cmd.arg("-shared-libsan");
        }
        _ => {}
    }
    let status = cmd
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg("-lpthread")
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke clang for {}: {e}", src.display()));
    assert!(
        status.success(),
        "failed to build {} with -fsanitize={sanitizer}",
        src.display()
    );
    out
}

fn clang_runtime(name: &str) -> Option<PathBuf> {
    let output = Command::new("clang")
        .arg(format!("-print-file-name={name}"))
        .output()
        .expect("spawn clang to locate sanitizer runtime");
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

fn sanitizer_runtime(sanitizer: &str) -> Option<PathBuf> {
    match sanitizer {
        "asan" => clang_runtime("libclang_rt.asan-x86_64.so"),
        "tsan" => clang_runtime("libclang_rt.tsan-x86_64.so"),
        _ => None,
    }
}

fn preload_value(sanitizer: &str, preload: &Path) -> OsString {
    if sanitizer == "tsan" {
        return preload.as_os_str().to_owned();
    }
    let runtime = sanitizer_runtime(sanitizer);
    match runtime {
        Some(runtime) => OsString::from(format!("{}:{}", runtime.display(), preload.display())),
        None => preload.as_os_str().to_owned(),
    }
}

// Instruments `original` into a fresh `out_dir_name` subdirectory of temp_dir().
// Each caller should supply a unique `out_dir_name` so concurrent test threads
// don't share a working directory; instrument.sh compiles schedule_memops into
// the output directory rather than the shared e9patch/ tree.
fn instrument_binary(original: &Path, out_dir_name: &str) -> Option<(PathBuf, PathBuf, String)> {
    let root = repo_root();
    let bininst_dir = root.join("binary-instrumentation");
    let e9tool = bininst_dir.join("e9patch").join("e9tool");
    if !e9tool.exists() {
        eprintln!(
            "skipping binary instrumentation test; {} is missing",
            e9tool.display()
        );
        return None;
    }

    let instrumented_dir = temp_dir().join(out_dir_name);
    let instrumented = instrumented_dir.join(format!(
        "{}.inst",
        original
            .file_name()
            .expect("binary should have file name")
            .to_string_lossy()
    ));
    let script = bininst_dir.join("instrument.sh");
    let output = Command::new("timeout")
        .arg("--kill-after=5s")
        .arg(format!("{}s", TIMEOUT.as_secs()))
        .arg("bash")
        .arg(&script)
        .arg(original)
        .arg(&instrumented_dir)
        .current_dir(&bininst_dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", script.display()));
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "binary instrumentation failed\n{log}"
    );
    assert!(
        instrumented.exists(),
        "expected instrumented binary at {}\n{log}",
        instrumented.display()
    );
    Some((instrumented, instrumented_dir, log))
}

fn assert_clean(label: &str, output: RunOutput) {
    assert!(
        !output.timed_out,
        "{label} timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.status.success(),
        "{label} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
}

fn assert_sanitizer_report(label: &str, output: RunOutput, needle: &str) {
    assert!(
        !output.timed_out,
        "{label} timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        !output.status.success(),
        "{label} unexpectedly exited cleanly\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    assert!(
        combined.contains(needle),
        "{label} did not report {needle:?}\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
}

fn run_instrumented_sanitizer(
    label: &str,
    binary: &Path,
    sanitizer: &str,
    preload: &Path,
    extra_env: &[(&str, OsString)],
    expected_report: Option<&str>,
) {
    let Some((instrumented, instrumented_dir, _log)) =
        instrument_binary(binary, &format!("instrumented-{label}"))
    else {
        return;
    };
    let ld_library_path =
        match sanitizer_runtime(sanitizer).and_then(|p| p.parent().map(Path::to_path_buf)) {
            Some(runtime_dir) => OsString::from(format!(
                "{}:{}",
                instrumented_dir.display(),
                runtime_dir.display()
            )),
            None => instrumented_dir.as_os_str().to_owned(),
        };
    let mut envs = vec![
        ("LD_PRELOAD", preload_value(sanitizer, preload)),
        ("LD_LIBRARY_PATH", ld_library_path),
    ];
    envs.extend(extra_env.iter().map(|(k, v)| (*k, v.clone())));
    let run = if sanitizer == "tsan" {
        run_timeout(
            Path::new("setarch"),
            &[
                OsString::from("x86_64"),
                OsString::from("-R"),
                instrumented.as_os_str().to_owned(),
            ],
            &envs,
        )
    } else {
        run_timeout(&instrumented, &[], &envs)
    };
    match expected_report {
        Some(needle) => assert_sanitizer_report(&format!("{sanitizer} {label}"), run, needle),
        None => assert_clean(&format!("{sanitizer} {label}"), run),
    }
}

#[test]
fn e9patch_binary_atomic_instrumentation_schedules_memops() {
    let original = build_atomic_example();
    let Some((instrumented, instrumented_dir, instrument_log)) =
        instrument_binary(&original, "instrumented")
    else {
        return;
    };

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

fn sanitizer_subject(extra: &str) -> String {
    format!(
        r#"
#include <pthread.h>
#include <stdatomic.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>

static _Atomic int observed;
static int plain_counter;

static void touch_atomic(void) {{
    atomic_fetch_add_explicit(&observed, 1, memory_order_seq_cst);
}}

static void *race_worker(void *arg) {{
    (void)arg;
    touch_atomic();
    plain_counter++;
    return NULL;
}}

int main(void) {{
    touch_atomic();
    {extra}
    printf("observed=%d\n", atomic_load_explicit(&observed, memory_order_seq_cst));
    return 0;
}}
"#
    )
}

#[test]
fn binary_instrumentation_asan_compatibility() {
    let clean = sanitizer_subject("");
    let buggy =
        sanitizer_subject("int *p = malloc(sizeof(int)); *p = 7; free(p); plain_counter += *p;");
    let clean_bin = build_sanitizer_example("bininst_asan_clean", &clean, "address");
    let buggy_bin = build_sanitizer_example("bininst_asan_buggy", &buggy, "address");
    let preload = preload_lib();
    let asan_options = OsString::from("halt_on_error=1:detect_leaks=0");

    run_instrumented_sanitizer(
        "asan_clean",
        &clean_bin,
        "asan",
        &preload,
        &[("ASAN_OPTIONS", asan_options.clone())],
        None,
    );
    run_instrumented_sanitizer(
        "asan_buggy",
        &buggy_bin,
        "asan",
        &preload,
        &[("ASAN_OPTIONS", asan_options)],
        Some("heap-use-after-free"),
    );
}

#[test]
fn binary_instrumentation_ubsan_compatibility() {
    let clean = sanitizer_subject("");
    let buggy = sanitizer_subject("volatile int x = INT_MAX; plain_counter += x + 1;");
    let clean_bin = build_sanitizer_example("bininst_ubsan_clean", &clean, "undefined");
    let buggy_bin = build_sanitizer_example("bininst_ubsan_buggy", &buggy, "undefined");
    let preload = preload_lib();
    let ubsan_options = OsString::from("halt_on_error=1");

    run_instrumented_sanitizer(
        "ubsan_clean",
        &clean_bin,
        "ubsan",
        &preload,
        &[("UBSAN_OPTIONS", ubsan_options.clone())],
        None,
    );
    run_instrumented_sanitizer(
        "ubsan_buggy",
        &buggy_bin,
        "ubsan",
        &preload,
        &[("UBSAN_OPTIONS", ubsan_options)],
        Some("signed integer overflow"),
    );
}

#[test]
fn binary_instrumentation_tsan_compatibility() {
    let clean = sanitizer_subject("");
    let buggy = sanitizer_subject(
        "pthread_t t; pthread_create(&t, 0, race_worker, 0); plain_counter++; pthread_join(t, 0);",
    );
    let clean_bin = build_sanitizer_example("bininst_tsan_clean", &clean, "thread");
    let buggy_bin = build_sanitizer_example("bininst_tsan_buggy", &buggy, "thread");
    let preload = tsan_preload_lib();
    let tsan_options = OsString::from("halt_on_error=1");

    run_instrumented_sanitizer(
        "tsan_clean",
        &clean_bin,
        "tsan",
        &preload,
        &[("TSAN_OPTIONS", tsan_options.clone())],
        None,
    );
    run_instrumented_sanitizer(
        "tsan_buggy",
        &buggy_bin,
        "tsan",
        &preload,
        &[("TSAN_OPTIONS", tsan_options)],
        Some("ThreadSanitizer: data race"),
    );
}
