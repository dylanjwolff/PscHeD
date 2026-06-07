use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

static PRELOAD_LIB: OnceLock<PathBuf> = OnceLock::new();

// Tests that use native sem_wait to coordinate emulated pthreads cannot run
// until the preload layer also intercepts POSIX semaphores: a native sem_wait
// can block the kernel thread that rsched needs to run the semaphore's poster.
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
    let dir = std::env::temp_dir().join(format!("rsched-musl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create musl test temp dir");
    dir
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

fn build_musl_preload() -> PathBuf {
    PRELOAD_LIB
        .get_or_init(|| {
            let root = repo_root();
            let target_dir = temp_dir().join("preload-target");
            let linker = temp_dir().join("musl-linker");

            // Rust requests libgcc_s for cdylibs, but musl-gcc only provides
            // libgcc's unwinder as static archives. Substitute those archives
            // while preserving every other linker argument exactly.
            std::fs::write(
                &linker,
                r#"#!/usr/bin/env bash
set -eu
args=()
for arg in "$@"; do
    if [ "$arg" = "-lgcc_s" ]; then
        args+=(-lgcc_eh -lgcc)
    else
        args+=("$arg")
    fi
done
exec musl-gcc "${args[@]}"
"#,
            )
            .expect("write musl linker wrapper");
            let mut permissions = std::fs::metadata(&linker)
                .expect("stat musl linker wrapper")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&linker, permissions)
                .expect("make musl linker wrapper executable");

            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
            let mut command = Command::new(cargo);
            command
                .current_dir(&root)
                .env("CARGO_TARGET_DIR", &target_dir)
                .env("CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER", &linker)
                .env("CC_x86_64_unknown_linux_musl", "musl-gcc")
                .env(
                    "RUSTFLAGS",
                    append_env_flag(
                        std::env::var_os("RUSTFLAGS"),
                        "-C target-feature=-crt-static -C panic=abort",
                    ),
                )
                .args([
                    "build",
                    "-p",
                    "rsched-preload",
                    "--target",
                    "x86_64-unknown-linux-musl",
                ]);
            if cfg!(feature = "coro") {
                command.args(["--features", "coro"]);
            }
            run(command, "build the musl preload library");

            let path = target_dir
                .join("x86_64-unknown-linux-musl")
                .join("debug")
                .join("librsched_preload.so");
            assert!(
                path.exists(),
                "expected musl preload library at {}",
                path.display()
            );
            path
        })
        .clone()
}

fn append_env_flag(current: Option<OsString>, extra: &str) -> OsString {
    let mut value = current.unwrap_or_default();
    if !value.is_empty() {
        value.push(" ");
    }
    value.push(extra);
    value
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

fn run_preloaded(binary: &Path, preload: &Path, name: &str) {
    let mut command = Command::new("timeout");
    command
        .args(["--signal=KILL", "20s", "env"])
        .arg(preload_env(preload))
        .arg(binary);
    run(
        command,
        &format!("run libc-test {name} with rsched preloaded"),
    );
}

fn preload_env(preload: &Path) -> OsString {
    let mut value = OsString::from("LD_PRELOAD=");
    value.push(preload.as_os_str());
    value
}

fn require_program(name: &str) {
    let status = Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("{name} is required for the musl tests: {error}"));
    assert!(status.success(), "{name} --version failed");
}

#[test]
fn musl_libc_test_with_preload() {
    require_program("musl-gcc");
    require_program("timeout");

    let suite = repo_root().join("musl/libc-test");
    assert!(
        suite.join("src/common/test.h").exists(),
        "libc-test submodule is missing; run git submodule update --init --recursive"
    );

    let preload = build_musl_preload();
    for &(group, name) in LIBC_TEST_CASES {
        let binary = build_libc_test(&suite, group, name);
        run_preloaded(&binary, &preload, &format!("{group}/{name}"));
    }
}
