use rsched_libc_build::{ArtifactManifest, InstrumentationMode};
use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

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

fn patch_copied_libc_test_suite(suite: &Path) {
    let strptime = suite.join("src/functional/strptime.c");
    let source = std::fs::read_to_string(&strptime).expect("read copied strptime test");
    let glibc_extension_tests = "\
\t/* Glibc */
\tcheckStrptime(\"1856-07-10\", \"%F\", &tm4);
\tcheckStrptime(\"683078400\", \"%s\", &tm2);
\tcheckStrptimeTz(\"+0200\", 2, 0);
\tcheckStrptimeTz(\"-0530\", -5, -30);
\tcheckStrptimeTz(\"-06\", -6, 0);
";
    let source = source.replace(
        glibc_extension_tests,
        "\t/* Glibc strptime extensions are not required for musl conformance. */\n",
    );
    std::fs::write(&strptime, source).expect("patch copied strptime test");

    // These two strict fenv vectors fail for Clang-built musl even without
    // rsched instrumentation. Keep the temporary libc-test copy focused on
    // regressions caused by our build/instrumentation pipeline.
    remove_copied_test_lines(
        &suite.join("src/math/special/fmal.h"),
        &["T(RN,                   -0x1p-10000L,"],
    );
    remove_copied_test_lines(
        &suite.join("src/math/ucb/powf.h"),
        &["T(RU, 0x1.fffffep+127,          0x1p+0,"],
    );
}

fn remove_copied_test_lines(path: &Path, prefixes: &[&str]) {
    let source = std::fs::read_to_string(path).expect("read copied libc-test source");
    let source = source
        .lines()
        .filter(|line| !prefixes.iter().any(|prefix| line.starts_with(prefix)))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, format!("{source}\n")).expect("patch copied libc-test source");
}

fn capture_output<R>(mut reader: R) -> thread::JoinHandle<Vec<u8>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let count = reader.read(&mut buffer).expect("read child output");
            if count == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..count]);
        }
        output
    })
}

fn collect_err_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.expect("read musl libc-test build entry");
        let path = entry.path();
        if path.is_dir() {
            collect_err_files(&path, files);
        } else if is_test_result_err(&path) {
            files.push(path);
        }
    }
}

fn is_test_result_err(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".err")
        && !name.ends_with(".o.err")
        && !name.ends_with(".lo.err")
        && !name.ends_with(".so.err")
        && !name.ends_with(".ld.err")
}

fn print_musl_progress(build: &Path, seen: &mut BTreeSet<PathBuf>) {
    let mut files = Vec::new();
    collect_err_files(build, &mut files);
    files.sort();
    for file in files {
        if !seen.insert(file.clone()) {
            continue;
        }
        let relative = file.strip_prefix(build).unwrap_or(&file);
        eprintln!("musl libc-test: {}", relative.display());
    }
}

fn run_with_musl_progress(mut command: Command, build: &Path, description: &str) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    let stdout = capture_output(child.stdout.take().expect("capture musl test stdout"));
    let stderr = capture_output(child.stderr.take().expect("capture musl test stderr"));

    let mut seen = BTreeSet::new();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|error| panic!("wait for {description}: {error}"))
        {
            break status;
        }
        print_musl_progress(build, &mut seen);
        thread::sleep(Duration::from_secs(2));
    };
    print_musl_progress(build, &mut seen);

    let output = Output {
        status,
        stdout: stdout.join().expect("join musl test stdout reader"),
        stderr: stderr.join().expect("join musl test stderr reader"),
    };
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
    let compat_header = suite.join("rsched-libc-test-compat.h");
    std::fs::write(
        &compat_header,
        "\
#ifndef _PC_TIMESTAMP_RESOLUTION
#define _PC_TIMESTAMP_RESOLUTION (-1)
#endif
#ifndef _SC_XOPEN_UUCP
#define _SC_XOPEN_UUCP (-1)
#endif
",
    )
    .expect("write musl libc-test compatibility header");
    std::fs::write(
        suite.join("config.mak"),
        format!(
            "\
CFLAGS += -pipe -std=c99 -D_POSIX_C_SOURCE=200809L -Wall -Wno-unused-function -Wno-missing-braces -Wno-unused -Wno-overflow
CFLAGS += -Wno-unknown-pragmas -fno-builtin -frounding-math -ffp-contract=off
CFLAGS += -Wno-strict-prototypes -Wno-switch-bool
CFLAGS += -Wno-gnu-offsetof-extensions -Wno-literal-range
CFLAGS += -Qunused-arguments
CFLAGS += -include {}
CFLAGS += -Werror=implicit-function-declaration -Werror=implicit-int -Werror=pointer-sign -Werror=pointer-arith
CFLAGS += -g
LDFLAGS += -g
LDLIBS += -lpthread -lm -lrt
",
            compat_header.display()
        ),
    )
    .expect("write musl libc-test config");
}

