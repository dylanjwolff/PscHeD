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
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("preload crate should live under rsched/preload")
        .to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("rsched-preload-sanitizers-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create sanitizer test temp dir");
    dir
}

fn preload_lib() -> PathBuf {
    PRELOAD_LIB.get_or_init(build_preload_cdylib).clone()
}

fn build_preload_cdylib() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join(if cfg!(feature = "tsan") {
        "target-tsan"
    } else {
        "target-default"
    });
    let mut cmd = Command::new(cargo);
    cmd.current_dir(repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "-p", "rsched-preload"]);
    if cfg!(feature = "tsan") {
        cmd.args(["--features", "tsan"]);
    }

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

fn build_example(name: &str, sanitizer: &str) -> PathBuf {
    let root = repo_root();
    let out = temp_dir().join(format!(
        "{}-{}{}",
        name,
        sanitizer,
        std::env::consts::EXE_SUFFIX
    ));
    let src = root.join("c-examples").join(format!("{name}.c"));
    let lib_path = preload_lib();
    let lib_dir = lib_path
        .parent()
        .expect("preload library should have a parent directory");

    let mut compiler = cc::Build::new()
        .compiler("clang")
        .host(host_target())
        .target(host_target())
        .opt_level(0)
        .cargo_metadata(false)
        .get_compiler()
        .to_command();
    let status = compiler
        .arg(format!("-fsanitize={sanitizer}"))
        .args(["-g", "-Wall", "-Wextra"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg(format!("-L{}", lib_dir.display()))
        .arg("-lrsched_preload")
        .arg(format!("-Wl,-rpath,{}", lib_dir.display()))
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke clang for {}: {e}", src.display()));
    assert!(
        status.success(),
        "failed to build {} with -fsanitize={sanitizer}",
        src.display()
    );

    out
}

fn host_target() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        other => panic!("unsupported sanitizer test host: {other:?}"),
    }
}

fn run_with_timeout(
    program: &Path,
    args: &[&str],
    envs: &[(&str, OsString)],
    timeout: Duration,
) -> RunOutput {
    let mut cmd = Command::new("timeout");
    cmd.arg("--kill-after=5s")
        .arg(format!("{}s", timeout.as_secs()))
        .arg(program)
        .args(args);
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

#[cfg(not(feature = "tsan"))]
#[test]
fn asan_preload_compatibility() {
    let _ = preload_lib();
    let clean = build_example("asan_no_uaf", "address");
    let racy = build_example("asan_uaf", "address");

    let asan_options = OsString::from("halt_on_error=1:detect_leaks=0");
    assert_clean(
        "asan_no_uaf",
        run_with_timeout(
            &clean,
            &[],
            &[("ASAN_OPTIONS", asan_options.clone())],
            TIMEOUT,
        ),
    );
    assert_sanitizer_report(
        "asan_uaf",
        run_with_timeout(
            &racy,
            &[],
            &[
                ("ASAN_OPTIONS", asan_options),
                ("RANDOM_SEED", OsString::from("2")),
            ],
            TIMEOUT,
        ),
        "heap-use-after-free",
    );
}

#[cfg(not(feature = "tsan"))]
#[test]
fn ubsan_preload_compatibility() {
    let _ = preload_lib();
    let clean = build_example("ubsan_no_ub", "undefined");
    let racy = build_example("ubsan_ub", "undefined");

    let ubsan_options = OsString::from("halt_on_error=1");
    assert_clean(
        "ubsan_no_ub",
        run_with_timeout(
            &clean,
            &[],
            &[("UBSAN_OPTIONS", ubsan_options.clone())],
            TIMEOUT,
        ),
    );
    assert_sanitizer_report(
        "ubsan_ub",
        run_with_timeout(&racy, &[], &[("UBSAN_OPTIONS", ubsan_options)], TIMEOUT),
        "signed integer overflow",
    );
}

#[cfg(feature = "tsan")]
#[test]
fn tsan_preload_compatibility_all_background_modes() {
    let _ = preload_lib();
    let clean = build_example("tsan_no_race", "thread");
    let racy = build_example("tsan_race", "thread");

    for mode in [
        "first",
        "return-address",
        "start-arg",
        "first&return-address",
        "first&start-arg",
        "return-address&start-arg",
        "first&return-address&start-arg",
        "none",
    ] {
        let mode_value = OsString::from(mode);
        assert_clean(
            &format!("tsan_no_race ({mode})"),
            run_with_timeout(
                Path::new("setarch"),
                &["x86_64", "-R", clean.to_str().expect("utf-8 test path")],
                &[("RSCHED_TSAN_BACKGROUND_THREAD", mode_value.clone())],
                TIMEOUT,
            ),
        );
        assert_sanitizer_report(
            &format!("tsan_race ({mode})"),
            run_with_timeout(
                Path::new("setarch"),
                &["x86_64", "-R", racy.to_str().expect("utf-8 test path")],
                &[
                    ("RSCHED_TSAN_BACKGROUND_THREAD", mode_value),
                    ("TSAN_OPTIONS", OsString::from("halt_on_error=1")),
                ],
                TIMEOUT,
            ),
            "ThreadSanitizer: data race",
        );
    }
}
