use rsched_libc_build::ArtifactManifest;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GLIBC_TEST_CASES: &[&str] = &[
    "nptl/tst-basic1",
    "nptl/tst-mutex1",
    "nptl/tst-cond1",
    "nptl/tst-barrier1",
    "nptl/tst-tsd3",
    "nptl/tst-pt-tls1",
];

fn artifact() -> ArtifactManifest {
    let path = std::env::var_os("RSCHED_GLIBC_ARTIFACT").expect(
        "RSCHED_GLIBC_ARTIFACT is required; run `cargo xtask test libc glibc --provider native`",
    );
    ArtifactManifest::load(path).expect("load instrumented glibc artifact")
}

fn run(mut command: Command, description: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    if !output.status.success() {
        panic!(
            "{description} failed with {}\nlast stdout lines:\n{}\nlast stderr lines:\n{}",
            output.status,
            tail(&output.stdout),
            tail(&output.stderr),
        );
    }
    output
}

fn tail(bytes: &[u8]) -> String {
    const MAX_LINES: usize = 200;
    let text = String::from_utf8_lossy(bytes);
    let lines = text.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(MAX_LINES)..].join("\n")
}

#[test]
fn glibc_nptl_tests_with_instrumented_libc() {
    let artifact = artifact();
    let build_dir = artifact
        .build_dir
        .as_ref()
        .expect("glibc artifact build directory");
    let rsched = artifact
        .rsched_library
        .as_ref()
        .expect("glibc artifact rsched library");
    let shared_gnulib = format!("{} -lgcc_s -lgcc", rsched.display());

    for test in GLIBC_TEST_CASES {
        let mut command = Command::new("make");
        command
            .current_dir(build_dir)
            .env("RSCHED_LLVM_PLUGIN", &artifact.llvm_plugin)
            .arg("--silent")
            .arg(format!("libc.so-gnulib={shared_gnulib}"))
            .arg(format!("gnulib={shared_gnulib}"))
            .arg(format!("gnulib-tests={shared_gnulib}"))
            .arg("static-gnulib=-lgcc -lgcc_eh")
            .arg("static-gnulib-tests=-lgcc -lgcc_eh")
            .arg("test")
            .arg(format!("t={test}"));
        if artifact.provider == rsched_libc_build::Provider::Coro {
            command.env("GLIBC_TUNABLES", "glibc.pthread.rseq=0");
        }
        run(command, &format!("run glibc test {test}"));

        let result = build_dir.join(format!("{test}.test-result"));
        let result = std::fs::read_to_string(&result)
            .unwrap_or_else(|error| panic!("read {}: {error}", result.display()));
        assert!(
            result.starts_with("PASS:"),
            "glibc test {test} failed:\n{result}"
        );
    }
}

#[allow(dead_code)]
fn _assert_paths_are_paths(_: &Path, _: &PathBuf) {}
