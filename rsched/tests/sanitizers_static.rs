use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
static DEFAULT_STATIC_LIB: OnceLock<PathBuf> = OnceLock::new();
static ASAN_STATIC_LIB: OnceLock<PathBuf> = OnceLock::new();

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
    let dir = std::env::temp_dir().join(format!("rsched-static-sanitizers-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create static sanitizer test temp dir");
    dir
}

fn default_static_lib() -> PathBuf {
    DEFAULT_STATIC_LIB
        .get_or_init(|| build_static_lib("target-default", &[]))
        .clone()
}

fn asan_static_lib() -> PathBuf {
    ASAN_STATIC_LIB
        .get_or_init(|| build_static_lib("target-asan", &["--features", "asan"]))
        .clone()
}

fn build_static_lib(target_name: &str, cargo_args: &[&str]) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join(target_name);
    let status = Command::new(cargo)
        .current_dir(repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .arg("build")
        .args(cargo_args)
        .status()
        .expect("spawn cargo build for rsched staticlib");
    assert!(status.success(), "cargo build for rsched staticlib failed");

    let path = target_dir.join("debug").join("librsched.a");
    assert!(
        path.exists(),
        "expected rsched staticlib at {}",
        path.display()
    );
    path
}

fn build_example(name: &str, sanitizer: &str, lib: &Path) -> PathBuf {
    let root = repo_root();
    let out = temp_dir().join(format!(
        "{name}-static-{sanitizer}{}",
        std::env::consts::EXE_SUFFIX
    ));
    let src = root.join("c-examples").join("sanitizer_cases.c");

    let status = Command::new("clang")
        .arg(format!("-fsanitize={sanitizer}"))
        .args(["-g", "-Wall", "-Wextra", "-DRSCHED"])
        .arg(format!("-D{}", sanitizer_case_define(name)))
        .arg(format!("-I{}", root.join("include").display()))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg("-L")
        .arg(lib.parent().expect("staticlib should have parent"))
        .args(["-Wl,--start-group"])
        .arg(lib)
        .args(["-Wl,--end-group", "-lpthread", "-ldl", "-lm"])
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke clang for {}: {e}", src.display()));
    assert!(
        status.success(),
        "failed to build {} with -fsanitize={sanitizer}",
        src.display()
    );

    out
}

fn sanitizer_case_define(name: &str) -> &'static str {
    match name {
        "asan_no_uaf" => "CASE_ASAN_CLEAN",
        "asan_uaf" => "CASE_ASAN_UAF",
        "ubsan_no_ub" => "CASE_UBSAN_CLEAN",
        "ubsan_ub" => "CASE_UBSAN_UB",
        other => panic!("unknown sanitizer example: {other}"),
    }
}

fn run_with_timeout(program: &Path, envs: &[(&str, OsString)]) -> RunOutput {
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

#[test]
fn asan_static_compatibility() {
    let lib = asan_static_lib();
    let clean = build_example("asan_no_uaf", "address", &lib);
    let racy = build_example("asan_uaf", "address", &lib);

    let asan_options = OsString::from("halt_on_error=1:detect_leaks=0");
    assert_clean(
        "asan_no_uaf static",
        run_with_timeout(&clean, &[("ASAN_OPTIONS", asan_options.clone())]),
    );
    assert_sanitizer_report(
        "asan_uaf static",
        run_with_timeout(
            &racy,
            &[
                ("ASAN_OPTIONS", asan_options),
                ("RSCHED_SCHEDULER", OsString::from("dfs")),
            ],
        ),
        "heap-use-after-free",
    );
}

#[test]
fn ubsan_static_compatibility() {
    let lib = default_static_lib();
    let clean = build_example("ubsan_no_ub", "undefined", &lib);
    let racy = build_example("ubsan_ub", "undefined", &lib);

    let ubsan_options = OsString::from("halt_on_error=1");
    assert_clean(
        "ubsan_no_ub static",
        run_with_timeout(&clean, &[("UBSAN_OPTIONS", ubsan_options.clone())]),
    );
    assert_sanitizer_report(
        "ubsan_ub static",
        run_with_timeout(&racy, &[("UBSAN_OPTIONS", ubsan_options)]),
        "signed integer overflow",
    );
}
