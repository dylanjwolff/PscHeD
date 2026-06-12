use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

static INSTRUMENTED_GLIBC: OnceLock<InstrumentedGlibc> = OnceLock::new();

const GLIBC_TEST_CASES: &[&str] = &[
    "nptl/tst-basic1",
    "nptl/tst-mutex1",
    "nptl/tst-cond1",
    "nptl/tst-barrier1",
    "nptl/tst-tsd3",
    "nptl/tst-pt-tls1",
];

#[derive(Clone)]
struct InstrumentedGlibc {
    build_dir: PathBuf,
    plugin: PathBuf,
    rsched: PathBuf,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-glibc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create glibc test temp dir");
    dir
}

fn run(mut command: Command, description: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    if !output.status.success() {
        const MAX_FAILURE_LINES: usize = 200;

        fn tail(bytes: &[u8]) -> String {
            let text = String::from_utf8_lossy(bytes);
            let lines: Vec<_> = text.lines().collect();
            lines[lines.len().saturating_sub(MAX_FAILURE_LINES)..].join("\n")
        }

        panic!(
            "{description} failed with {}\nlast {MAX_FAILURE_LINES} stdout lines:\n{}\n\
             last {MAX_FAILURE_LINES} stderr lines:\n{}",
            output.status,
            tail(&output.stdout),
            tail(&output.stderr),
        );
    }
    output
}

fn append_env_flag(current: Option<OsString>, extra: &str) -> OsString {
    let mut value = current.unwrap_or_default();
    if !value.is_empty() {
        value.push(" ");
    }
    value.push(extra);
    value
}

fn require_program(name: &str) {
    let status = Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("{name} is required for the glibc tests: {error}"));
    assert!(status.success(), "{name} --version failed");
}

fn build_instrumented_glibc() -> InstrumentedGlibc {
    INSTRUMENTED_GLIBC
        .get_or_init(|| {
            let root = repo_root();
            let temp = temp_dir();
            let plugin_target = temp.join("llvm-pass-target");
            let rsched_target = temp.join("instrumented-rsched-target");
            let build_dir = temp.join("instrumented-glibc-build");
            let install_dir = temp.join("instrumented-glibc-install");
            std::fs::create_dir_all(&build_dir).expect("create instrumented glibc build dir");

            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
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
                .env(
                    "RUSTFLAGS",
                    append_env_flag(std::env::var_os("RUSTFLAGS"), "-C panic=abort"),
                )
                .args([
                    "build",
                    "-p",
                    "rsched",
                    "--features",
                    if cfg!(feature = "coro") {
                        "instrumented-libc,coro"
                    } else {
                        "instrumented-libc"
                    },
                ]);
            run(command, "build rsched for instrumented glibc");
            let rsched = rsched_target.join("debug/librsched.a");
            assert!(
                rsched.exists(),
                "expected rsched static library at {}",
                rsched.display()
            );

            let compiler_driver = root.join("glibc/instrument-clang.sh");
            let configure = root.join("glibc/glibc/configure");
            let mut command = Command::new(configure);
            command
                .current_dir(&build_dir)
                .env("RSCHED_LLVM_PLUGIN", &plugin)
                .env("CC", &compiler_driver)
                .arg(format!("--prefix={}", install_dir.display()))
                .arg("--disable-static")
                .arg("--disable-werror");
            run(command, "configure instrumented glibc");

            let jobs = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(2)
                .min(4)
                .to_string();
            let shared_gnulib = format!("{} -lgcc_s -lgcc", rsched.display());
            let mut command = Command::new("make");
            command
                .current_dir(&build_dir)
                .env("RSCHED_LLVM_PLUGIN", &plugin)
                .arg("--silent")
                .arg(format!("-j{jobs}"))
                .arg(format!("libc.so-gnulib={shared_gnulib}"))
                .arg(format!("gnulib={shared_gnulib}"))
                .arg(format!("gnulib-tests={shared_gnulib}"))
                .arg("static-gnulib=-lgcc -lgcc_eh")
                .arg("static-gnulib-tests=-lgcc -lgcc_eh")
                .arg("lib");
            run(command, "build instrumented glibc libraries");

            let support_archive = build_dir.join("support/libsupport_nonshared.a");
            let mut command = Command::new("make");
            command
                .current_dir(root.join("glibc/glibc/support"))
                .env("RSCHED_LLVM_PLUGIN", &plugin)
                .arg("--silent")
                .arg(format!("-j{jobs}"))
                .arg("subdir=support")
                .arg("..=../")
                .arg(format!("objdir={}", build_dir.display()))
                .arg(&support_archive);
            run(command, "build the glibc test support library");

            let mut command = Command::new("gcc");
            command.arg("--print-file-name=libgcc_s.so.1");
            let output = run(command, "locate libgcc_s.so.1");
            let libgcc = PathBuf::from(
                String::from_utf8(output.stdout)
                    .expect("gcc returned a non-UTF-8 libgcc path")
                    .trim(),
            );
            assert!(libgcc.is_file(), "gcc did not locate libgcc_s.so.1");
            std::fs::copy(&libgcc, build_dir.join("libgcc_s.so.1"))
                .expect("copy libgcc_s.so.1 into the glibc test library path");

            InstrumentedGlibc {
                build_dir,
                plugin,
                rsched,
            }
        })
        .clone()
}

#[test]
fn glibc_nptl_tests_with_instrumented_libc() {
    require_program("clang-17");
    require_program("gcc");
    require_program("make");
    require_program("objcopy");
    require_program("opt-17");

    let glibc = build_instrumented_glibc();
    let shared_gnulib = format!("{} -lgcc_s -lgcc", glibc.rsched.display());
    for test in GLIBC_TEST_CASES {
        let mut command = Command::new("make");
        command
            .current_dir(&glibc.build_dir)
            .env("RSCHED_LLVM_PLUGIN", &glibc.plugin)
            .arg("--silent")
            .arg(format!("libc.so-gnulib={shared_gnulib}"))
            .arg(format!("gnulib={shared_gnulib}"))
            .arg(format!("gnulib-tests={shared_gnulib}"))
            .arg("static-gnulib=-lgcc -lgcc_eh")
            .arg("static-gnulib-tests=-lgcc -lgcc_eh")
            .arg("test")
            .arg(format!("t={test}"));
        if cfg!(feature = "coro") {
            command.env("GLIBC_TUNABLES", "glibc.pthread.rseq=0");
        }
        run(command, &format!("run glibc test {test}"));

        let result = glibc.build_dir.join(format!("{test}.test-result"));
        let result = std::fs::read_to_string(&result)
            .unwrap_or_else(|error| panic!("read {}: {error}", result.display()));
        assert!(
            result.starts_with("PASS:"),
            "glibc test {test} failed:\n{result}"
        );
    }
}
