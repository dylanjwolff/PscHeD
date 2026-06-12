use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

static INSTRUMENTED_MUSL: OnceLock<InstrumentedMusl> = OnceLock::new();

#[derive(Clone)]
struct InstrumentedMusl {
    preload_runner: PathBuf,
}

// Tests that use native sem_wait to coordinate emulated pthreads cannot run
// until libc instrumentation also wraps POSIX semaphores: a native sem_wait can
// block the kernel thread that rsched needs to run the semaphore's poster.
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

fn build_instrumented_musl() -> InstrumentedMusl {
    INSTRUMENTED_MUSL
        .get_or_init(|| {
            let root = repo_root();
            let temp = temp_dir();
            let plugin_target = temp.join("llvm-pass-target");
            let rsched_target = temp.join("instrumented-rsched-target");
            let build_dir = temp.join("instrumented-musl-build");
            let install_dir = temp.join("instrumented-musl-install");
            std::fs::create_dir_all(&build_dir).expect("create instrumented musl build dir");

            let cargo =
                std::env::var_os("CARGO").unwrap_or_else(|| std::ffi::OsString::from("cargo"));
            let mut command = Command::new(&cargo);
            command
                .current_dir(&root)
                .env("CARGO_TARGET_DIR", &plugin_target)
                .args(["build", "-p", "rsched-llvm-pass"]);
            run(command, "build the rsched LLVM pass");
            let plugin = plugin_target.join("debug/librsched_llvm_pass.so");
            assert!(
                plugin.exists(),
                "expected rsched LLVM pass at {}",
                plugin.display()
            );

            let mut command = Command::new(&cargo);
            command
                .current_dir(&root)
                .env("CARGO_TARGET_DIR", &rsched_target)
                .env("CC_x86_64_unknown_linux_musl", "musl-gcc")
                .env(
                    "RUSTFLAGS",
                    append_env_flag(std::env::var_os("RUSTFLAGS"), "-C panic=abort"),
                )
                .args([
                    "build",
                    "-p",
                    "rsched",
                    "--target",
                    "x86_64-unknown-linux-musl",
                    "--features",
                    if cfg!(feature = "coro") {
                        "instrumented-libc,coro"
                    } else {
                        "instrumented-libc"
                    },
                ]);
            run(command, "build rsched for the instrumented musl");
            let rsched = rsched_target.join("x86_64-unknown-linux-musl/debug/librsched.a");

            let compiler_driver = root.join("musl/instrument-clang.sh");
            let configure = root.join("musl/musl/configure");
            let mut command = Command::new(configure);
            command
                .current_dir(&build_dir)
                .env("RSCHED_LLVM_PLUGIN", &plugin)
                .env(
                    "LDFLAGS",
                    "-Wl,--export-dynamic-symbol=rsched_atomic_instrument \
                     -Wl,--export-dynamic-symbol=rsched_atomic_instrument_ra \
                     -Wl,--export-dynamic-symbol=rsched_fuzzer_test_one_input",
                )
                .arg(format!("--prefix={}", install_dir.display()))
                .arg(format!("--syslibdir={}/lib", install_dir.display()))
                .arg("--disable-static")
                .env("CC", &compiler_driver);
            run(command, "configure instrumented musl");

            let libcc = format!("{} -lgcc -lgcc_eh", rsched.display());
            let jobs = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(2)
                .to_string();
            let mut command = Command::new("make");
            command
                .current_dir(&build_dir)
                .env("RSCHED_LLVM_PLUGIN", &plugin)
                .arg(format!("-j{jobs}"))
                .arg(format!("LIBCC={libcc}"));
            run(command, "build instrumented musl");

            let mut command = Command::new("make");
            command
                .current_dir(&build_dir)
                .env("RSCHED_LLVM_PLUGIN", &plugin)
                .arg("install");
            run(command, "install instrumented musl");

            let bin_dir = install_dir.join("bin");
            let compiler = bin_dir.join("musl-clang");
            assert!(
                compiler.exists(),
                "expected instrumented musl compiler at {}",
                compiler.display()
            );
            let libc = install_dir.join("lib/libc.so");
            let loader = install_dir.join("lib/ld-musl-x86_64.so.1");
            let preload_runner = temp.join("run-with-instrumented-musl");
            std::fs::write(
                &preload_runner,
                format!(
                    "#!/bin/sh\n\
                     LD_PRELOAD='{libc}' exec '{loader}' --library-path '{library_path}' \"$@\"\n",
                    libc = libc.display(),
                    loader = loader.display(),
                    library_path = install_dir.join("lib").display(),
                ),
            )
            .expect("write instrumented musl preload runner");
            let mut permissions = std::fs::metadata(&preload_runner)
                .expect("stat instrumented musl preload runner")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&preload_runner, permissions)
                .expect("make instrumented musl preload runner executable");
            InstrumentedMusl { preload_runner }
        })
        .clone()
}

fn append_env_flag(current: Option<std::ffi::OsString>, extra: &str) -> std::ffi::OsString {
    let mut value = current.unwrap_or_default();
    if !value.is_empty() {
        value.push(" ");
    }
    value.push(extra);
    value
}

fn build_libc_test(
    suite: &Path,
    group: &str,
    name: &str,
    compiler: &Path,
    variant: &str,
) -> PathBuf {
    let output = temp_dir().join(format!("{variant}-{group}-{name}"));
    let source = suite.join("src").join(group).join(format!("{name}.c"));
    let print = suite.join("src/common/print.c");
    assert!(
        source.exists(),
        "missing libc-test source {}",
        source.display()
    );

    let mut command = Command::new(compiler);
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
fn musl_libc_test_with_instrumented_libc() {
    require_program("clang-17");
    require_program("opt-17");
    require_program("make");
    require_program("musl-gcc");
    require_program("timeout");

    let suite = repo_root().join("musl/libc-test");
    assert!(
        suite.join("src/common/test.h").exists(),
        "libc-test submodule is missing; run git submodule update --init --recursive"
    );

    let musl = build_instrumented_musl();
    for &(group, name) in LIBC_TEST_CASES {
        let binary = build_libc_test(&suite, group, name, Path::new("musl-gcc"), "preloaded");
        let mut command = Command::new("timeout");
        command
            .args(["--signal=KILL", "20s"])
            .arg(&musl.preload_runner)
            .arg(&binary);
        run(
            command,
            &format!("run libc-test {group}/{name} with instrumented musl preloaded"),
        );
    }
}
