#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

static PLUGIN: OnceLock<PathBuf> = OnceLock::new();

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-llvm-pass-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create llvm pass test temp dir");
    dir
}

fn plugin_path() -> PathBuf {
    PLUGIN.get_or_init(build_plugin).clone()
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
