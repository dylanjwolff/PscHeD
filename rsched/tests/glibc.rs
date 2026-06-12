use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

// These preserve the former preload crate's thread-provider coverage. The
// coroutine-specific instrumented-libc path is covered by the NPTL cases below.
#[cfg(not(feature = "coro"))]
#[path = "support/instrumented_glibc_preload.rs"]
mod instrumented_glibc_preload;

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
    #[cfg_attr(feature = "coro", allow(dead_code))]
    libc: PathBuf,
    #[cfg_attr(feature = "coro", allow(dead_code))]
    loader: PathBuf,
    #[cfg_attr(feature = "coro", allow(dead_code))]
    library_path: String,
    preload_runner: PathBuf,
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
            let mut rsched_features = vec!["instrumented-libc"];
            if cfg!(feature = "coro") {
                rsched_features.push("coro");
            }
            if cfg!(feature = "tsan") {
                rsched_features.push("tsan");
            }
            if cfg!(feature = "asan") {
                rsched_features.push("asan");
            }
            let rsched_features = rsched_features.join(",");
            command
                .current_dir(&root)
                .env("CARGO_TARGET_DIR", &rsched_target)
                .env(
                    "RUSTFLAGS",
                    append_env_flag(std::env::var_os("RUSTFLAGS"), "-C panic=abort"),
                )
                .args(["build", "-p", "rsched", "--features", &rsched_features]);
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
                // glibc requires optimization and must not inherit flags from
                // the project being tested (for example CFLAGS=-O0).
                .env("CFLAGS", "-g -O2")
                .env("CXXFLAGS", "-g -O2")
                .env_remove("CPPFLAGS")
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
                .arg("--silent")
                .arg(format!("-j{jobs}"))
                .arg(build_dir.join("versions.stmp"));
            run(command, "generate glibc symbol version maps");
            let libc_map = build_dir.join("libc.map");
            let map = std::fs::read_to_string(&libc_map).expect("read generated glibc libc.map");
            let marker = "  local:\n    *;\n};";
            let replacement = "    rsched_atomic_instrument;\n\
                               rsched_atomic_instrument_ra;\n\
                               rsched_fuzzer_test_one_input;\n\
                               local:\n    *;\n};";
            if !map.contains("rsched_atomic_instrument;") {
                let map = map
                    .rfind(marker)
                    .map(|index| {
                        let mut output = map.clone();
                        output.replace_range(index..index + marker.len(), replacement);
                        output
                    })
                    .expect("find final GLIBC_PRIVATE local block in libc.map");
                std::fs::write(&libc_map, map)
                    .expect("export rsched hooks from instrumented glibc");
            }

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

            let loader = build_dir.join("elf/ld-linux-x86-64.so.2");
            let libc = build_dir.join("libc.so");
            let preload_runner = temp.join("run-with-instrumented-glibc");
            let mut library_paths = [
                "", "math", "elf", "dlfcn", "nss", "nis", "rt", "resolv", "mathvec", "support",
                "crypt", "nptl",
            ]
            .into_iter()
            .map(|dir| build_dir.join(dir))
            .collect::<Vec<_>>();
            library_paths.extend([
                PathBuf::from("/lib/x86_64-linux-gnu"),
                PathBuf::from("/usr/lib/x86_64-linux-gnu"),
            ]);
            let library_path = library_paths
                .iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(":");
            std::fs::write(
                &preload_runner,
                format!(
                    "#!/bin/sh\n\
                     library_path='{library_path}'\n\
                     if [ -n \"${{RSCHED_EXTRA_LIBRARY_PATH:-}}\" ]; then\n\
                         library_path=\"$RSCHED_EXTRA_LIBRARY_PATH:$library_path\"\n\
                     fi\n\
                     preload='{libc}'\n\
                     if [ -n \"${{RSCHED_EXTRA_PRELOAD:-}}\" ]; then\n\
                         preload=\"$RSCHED_EXTRA_PRELOAD:$preload\"\n\
                     fi\n\
                     if [ -n \"${{RSCHED_DIRECT_EXEC:-}}\" ]; then\n\
                         GLIBC_TUNABLES='glibc.pthread.rseq=0' LD_PRELOAD=\"$preload\" \
                             LD_LIBRARY_PATH=\"$library_path\" exec \"$@\"\n\
                     fi\n\
                     GLIBC_TUNABLES='glibc.pthread.rseq=0' LD_PRELOAD=\"$preload\" \
                         exec '{loader}' --library-path \"$library_path\" \"$@\"\n",
                    libc = libc.display(),
                    loader = loader.display(),
                ),
            )
            .expect("write instrumented glibc preload runner");
            let mut permissions = std::fs::metadata(&preload_runner)
                .expect("stat instrumented glibc preload runner")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&preload_runner, permissions)
                .expect("make instrumented glibc preload runner executable");

            InstrumentedGlibc {
                build_dir,
                plugin,
                rsched,
                libc,
                loader,
                library_path,
                preload_runner,
            }
        })
        .clone()
}

fn build_preload_smoke(temp: &Path) -> PathBuf {
    let source = temp.join("preload-smoke.c");
    let binary = temp.join("preload-smoke");
    std::fs::write(
        &source,
        r#"
#include <pthread.h>

static void *worker(void *arg)
{
    return arg;
}

int main(void)
{
    pthread_t thread;
    if (pthread_create(&thread, 0, worker, 0) != 0)
        return 1;
    return pthread_join(thread, 0);
}
"#,
    )
    .expect("write glibc preload smoke test");
    let mut command = Command::new("gcc");
    command.args(["-pthread", "-o"]).arg(&binary).arg(&source);
    run(command, "build glibc preload smoke test");
    binary
}

#[test]
fn glibc_nptl_tests_with_instrumented_libc() {
    require_program("clang-17");
    require_program("gcc");
    require_program("make");
    require_program("objcopy");
    require_program("opt-17");

    let glibc = build_instrumented_glibc();
    let smoke = build_preload_smoke(
        glibc
            .preload_runner
            .parent()
            .expect("glibc preload runner parent"),
    );
    let mut command = Command::new(&glibc.preload_runner);
    command.arg(&smoke);
    run(
        command,
        "run ordinary pthread binary with instrumented glibc preloaded",
    );

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
