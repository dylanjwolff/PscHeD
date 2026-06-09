#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::sync::OnceLock;

static PLUGIN: OnceLock<PathBuf> = OnceLock::new();
static PRELOAD_LIB: OnceLock<PathBuf> = OnceLock::new();
static TSAN_PRELOAD_LIB: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug)]
struct RunOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-llvm-pass-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create llvm pass test temp dir");
    dir
}

fn plugin_path() -> PathBuf {
    PLUGIN.get_or_init(build_plugin).clone()
}

fn rsched_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("llvm-pass crate should live under rsched/llvm-pass")
        .to_path_buf()
}

fn preload_lib() -> PathBuf {
    PRELOAD_LIB
        .get_or_init(|| build_preload_cdylib("target-preload", &[]))
        .clone()
}

fn tsan_preload_lib() -> PathBuf {
    TSAN_PRELOAD_LIB
        .get_or_init(|| build_preload_cdylib("target-preload-tsan", &["--features", "tsan"]))
        .clone()
}

fn build_preload_cdylib(target_name: &str, extra_args: &[&str]) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join(target_name);
    let mut cmd = Command::new(cargo);
    cmd.current_dir(rsched_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "-p", "rsched-preload"])
        .args(extra_args);
    let output = cmd.output().expect("spawn cargo build for preload cdylib");
    assert!(
        output.status.success(),
        "cargo build -p rsched-preload failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let path = target_dir.join("debug").join("librsched_preload.so");
    assert!(
        path.exists(),
        "expected preload cdylib at {}",
        path.display()
    );
    path
}

fn build_plugin() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join("plugin-target");
    let output = run_timeout(
        cargo,
        [
            OsString::from("build"),
            OsString::from("-p"),
            OsString::from("rsched-llvm-pass"),
            OsString::from("--lib"),
            OsString::from("--target-dir"),
            target_dir.as_os_str().to_owned(),
        ],
    );
    assert!(
        output.status.success(),
        "cargo build for LLVM pass plugin failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let plugin = target_dir.join("debug").join("librsched_llvm_pass.so");
    assert!(
        plugin.exists(),
        "expected LLVM pass plugin at {}",
        plugin.display()
    );
    plugin
}

fn run_timeout<P, I, S>(program: P, args: I) -> Output
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("timeout");
    cmd.arg("--kill-after=5s").arg("30s").arg(program.as_ref());
    for arg in args {
        cmd.arg(arg);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to spawn command through timeout: {e}"))
}

fn run_program<P, I, S>(program: P, args: I, envs: &[(&str, OsString)]) -> RunOutput
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("timeout");
    cmd.arg("--kill-after=5s").arg("30s").arg(program.as_ref());
    for arg in args {
        cmd.arg(arg);
    }
    cmd.env_remove("LD_LIBRARY_PATH");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn command through timeout: {e}"));
    RunOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        timed_out: output.status.code() == Some(124) || output.status.code() == Some(137),
    }
}

fn write_input(name: &str, contents: &str) -> PathBuf {
    let src = temp_dir().join(name);
    std::fs::write(&src, contents).expect("write llvm pass C input");
    src
}

fn atomic_pthread_input() -> &'static str {
    r#"
#include <pthread.h>
#include <stdatomic.h>

static _Atomic int global;

static void *worker(void *arg) {
    atomic_fetch_add_explicit(&global, 1, memory_order_seq_cst);
    return arg;
}

int main(void) {
    pthread_t thread;
    pthread_create(&thread, 0, worker, 0);
    pthread_join(thread, 0);
    return atomic_load_explicit(&global, memory_order_seq_cst);
}
"#
}

fn fuzzer_input() -> &'static str {
    r#"
#include <stdint.h>
#include <stddef.h>
#include <stdatomic.h>

static _Atomic uint32_t observations;

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size) {
    atomic_fetch_add_explicit(&observations, 1, memory_order_relaxed);
    if (size >= 4 && data[0] == 'r' && data[1] == 's' &&
        data[2] == 'c' && data[3] == 'h') {
        __builtin_trap();
    }
    return 0;
}
"#
}

fn musl_pthread_input() -> &'static str {
    r#"
typedef struct { int value; } pthread_mutex_t;

int __pthread_mutex_lock(pthread_mutex_t *mutex) {
    return __atomic_exchange_n(&mutex->value, 1, __ATOMIC_SEQ_CST);
}

