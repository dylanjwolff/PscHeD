use super::{build_instrumented_glibc, repo_root, temp_dir};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

struct RunOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_preloaded(binary: &Path, args: &[&str], envs: &[(&str, OsString)]) -> RunOutput {
    let glibc = build_instrumented_glibc();
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "30s"])
        .arg(&glibc.preload_runner)
        .arg(binary)
        .args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("run {}: {error}", binary.display()));
    RunOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[cfg(feature = "tsan")]
fn run_preloaded_tsan(binary: &Path, envs: &[(&str, OsString)]) -> RunOutput {
    let glibc = build_instrumented_glibc();
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "30s", "setarch", "x86_64", "-R"])
        .arg(&glibc.preload_runner)
        .arg(binary);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("run TSAN {}: {error}", binary.display()));
    RunOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn assert_clean(label: &str, output: RunOutput) {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr,
    );
}

fn assert_report(label: &str, output: RunOutput, needle: &str) {
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr,
    );
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    assert!(
        combined.contains(needle),
        "{label} did not report {needle:?}\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr,
    );
}

fn build_c_case(source: &Path, name: &str, defines: &[&str], args: &[&str]) -> PathBuf {
    build_c_case_with_glibc(source, name, defines, args, None)
}

fn build_binary_instrumentation_case(
    source: &Path,
    name: &str,
    defines: &[&str],
    args: &[&str],
) -> PathBuf {
    let glibc = build_instrumented_glibc();
    build_c_case_with_glibc(source, name, defines, args, Some(&glibc))
}

fn build_c_case_with_glibc(
    source: &Path,
    name: &str,
    defines: &[&str],
    args: &[&str],
    glibc: Option<&super::InstrumentedGlibc>,
) -> PathBuf {
    let output = temp_dir().join(name);
    let mut command = Command::new("clang");
    command.args(["-g", "-O0", "-Wall", "-Wextra"]);
    for define in defines {
        command.arg(format!("-D{define}"));
    }
    command
        .args(args)
        .arg("-o")
        .arg(&output)
        .arg(source)
        .arg("-pthread");
    if let Some(glibc) = glibc {
        command
            .arg(format!("-Wl,--dynamic-linker={}", glibc.loader.display()))
            .arg(format!("-Wl,-rpath,{}", glibc.library_path));
    }
    let result = command
        .output()
        .unwrap_or_else(|error| panic!("compile {}: {error}", source.display()));
    assert!(
        result.status.success(),
        "compile {} failed\nstdout:\n{}\nstderr:\n{}",
        source.display(),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
    output
}

fn instrument_binary(original: &Path, name: &str) -> Option<(PathBuf, PathBuf, String)> {
    let root = repo_root();
    let instrumentation = root.join("binary-instrumentation");
    let e9tool = instrumentation.join("e9patch/e9tool");
    if !e9tool.exists() {
        eprintln!(
            "skipping binary instrumentation test; {} is missing",
            e9tool.display()
        );
        return None;
    }
    let output_dir = temp_dir().join(name);
    let instrumented = output_dir.join(format!(
        "{}.inst",
        original
            .file_name()
            .expect("binary file name")
            .to_string_lossy()
    ));
    let output = Command::new("timeout")
        .args(["--kill-after=5s", "30s", "bash"])
        .arg(instrumentation.join("instrument.sh"))
        .arg(original)
        .arg(&output_dir)
        .env("INSTRUMENT_LIBS", "0")
        .current_dir(&instrumentation)
        .output()
        .expect("run binary instrumentation");
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "binary instrumentation failed\n{log}"
    );
    assert!(
        instrumented.exists(),
        "missing instrumented binary {}\n{log}",
        instrumented.display(),
    );
    Some((instrumented, output_dir, log))
}

fn run_binary_instrumented(label: &str, binary: &Path, envs: &[(&str, OsString)]) -> RunOutput {
    let (instrumented, _output_dir, _) =
        instrument_binary(binary, label).expect("binary instrumentation was checked");
    let mut combined = Vec::new();
    combined.extend(envs.iter().cloned());
    combined.push(("RSCHED_DIRECT_EXEC", OsString::from("1")));
    run_preloaded(&instrumented, &[], &combined)
}

