use rsched_libc_build::ArtifactManifest;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-musl-preload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create musl preload test temp dir");
    dir
}

fn artifact() -> ArtifactManifest {
    let path = std::env::var_os("RSCHED_MUSL_ARTIFACT").expect(
        "RSCHED_MUSL_ARTIFACT is required; run `cargo xtask test preload musl --provider native`",
    );
    ArtifactManifest::load(path).expect("load instrumented musl artifact")
}

#[test]
fn ordinary_binary_runs_with_instrumented_musl_preloaded() {
    let artifact = artifact();
    let source = temp_dir().join("preload-smoke.c");
    let binary = temp_dir().join("preload-smoke");
    std::fs::write(
        &source,
        r#"
#include <pthread.h>
#include <stdatomic.h>

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
    if (pthread_create(&thread, 0, worker, 0) != 0)
        return 1;
    atomic_fetch_add_explicit(&counter, 1, memory_order_seq_cst);
    if (pthread_join(thread, 0) != 0)
        return 2;
    return atomic_load_explicit(&counter, memory_order_seq_cst) == 2 ? 0 : 3;
}
"#,
    )
    .expect("write musl preload smoke test");

    let status = Command::new("musl-gcc")
        .args(["-pthread", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .expect("compile musl preload smoke test");
    assert!(status.success(), "compile musl preload smoke test failed");

    let runner = artifact.runner.expect("musl artifact runner");
    let output = Command::new("timeout")
        .args(["--kill-after=5s", "30s"])
        .arg(runner)
        .arg(binary)
        .output()
        .expect("run musl preload smoke test");
    assert!(
        output.status.success(),
        "musl preload smoke failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn artifact_contains_application_instrumentation_plugin() {
    let artifact = artifact();
    assert!(
        artifact.llvm_plugin.is_file(),
        "missing LLVM pass at {}",
        artifact.llvm_plugin.display()
    );
    assert!(repo_root().join("llvm-pass").is_dir());
}
