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

fn write_input(name: &str) -> PathBuf {
    let src = temp_dir().join(name);
    std::fs::write(
        &src,
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
"#,
    )
    .expect("write llvm pass C input");
    src
}

fn compile_to_ir(name: &str) -> PathBuf {
    let src = write_input(&format!("{name}.c"));
    let ll = temp_dir().join(format!("{name}.ll"));
    let output = run_timeout(
        "clang-17",
        [
            OsString::from("-S"),
            OsString::from("-emit-llvm"),
            OsString::from("-O0"),
            OsString::from("-g0"),
            OsString::from("-o"),
            ll.as_os_str().to_owned(),
            src.as_os_str().to_owned(),
        ],
    );
    assert!(
        output.status.success(),
        "clang-17 failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ll
}

fn run_pass(pass: &str, input: &Path, output_name: &str) -> String {
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
    std::fs::read_to_string(out).expect("read transformed LLVM IR")
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