fn build_llvm_instrumented_case(source: &str, name: &str, sanitizer: &str) -> PathBuf {
    let glibc = build_instrumented_glibc();
    let c_path = temp_dir().join(format!("{name}.c"));
    let input = temp_dir().join(format!("{name}.ll"));
    let transformed = temp_dir().join(format!("{name}.instrumented.ll"));
    let binary = temp_dir().join(name);
    std::fs::write(&c_path, source).expect("write LLVM pass sanitizer subject");

    let output = Command::new("clang-17")
        .args(["-S", "-emit-llvm", "-g0", "-O0"])
        .arg(format!("-fsanitize={sanitizer}"))
        .arg("-o")
        .arg(&input)
        .arg(&c_path)
        .output()
        .expect("compile LLVM pass sanitizer subject");
    assert!(
        output.status.success(),
        "compile LLVM IR failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let output = Command::new("opt-17")
        .arg("-load-pass-plugin")
        .arg(&glibc.plugin)
        .arg("-passes=rsched-atomics")
        .arg("-S")
        .arg(&input)
        .arg("-o")
        .arg(&transformed)
        .output()
        .expect("instrument sanitizer LLVM IR");
    assert!(
        output.status.success(),
        "instrument LLVM IR failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let output = Command::new("clang-17")
        .arg(format!("-fsanitize={sanitizer}"))
        .args(["-static-libsan", "-static-libgcc"])
        .args(["-g", "-O0", "-o"])
        .arg(&binary)
        .arg(&transformed)
        .arg(glibc.build_dir.join("libc.so"))
        .arg("-pthread")
        .output()
        .expect("link LLVM pass sanitizer subject");
    assert!(
        output.status.success(),
        "link LLVM pass subject failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

fn llvm_sanitizer_subject(extra: &str) -> String {
    format!(
        r#"
#include <pthread.h>
#include <stdatomic.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>

static _Atomic int observed;
static int plain_counter;

static void *race_worker(void *arg)
{{
    (void)arg;
    atomic_fetch_add_explicit(&observed, 1, memory_order_seq_cst);
    plain_counter++;
    return 0;
}}

int main(void)
{{
    atomic_fetch_add_explicit(&observed, 1, memory_order_seq_cst);
    {extra}
    printf("observed=%d\n", atomic_load_explicit(&observed, memory_order_seq_cst));
    return 0;
}}
"#
    )
}

#[test]
fn instrumented_glibc_preload_asan_compatibility() {
    let source = repo_root().join("c-examples/sanitizer_cases.c");
    let clean = build_c_case(
        &source,
        "glibc-preload-asan-clean",
        &["CASE_ASAN_CLEAN"],
        &["-fsanitize=address", "-static-libsan", "-static-libgcc"],
    );
    let buggy = build_c_case(
        &source,
        "glibc-preload-asan-buggy",
        &["CASE_ASAN_UAF"],
        &["-fsanitize=address", "-static-libsan", "-static-libgcc"],
    );
    let options = OsString::from("halt_on_error=1:detect_leaks=0");
    assert_clean(
        "instrumented glibc preload ASAN clean",
        run_preloaded(&clean, &[], &[("ASAN_OPTIONS", options.clone())]),
    );
    assert_report(
        "instrumented glibc preload ASAN buggy",
        run_preloaded(
            &buggy,
            &[],
            &[
                ("ASAN_OPTIONS", options),
                ("RSCHED_SCHEDULER", OsString::from("dfs")),
            ],
        ),
        "heap-use-after-free",
    );
}

#[test]
fn instrumented_glibc_preload_ubsan_compatibility() {
    let source = repo_root().join("c-examples/sanitizer_cases.c");
    let clean = build_c_case(
        &source,
        "glibc-preload-ubsan-clean",
        &["CASE_UBSAN_CLEAN"],
        &["-fsanitize=undefined", "-static-libsan", "-static-libgcc"],
    );
    let buggy = build_c_case(
        &source,
        "glibc-preload-ubsan-buggy",
        &["CASE_UBSAN_UB"],
        &["-fsanitize=undefined", "-static-libsan", "-static-libgcc"],
    );
    let options = OsString::from("halt_on_error=1");
    assert_clean(
        "instrumented glibc preload UBSAN clean",
        run_preloaded(&clean, &[], &[("UBSAN_OPTIONS", options.clone())]),
    );
    assert_report(
        "instrumented glibc preload UBSAN buggy",
        run_preloaded(&buggy, &[], &[("UBSAN_OPTIONS", options)]),
        "signed integer overflow",
    );
}

#[cfg(feature = "tsan")]
#[test]
fn instrumented_glibc_preload_tsan_compatibility() {
    let source = repo_root().join("c-examples/sanitizer_cases.c");
    let clean = build_c_case(
        &source,
        "glibc-preload-tsan-clean",
        &["CASE_TSAN_CLEAN"],
        &["-fsanitize=thread", "-shared-libsan"],
    );
    let buggy = build_c_case(
        &source,
        "glibc-preload-tsan-buggy",
        &["CASE_TSAN_RACE"],
        &["-fsanitize=thread", "-shared-libsan"],
    );
    let options = OsString::from("halt_on_error=1");
    assert_clean(
        "instrumented glibc preload TSAN clean",
        run_preloaded_tsan(&clean, &[("TSAN_OPTIONS", options.clone())]),
    );
    assert_report(
        "instrumented glibc preload TSAN buggy",
        run_preloaded_tsan(&buggy, &[("TSAN_OPTIONS", options)]),
        "ThreadSanitizer: data race",
    );
}

#[test]
fn instrumented_glibc_preload_intercepts_direct_clone_syscall() {
    let source = temp_dir().join("glibc-preload-clone-smoke.c");
    std::fs::write(
        &source,
        r#"
#define _GNU_SOURCE
#include <errno.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void)
{
    errno = 0;
    pid_t unsupported = syscall(SYS_clone, CLONE_VM | SIGCHLD, 0, 0, 0, 0);
    if (unsupported != -1 || errno != ENOTSUP) {
        fprintf(stderr, "thread-style clone should fail with ENOTSUP\n");
        return 1;
    }

    pid_t child = syscall(SYS_clone, SIGCHLD, 0, 0, 0, 0);
    if (child < 0)
        return 2;
    if (child == 0) {
        sched_yield();
        _exit(0);
    }

    sched_yield();
    int status = 0;
    if (waitpid(child, &status, 0) != child)
        return 2;
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
}
"#,
    )
    .expect("write direct clone subject");
    let subject = build_c_case(&source, "glibc-preload-clone-smoke", &[], &[]);
    assert_clean(
        "instrumented glibc preload clone smoke",
        run_preloaded(&subject, &[], &[]),
    );
}

#[test]
fn instrumented_glibc_preload_bash_client_server() {
    let source = repo_root().join("c-examples/preload_fifo_cases.c");
    let server = build_c_case(
        &source,
        "glibc-preload-fifo-server",
        &["ROLE_FIFO_SERVER"],
        &[],
    );
    let client = build_c_case(
        &source,
        "glibc-preload-fifo-client",
        &["ROLE_FIFO_CLIENT"],
        &[],
    );
    let glibc = build_instrumented_glibc();
    let ipc = temp_dir().join("glibc-preload-fifo");
    let output_file = temp_dir().join("glibc-preload-fifo.out");
    let script = repo_root().join("tests/support/run_client_server.sh");
    let output = Command::new("timeout")
        .args(["--kill-after=5s", "30s", "bash"])
        .arg(&script)
        .arg(&glibc.preload_runner)
        .arg(&server)
        .arg(&client)
        .arg(&ipc)
        .arg(&output_file)
        .env("RSCHED_SCHEDULER", "dfs")
        .output()
        .expect("run instrumented glibc client/server test");
    assert!(
        output.status.success(),
        "instrumented glibc client/server failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("seen=A") || stdout.contains("seen=B") || stdout.contains("seen=NONE"),
        "client/server produced unexpected output: {stdout}",
    );
}

#[test]
fn instrumented_glibc_preload_binary_atomics() {
    let source = temp_dir().join("binary-atomic.c");
    std::fs::write(
        &source,
        r#"
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>

static _Atomic int counter;

static void *worker(void *arg)
{
    (void)arg;
    atomic_fetch_add_explicit(&counter, 1, memory_order_seq_cst);
    return 0;
}

int main(void)
{
    pthread_t thread;
    pthread_create(&thread, 0, worker, 0);
    atomic_fetch_add_explicit(&counter, 1, memory_order_seq_cst);
    pthread_join(thread, 0);
    printf("counter=%d\n", atomic_load_explicit(&counter, memory_order_seq_cst));
    return 0;
}
"#,
    )
    .expect("write binary atomic subject");
    let binary =
        build_binary_instrumentation_case(&source, "glibc-preload-binary-atomic", &[], &[]);
    let Some((instrumented, output_dir, log)) =
        instrument_binary(&binary, "glibc-preload-binary-atomic-instrumented")
    else {
        return;
    };
    assert!(
        log.contains("num_patched           = 2 / 2"),
        "expected both atomic operations to be patched\n{log}"
    );
    let output = run_preloaded(
        &instrumented,
        &[],
        &[
            (
                "RSCHED_EXTRA_LIBRARY_PATH",
                output_dir.as_os_str().to_owned(),
            ),
            ("RSCHED_DIRECT_EXEC", OsString::from("1")),
            ("RSCHED_LOG", OsString::from("1")),
        ],
    );
    assert!(
        output.stdout.contains("counter=2"),
        "instrumented atomic subject produced unexpected stdout:\n{}",
        output.stdout,
    );
    assert!(
        output.stderr.contains("[rsched] MemOp(RW"),
        "instrumented atomic subject did not report scheduling points:\n{}",
        output.stderr,
    );
    assert_clean("instrumented glibc preload binary atomics", output);
}

#[test]
fn instrumented_glibc_preload_binary_sanitizers() {
    if !repo_root()
        .join("binary-instrumentation/e9patch/e9tool")
        .exists()
    {
        eprintln!("skipping binary sanitizer tests; e9tool is missing");
        return;
    }

    let source = repo_root().join("c-examples/sanitizer_cases.c");
    let asan_runtime = build_instrumented_glibc().asan_runtime;
    let asan_preload = asan_runtime.as_os_str().to_owned();
    let asan_clean = build_binary_instrumentation_case(
        &source,
        "glibc-preload-bininst-asan-clean",
        &["CASE_ASAN_CLEAN"],
        &["-fsanitize=address", "-shared-libasan"],
    );
    let asan_buggy = build_binary_instrumentation_case(
        &source,
        "glibc-preload-bininst-asan-buggy",
        &["CASE_ASAN_UAF"],
        &["-fsanitize=address", "-shared-libasan"],
    );
    let asan_options = OsString::from("halt_on_error=1:detect_leaks=0");
    assert_clean(
        "instrumented glibc preload binary ASAN clean",
        run_binary_instrumented(
            "glibc-preload-bininst-asan-clean-instrumented",
            &asan_clean,
            &[
                ("ASAN_OPTIONS", asan_options.clone()),
                ("RSCHED_EXTRA_PRELOAD", asan_preload.clone()),
            ],
        ),
    );
    assert_report(
        "instrumented glibc preload binary ASAN buggy",
        run_binary_instrumented(
            "glibc-preload-bininst-asan-buggy-instrumented",
            &asan_buggy,
            &[
                ("ASAN_OPTIONS", asan_options),
                ("RSCHED_EXTRA_PRELOAD", asan_preload),
            ],
        ),
        "heap-use-after-free",
    );

    let ubsan_clean = build_binary_instrumentation_case(
        &source,
        "glibc-preload-bininst-ubsan-clean",
        &["CASE_UBSAN_CLEAN"],
        &["-fsanitize=undefined", "-static-libsan", "-static-libgcc"],
    );
    let ubsan_buggy = build_binary_instrumentation_case(
        &source,
        "glibc-preload-bininst-ubsan-buggy",
        &["CASE_UBSAN_UB"],
        &["-fsanitize=undefined", "-static-libsan", "-static-libgcc"],
    );
    let ubsan_options = OsString::from("halt_on_error=1");
    assert_clean(
        "instrumented glibc preload binary UBSAN clean",
        run_binary_instrumented(
            "glibc-preload-bininst-ubsan-clean-instrumented",
            &ubsan_clean,
            &[("UBSAN_OPTIONS", ubsan_options.clone())],
        ),
    );
    assert_report(
        "instrumented glibc preload binary UBSAN buggy",
        run_binary_instrumented(
            "glibc-preload-bininst-ubsan-buggy-instrumented",
            &ubsan_buggy,
            &[("UBSAN_OPTIONS", ubsan_options)],
        ),
        "signed integer overflow",
    );
}

#[test]
fn instrumented_glibc_preload_llvm_pass_sanitizers() {
    let asan_clean = build_llvm_instrumented_case(
        &llvm_sanitizer_subject(""),
        "glibc-preload-llvm-asan-clean",
        "address",
    );
    let asan_buggy = build_llvm_instrumented_case(
        &llvm_sanitizer_subject(
            "int *p = malloc(sizeof(int)); *p = 7; free(p); plain_counter += *p;",
        ),
        "glibc-preload-llvm-asan-buggy",
        "address",
    );
    let asan_options = OsString::from("halt_on_error=1:detect_leaks=0");
    assert_clean(
        "instrumented glibc preload LLVM ASAN clean",
        run_preloaded(&asan_clean, &[], &[("ASAN_OPTIONS", asan_options.clone())]),
    );
    assert_report(
        "instrumented glibc preload LLVM ASAN buggy",
        run_preloaded(&asan_buggy, &[], &[("ASAN_OPTIONS", asan_options)]),
        "AddressSanitizer",
    );

    let ubsan_clean = build_llvm_instrumented_case(
        &llvm_sanitizer_subject(""),
        "glibc-preload-llvm-ubsan-clean",
        "undefined",
    );
    let ubsan_buggy = build_llvm_instrumented_case(
        &llvm_sanitizer_subject("volatile int x = INT_MAX; plain_counter += x + 1;"),
        "glibc-preload-llvm-ubsan-buggy",
        "undefined",
    );
    let ubsan_options = OsString::from("halt_on_error=1");
    assert_clean(
        "instrumented glibc preload LLVM UBSAN clean",
        run_preloaded(
            &ubsan_clean,
            &[],
            &[("UBSAN_OPTIONS", ubsan_options.clone())],
        ),
    );
    assert_report(
        "instrumented glibc preload LLVM UBSAN buggy",
        run_preloaded(&ubsan_buggy, &[], &[("UBSAN_OPTIONS", ubsan_options)]),
        "signed integer overflow",
    );
}
