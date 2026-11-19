use rsched_libc_build::ArtifactManifest;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "support/io_cases.rs"]
mod io_cases;

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
#include <sched.h>
#include <stdio.h>

static void *worker(void *arg)
{
    return arg;
}

static void *exiting_worker(void *arg)
{
    pthread_exit(arg);
}

int main(void)
{
    pthread_t thread;
    void *result = 0, *exited = 0, *later = 0;
    pthread_attr_t detached;
    if (pthread_create(&thread, 0, worker, (void *)42) != 0 || pthread_join(thread, &result) != 0)
        return 1;
    if (pthread_create(&thread, 0, exiting_worker, (void *)9) != 0 ||
        pthread_join(thread, &exited) != 0)
        return 2;
    /* Detached threads exit unjoined; libc then reuses their descriptors. */
    pthread_attr_init(&detached);
    pthread_attr_setdetachstate(&detached, PTHREAD_CREATE_DETACHED);
    for (int i = 0; i < 3; i++)
        if (pthread_create(&thread, &detached, worker, 0) != 0)
            return 3;
    for (int i = 0; i < 10; i++)
        sched_yield();
    if (pthread_create(&thread, 0, worker, (void *)6) != 0 || pthread_join(thread, &later) != 0)
        return 4;
    /* Printing after the joins shows no worker's exit ended the process. */
    printf("joined=%ld exited=%ld later=%ld\n", (long)result, (long)exited, (long)later);
    return 0;
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("joined=42 exited=9 later=6"),
        "main did not continue after the worker exited:\n{stdout}"
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

#[test]
fn io_message_passing_with_instrumented_musl_preloaded() {
    let runner = artifact().runner.expect("musl artifact runner");
    // RSCHED_LOG=1 crashes instrumented musl even without I/O, so this relies
    // on the cases finishing: an unmodeled blocking read hangs.
    io_cases::check_io_cases("musl-gcc", &runner, &temp_dir(), false);
}
