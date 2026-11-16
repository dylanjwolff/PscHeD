use rsched_libc_build::ArtifactManifest;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

#[cfg(not(feature = "coro"))]
#[path = "support/glibc_cases.rs"]
mod glibc_cases;

static INSTRUMENTED_GLIBC: OnceLock<InstrumentedGlibc> = OnceLock::new();

#[derive(Clone)]
struct InstrumentedGlibc {
    #[cfg_attr(feature = "coro", allow(dead_code))]
    build_dir: PathBuf,
    #[cfg_attr(feature = "coro", allow(dead_code))]
    plugin: PathBuf,
    #[cfg_attr(feature = "coro", allow(dead_code))]
    loader: PathBuf,
    #[cfg_attr(feature = "coro", allow(dead_code))]
    library_path: String,
    preload_runner: PathBuf,
    #[cfg(not(feature = "coro"))]
    asan_runtime: PathBuf,
}

#[cfg(not(feature = "coro"))]
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-glibc-preload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create glibc preload test temp dir");
    dir
}

fn build_instrumented_glibc() -> InstrumentedGlibc {
    INSTRUMENTED_GLIBC
        .get_or_init(|| {
            let path = std::env::var_os("RSCHED_GLIBC_ARTIFACT").expect(
                "RSCHED_GLIBC_ARTIFACT is required; \
                 run `cargo xtask test preload glibc --provider native`",
            );
            let artifact = ArtifactManifest::load(path).expect("load instrumented glibc artifact");
            InstrumentedGlibc {
                build_dir: artifact.build_dir.expect("glibc artifact build directory"),
                plugin: artifact.llvm_plugin,
                loader: artifact.loader.expect("glibc artifact loader"),
                library_path: artifact.library_path.expect("glibc artifact library path"),
                preload_runner: artifact.runner.expect("glibc artifact runner"),
                #[cfg(not(feature = "coro"))]
                asan_runtime: artifact.asan_runtime.expect("glibc artifact ASAN runtime"),
            }
        })
        .clone()
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
fn ordinary_binary_runs_with_instrumented_glibc_preloaded() {
    let glibc = build_instrumented_glibc();
    let smoke = build_preload_smoke(&temp_dir());
    let mut command = Command::new(&glibc.preload_runner);
    command.arg(&smoke);
    run(
        command,
        "run ordinary pthread binary with instrumented glibc preloaded",
    );
}