extern __typeof(__pthread_mutex_lock) pthread_mutex_lock
    __attribute__((weak, alias("__pthread_mutex_lock")));

int __clone(int (*func)(void *), void *stack, int flags, void *arg, int *ptid, void *tls, int *ctid);

int create_with_clone(int (*func)(void *), void *stack, void *arg, int *ptid, void *tls, int *ctid) {
    return __clone(func, stack, 0, arg, ptid, tls, ctid);
}
"#
}

fn glibc_pthread_input() -> &'static str {
    r#"
typedef struct { int value; } pthread_mutex_t;
struct clone_args { unsigned long long fields[11]; };

int ___pthread_mutex_lock(pthread_mutex_t *mutex) {
    return __atomic_exchange_n(&mutex->value, 1, __ATOMIC_SEQ_CST);
}

int __clone_internal(struct clone_args *, int (*)(void *), void *);

int create_with_clone(struct clone_args *args, int (*func)(void *), void *arg) {
    return __clone_internal(args, func, arg);
}
"#
}

fn compile_to_ir(name: &str) -> PathBuf {
    compile_source_to_ir(name, atomic_pthread_input(), &[])
}

fn compile_source_to_ir(name: &str, source: &str, extra_args: &[&str]) -> PathBuf {
    let src = write_input(&format!("{name}.c"), source);
    let ll = temp_dir().join(format!("{name}.ll"));
    let mut args = vec![
        OsString::from("-S"),
        OsString::from("-emit-llvm"),
        OsString::from("-O0"),
        OsString::from("-g0"),
    ];
    args.extend(extra_args.iter().map(OsString::from));
    args.extend([
        OsString::from("-o"),
        ll.as_os_str().to_owned(),
        src.as_os_str().to_owned(),
    ]);
    let output = run_timeout("clang-17", args);
    assert!(
        output.status.success(),
        "clang-17 failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ll
}

fn run_pass(pass: &str, input: &Path, output_name: &str) -> String {
    let out = run_pass_to_file(pass, input, output_name);
    std::fs::read_to_string(out).expect("read transformed LLVM IR")
}

fn run_pass_to_file(pass: &str, input: &Path, output_name: &str) -> PathBuf {
    let out = temp_dir().join(output_name);
    let output = run_timeout(
        "opt-17",
        [
            OsString::from("-load-pass-plugin"),
            plugin_path().as_os_str().to_owned(),
            OsString::from(format!("-passes={pass}")),
            OsString::from("-S"),
            input.as_os_str().to_owned(),
            OsString::from("-o"),
            out.as_os_str().to_owned(),
        ],
    );
    assert!(
        output.status.success(),
        "opt-17 failed for {pass}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    out
}

fn link_ir(name: &str, ir: &Path, sanitizer: &str, preload: &Path) -> PathBuf {
    let out = temp_dir().join(name);
    let lib_dir = preload
        .parent()
        .expect("preload library should have parent directory");
    let output = run_timeout(
        "clang-17",
        [
            OsString::from(format!("-fsanitize={sanitizer}")),
            OsString::from("-g"),
            OsString::from("-O0"),
            OsString::from("-o"),
            out.as_os_str().to_owned(),
            ir.as_os_str().to_owned(),
            OsString::from(format!("-L{}", lib_dir.display())),
            OsString::from("-lrsched_preload"),
            OsString::from(format!("-Wl,-rpath,{}", lib_dir.display())),
            OsString::from("-lpthread"),
        ],
    );
    assert!(
        output.status.success(),
        "clang-17 failed to link {name}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    out
}

fn instrumented_sanitizer_binary(
    name: &str,
    source: &str,
    sanitizer: &str,
    preload: &Path,
) -> PathBuf {
    let input = compile_source_to_ir(name, source, &[&format!("-fsanitize={sanitizer}")]);
    let transformed = run_pass_to_file("rsched-atomics", &input, &format!("{name}.out.ll"));
    let ir = std::fs::read_to_string(&transformed).expect("read transformed sanitizer IR");
    assert!(
        ir.contains("call void @rsched_atomic_instrument"),
        "sanitizer subject was not instrumented:\n{ir}"
    );
    link_ir(name, &transformed, sanitizer, preload)
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

fn sanitizer_subject(extra: &str) -> String {
    format!(
        r#"
#include <pthread.h>
#include <stdatomic.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>

static _Atomic int observed;
static int plain_counter;

static void touch_atomic(void) {{
    atomic_fetch_add_explicit(&observed, 1, memory_order_seq_cst);
}}

static void *race_worker(void *arg) {{
    (void)arg;
    touch_atomic();
    plain_counter++;
    return NULL;
}}

int main(void) {{
    touch_atomic();
    {extra}
    printf("observed=%d\n", atomic_load_explicit(&observed, memory_order_seq_cst));
    return 0;
}}
"#
    )
}

#[test]
fn instruments_llvm_atomic_operations() {
    let input = compile_to_ir("atomics");
    let ir = run_pass("rsched-atomics", &input, "atomics.out.ll");

    assert!(
        ir.contains("call void @rsched_atomic_instrument"),
        "pass did not insert rsched atomic hook:\n{ir}"
    );
    assert!(
        ir.contains("atomicrmw") || ir.contains("load atomic"),
        "pass should leave original LLVM atomic operation in place:\n{ir}"
    );
}

#[test]
fn musl_libc_mode_wraps_pthread_implementation_and_weak_alias() {
    let input = compile_source_to_ir("musl-pthread", musl_pthread_input(), &[]);
    let ir = run_pass(
        "rsched-atomics<musl-libc>",
        &input,
        "musl-pthread.instrumented.ll",
    );

    assert!(
        ir.contains("define dso_local i32 @__rsched_real_pthread_mutex_lock"),
        "pass did not preserve the original musl implementation:\n{ir}"
    );
    assert!(
        ir.contains("@__rsched_real_alias_pthread_mutex_lock"),
        "pass did not rename the musl weak alias:\n{ir}"
    );
    assert!(
        ir.contains("define i32 @__pthread_mutex_lock")
            && ir.contains("define i32 @pthread_mutex_lock"),
        "pass did not create internal and public wrappers:\n{ir}"
    );
    assert!(
        ir.matches("call i1 @rsched_try_enter").count() == 2,
        "each wrapper should enter the recursion guard:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @rsched_pthread_mutex_lock")
            && ir.contains("call i32 @__rsched_real_pthread_mutex_lock"),
        "wrappers did not dispatch to rsched and the original musl body:\n{ir}"
    );
    assert!(
        ir.contains("call void @rsched_atomic_instrument"),
        "original musl atomics were not instrumented:\n{ir}"
    );
    assert!(
        ir.contains("@rsched_clone"),
        "musl mode did not rewrite __clone calls:\n{ir}"
    );
}

#[test]
fn glibc_libc_mode_wraps_pthread_implementation_and_clone_internal() {
    let input = compile_source_to_ir("glibc-pthread", glibc_pthread_input(), &[]);
    let ir = run_pass(
        "rsched-atomics<glibc-libc>",
        &input,
        "glibc-pthread.instrumented.ll",
    );

    assert!(
        ir.contains("define dso_local i32 @__rsched_real_pthread_mutex_lock"),
        "pass did not preserve the original glibc implementation:\n{ir}"
    );
    assert!(
        ir.contains("define i32 @___pthread_mutex_lock"),
        "pass did not create the glibc implementation wrapper:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @rsched_pthread_mutex_lock")
            && ir.contains("call i32 @__rsched_real_pthread_mutex_lock"),
        "glibc wrappers did not dispatch through rsched recursion guards:\n{ir}"
    );
    assert!(
        ir.contains("@rsched_clone_internal"),
        "glibc mode did not rewrite __clone_internal calls:\n{ir}"
    );
}

#[test]
fn direct_pthread_mode_rewrites_libc_pthread_calls() {
    let input = compile_to_ir("direct_pthread");
    let ir = run_pass(
        "rsched-atomics<direct-pthread>",
        &input,
        "direct_pthread.out.ll",
    );

    assert!(
        ir.contains("@rsched_pthread_create"),
        "direct mode did not rewrite pthread_create:\n{ir}"
    );
    assert!(
        ir.contains("@rsched_pthread_join"),
        "direct mode did not rewrite pthread_join:\n{ir}"
    );
}

#[test]
fn fuzzer_sanitizer_ir_is_supported() {
    let input = compile_source_to_ir("fuzzer", fuzzer_input(), &["-fsanitize=fuzzer-no-link"]);
    let ir = run_pass("rsched-atomics", &input, "fuzzer.out.ll");

    assert!(
        ir.contains("define dso_local i32 @__rsched_original_LLVMFuzzerTestOneInput"),
        "original fuzzer entry point was not renamed:\n{ir}"
    );
    assert!(
        ir.contains("define i32 @LLVMFuzzerTestOneInput")
            || ir.contains("define dso_local i32 @LLVMFuzzerTestOneInput"),
        "fuzzer entry point was not preserved:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @rsched_fuzzer_test_one_input"),
        "fuzzer wrapper does not delegate to rsched schedule loop:\n{ir}"
    );
    assert!(
        ir.contains("__sancov") || ir.contains("__sanitizer_cov"),
        "clang did not emit sanitizer coverage for fuzzer input:\n{ir}"
    );
    assert!(
        ir.contains("call void @rsched_atomic_instrument"),
        "pass did not instrument atomic operation in fuzzer harness:\n{ir}"
    );
    assert!(
        ir.contains("atomicrmw"),
        "pass should leave original atomicrmw in place:\n{ir}"
    );
}

#[test]
fn llvm_pass_asan_compatibility() {
    let preload = preload_lib();
    let clean = instrumented_sanitizer_binary(
        "llvm_asan_clean",
        &sanitizer_subject(""),
        "address",
        &preload,
    );
    let buggy = instrumented_sanitizer_binary(
        "llvm_asan_buggy",
        &sanitizer_subject("int *p = malloc(sizeof(int)); *p = 7; free(p); plain_counter += *p;"),
        "address",
        &preload,
    );
    let asan_options = OsString::from("halt_on_error=1:detect_leaks=0");

    assert_clean(
        "llvm pass asan clean",
        run_program(
            &clean,
            std::iter::empty::<OsString>(),
            &[("ASAN_OPTIONS", asan_options.clone())],
        ),
    );
    assert_sanitizer_report(
        "llvm pass asan buggy",
        run_program(
            &buggy,
            std::iter::empty::<OsString>(),
            &[("ASAN_OPTIONS", asan_options)],
        ),
        "AddressSanitizer",
    );
}

#[test]
fn llvm_pass_ubsan_compatibility() {
    let preload = preload_lib();
    let clean = instrumented_sanitizer_binary(
        "llvm_ubsan_clean",
        &sanitizer_subject(""),
        "undefined",
        &preload,
    );
    let buggy = instrumented_sanitizer_binary(
        "llvm_ubsan_buggy",
        &sanitizer_subject("volatile int x = INT_MAX; plain_counter += x + 1;"),
        "undefined",
        &preload,
    );
    let ubsan_options = OsString::from("halt_on_error=1");

    assert_clean(
        "llvm pass ubsan clean",
        run_program(
            &clean,
            std::iter::empty::<OsString>(),
            &[("UBSAN_OPTIONS", ubsan_options.clone())],
        ),
    );
    assert_sanitizer_report(
        "llvm pass ubsan buggy",
        run_program(
            &buggy,
            std::iter::empty::<OsString>(),
            &[("UBSAN_OPTIONS", ubsan_options)],
        ),
        "signed integer overflow",
    );
}

#[test]
fn llvm_pass_tsan_compatibility() {
    let preload = tsan_preload_lib();
    let clean = instrumented_sanitizer_binary(
        "llvm_tsan_clean",
        &sanitizer_subject(""),
        "thread",
        &preload,
    );
    let buggy = instrumented_sanitizer_binary(
        "llvm_tsan_buggy",
        &sanitizer_subject(
            "pthread_t t; pthread_create(&t, 0, race_worker, 0); plain_counter++; pthread_join(t, 0);",
        ),
        "thread",
        &preload,
    );
    let tsan_options = OsString::from("halt_on_error=1");

    assert_clean(
        "llvm pass tsan clean",
        run_program(
            Path::new("setarch"),
            [
                OsString::from("x86_64"),
                OsString::from("-R"),
                clean.as_os_str().to_owned(),
            ],
            &[("TSAN_OPTIONS", tsan_options.clone())],
        ),
    );
    assert_sanitizer_report(
        "llvm pass tsan buggy",
        run_program(
            Path::new("setarch"),
            [
                OsString::from("x86_64"),
                OsString::from("-R"),
                buggy.as_os_str().to_owned(),
            ],
            &[("TSAN_OPTIONS", tsan_options)],
        ),
        "ThreadSanitizer: data race",
    );
}
