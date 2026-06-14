use rsched_libc_build::ArtifactManifest;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// Tests using native sem_wait cannot coordinate emulated pthreads until libc
// instrumentation also wraps POSIX semaphores.
const LIBC_TEST_CASES: &[(&str, &str)] = &[
    ("functional", "pthread_cond"),
    ("functional", "pthread_tsd"),
    ("functional", "tls_init"),
    ("regression", "pthread_condattr_setclock"),
    ("regression", "pthread_rwlock-ebusy"),
];

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

fn build_libc_test(suite: &Path, group: &str, name: &str) -> PathBuf {
    let output = temp_dir().join(format!("{group}-{name}"));
    let source = suite.join("src").join(group).join(format!("{name}.c"));
    let print = suite.join("src/common/print.c");
    assert!(
        source.exists(),
        "missing libc-test source {}",
        source.display()
    );

    let mut command = Command::new("musl-gcc");
    command
        .args([
            "-std=c99",
            "-D_POSIX_C_SOURCE=200809L",
            "-D_XOPEN_SOURCE=700",
            "-pthread",
        ])
        .arg(format!("-I{}", suite.join("src/common").display()))
        .arg("-o")
        .arg(&output)
        .arg(&source)
        .arg(&print);
    run(command, &format!("compile libc-test {group}/{name}"));
    output
}

#[test]
fn musl_libc_test_with_instrumented_libc() {
    let artifact = artifact();
    let runner = artifact.runner.as_ref().expect("musl artifact runner");
    let suite = repo_root().join("musl/libc-test");
    assert!(
        suite.join("src/common/test.h").exists(),
        "libc-test submodule is missing; run git submodule update --init --recursive"
    );

    for &(group, name) in LIBC_TEST_CASES {
        let binary = build_libc_test(&suite, group, name);
        let mut command = Command::new("timeout");
        command
            .args(["--signal=KILL", "20s"])
            .arg(runner)
            .arg(&binary);
        run(
            command,
            &format!("run libc-test {group}/{name} with instrumented musl preloaded"),
        );
    }
}
