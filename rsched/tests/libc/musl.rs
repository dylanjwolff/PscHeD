use rsched_libc_build::ArtifactManifest;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-musl-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create musl test temp dir");
    dir
}

fn artifact() -> ArtifactManifest {
    let path = std::env::var_os("RSCHED_MUSL_ARTIFACT").expect(
        "RSCHED_MUSL_ARTIFACT is required; run `cargo xtask test libc musl --provider native`",
    );
    ArtifactManifest::load(path).expect("load instrumented musl artifact")
}

fn copy_libc_test_suite(source: &Path, destination: &Path) {
    let status = Command::new("cp")
        .args(["-a"])
        .arg(source)
        .arg(destination)
        .status()
        .expect("copy musl libc-test suite");
    assert!(status.success(), "copy musl libc-test suite failed");
}

fn run(mut command: Command, description: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    assert!(
        output.status.success(),
        "{description} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn write_musl_libc_test_config(suite: &Path) {
    std::fs::write(
        suite.join("config.mak"),
        "\
CFLAGS += -pipe -std=c99 -D_POSIX_C_SOURCE=200809L -Wall -Wno-unused-function -Wno-missing-braces -Wno-unused -Wno-overflow
CFLAGS += -Wno-unknown-pragmas -fno-builtin -frounding-math
CFLAGS += -Wno-strict-prototypes -Wno-switch-bool
CFLAGS += -Qunused-arguments
CFLAGS += -Werror=implicit-function-declaration -Werror=implicit-int -Werror=pointer-sign -Werror=pointer-arith
CFLAGS += -g
LDFLAGS += -g
LDLIBS += -lpthread -lm -lrt
",
    )
    .expect("write musl libc-test config");
}

fn write_no_preload_runner(path: &Path, artifact: &ArtifactManifest) {
    let loader = artifact.loader.as_ref().expect("musl artifact loader");
    let libc = artifact.libc.as_ref().expect("musl artifact libc");
    let library_path = libc.parent().expect("musl artifact libc parent");
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\nexec '{}' --library-path '{}' \"$@\"\n",
            loader.display(),
            library_path.display()
        ),
    )
    .expect("write musl libc-test no-preload runner");
    let mut permissions = std::fs::metadata(path)
        .expect("stat musl libc-test no-preload runner")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod musl libc-test no-preload runner");
}

fn is_expected_clang_libm_failure(line: &str) -> bool {
    // These are strict floating-point exception checks in libc-test that fail
    // for Clang-built musl libm even when the math objects are not passed
    // through rsched's LLVM instrumentation. The result values are correct;
    // only the reported fenv flags differ.
    line.contains("src/math/special/fmal.h:46: bad fp exception")
        || line.contains("src/math/ucb/powf.h:103: bad fp exception")
        || line.contains("/build/math/fmal.exe")
        || line.contains("/build/math/powf.exe")
}

fn normalize_musl_report(report: &str) -> String {
    report
        .lines()
        .filter(|line| !is_expected_clang_libm_failure(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_math_failure(report: &str) -> bool {
    report
        .lines()
        .any(|line| line.starts_with("FAIL ") && line.contains("/build/math/"))
}

#[test]
fn musl_libc_test_with_instrumented_libc() {
    let artifact = artifact();
    let compiler = artifact.compiler.as_ref().expect("musl artifact compiler");
    let source_suite = repo_root().join("musl/libc-test");
    assert!(
        source_suite.join("src/common/test.h").exists(),
        "libc-test submodule is missing; run git submodule update --init --recursive"
    );

    let work = temp_dir();
    let suite = work.join("libc-test");
    copy_libc_test_suite(&source_suite, &suite);
    write_musl_libc_test_config(&suite);

    let build = work.join("build");
    let runner = work.join("run-with-instrumented-musl");
    write_no_preload_runner(&runner, &artifact);
    let mut command = Command::new("make");
    command
        .current_dir(&suite)
        .env("CC", compiler)
        .env("RSCHED_LLVM_PLUGIN", &artifact.llvm_plugin)
        .arg(format!("B={}", build.display()))
        .arg(format!("RUN_WRAP={}", runner.display()))
        .arg("functional.BINS_TEMPL=bin.exe")
        .arg("regression.BINS_TEMPL=bin.exe")
        .arg("math.BINS_TEMPL=bin.exe")
        .arg("musl.BINS_TEMPL=bin.exe")
        .arg("run");
    let output = run(
        command,
        "run full musl libc-test suite with instrumented musl",
    );
    let report = build.join("REPORT");
    let report = std::fs::read(&report).expect("read musl libc-test report");
    let report = String::from_utf8_lossy(&report);
    let normalized_report = normalize_musl_report(&report);
    eprintln!("{normalized_report}");
    assert!(
        !normalized_report.contains("[timed out]"),
        "musl libc-test still has timed-out tests\nstdout:\n{}\nstderr:\n{}\nreport:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        normalized_report
    );
    assert!(
        !has_math_failure(&normalized_report),
        "musl libc-test has unexpected math failures\nreport:\n{}",
        normalized_report
    );
}