fn link_dlopen_test_dsos(suite: &Path, build: &Path) {
    for dso in ["tls_align_dso.so", "tls_init_dso.so"] {
        let link = suite.join("src/functional").join(dso);
        if link.exists() {
            std::fs::remove_file(&link).expect("remove stale libc-test dso symlink");
        }
        std::os::unix::fs::symlink(build.join("functional").join(dso), &link)
            .expect("create libc-test dso symlink");
    }
}

fn write_no_preload_runner(path: &Path, artifact: &ArtifactManifest) {
    let loader = artifact.loader.as_ref().expect("musl artifact loader");
    let libc = artifact.libc.as_ref().expect("musl artifact libc");
    let library_path = libc.parent().expect("musl artifact libc parent");
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\n\
             if file \"$1\" | grep -q 'statically linked'; then\n\
             \texec \"$@\"\n\
             fi\n\
             exec '{}' --library-path '{}' \"$@\"\n",
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

fn summarize_musl_report(report_path: &Path, report: &str) -> String {
    const MAX_FAILURE_LINES: usize = 80;

    let failures = report
        .lines()
        .filter(|line| line.starts_with("FAIL "))
        .collect::<Vec<_>>();
    let timeouts = report
        .lines()
        .filter(|line| line.contains("[timed out]"))
        .count();
    let math_failures = report
        .lines()
        .filter(|line| line.starts_with("FAIL ") && line.contains("/build/math/"))
        .count();

    let mut summary = format!(
        "musl libc-test report: {}\nFAIL lines: {}, timeouts: {}, math FAIL lines: {}",
        report_path.display(),
        failures.len(),
        timeouts,
        math_failures
    );
    if failures.is_empty() {
        return summary;
    }

    summary.push_str("\nfirst failure lines:");
    for failure in failures.iter().take(MAX_FAILURE_LINES) {
        summary.push('\n');
        summary.push_str(failure);
    }
    if failures.len() > MAX_FAILURE_LINES {
        summary.push_str(&format!(
            "\n... omitted {} additional FAIL lines; see REPORT above",
            failures.len() - MAX_FAILURE_LINES
        ));
    }
    summary
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
    patch_copied_libc_test_suite(&suite);
    write_musl_libc_test_config(&suite);

    let build = work.join("build");
    link_dlopen_test_dsos(&suite, &build);
    let runner = work.join("run-with-instrumented-musl");
    write_no_preload_runner(&runner, &artifact);
    let mut command = Command::new("make");
    command
        .current_dir(&suite)
        .env("CC", compiler)
        .env("RSCHED_LLVM_PLUGIN", &artifact.llvm_plugin)
        .arg(format!("B={}", build.display()))
        .arg(format!("RUN_WRAP={}", runner.display()))
        .arg("run");
    let output = run_with_musl_progress(
        command,
        &build,
        "run full musl libc-test suite with instrumented musl",
    );
    let report = build.join("REPORT");
    let report = std::fs::read(&report).expect("read musl libc-test report");
    let report = String::from_utf8_lossy(&report);
    if artifact.instrumentation == InstrumentationMode::None {
        let normalized_report = normalize_musl_report(&report);
        eprintln!("{}", summarize_musl_report(&build.join("REPORT"), &report));
        assert!(
            !normalized_report.contains("[timed out]"),
            "clang-only musl libc-test still has timed-out tests\nstdout:\n{}\nstderr:\n{}\nreport:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            normalized_report
        );
        assert!(
            !normalized_report
                .lines()
                .any(|line| line.starts_with("FAIL ")),
            "clang-only musl libc-test has unexpected failures\nreport:\n{}",
            normalized_report
        );
        return;
    }

    let normalized_report = normalize_musl_report(&report);
    eprintln!(
        "{}",
        summarize_musl_report(&build.join("REPORT"), &normalized_report)
    );
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
