#![cfg(not(feature = "tsan"))]

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
static PRELOAD_LIB: OnceLock<PathBuf> = OnceLock::new();
static ARTIFACT_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct RunOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("preload crate should live under rsched/preload")
        .to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rsched-preload-client-server-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create preload client/server temp dir");
    dir
}

fn preload_lib() -> PathBuf {
    PRELOAD_LIB.get_or_init(build_preload_cdylib).clone()
}

fn build_preload_cdylib() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target_dir = temp_dir().join(if cfg!(feature = "tsan") {
        "target-tsan"
    } else {
        "target-default"
    });
    let mut cmd = Command::new(cargo);
    cmd.current_dir(repo_root())
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "-p", "rsched-preload"]);
    if cfg!(feature = "tsan") {
        cmd.args(["--features", "tsan"]);
    }

    let status = cmd.status().expect("spawn cargo build for preload cdylib");
    assert!(status.success(), "cargo build -p rsched-preload failed");

    let path = target_dir.join("debug").join("librsched_preload.so");
    assert!(
        path.exists(),
        "expected preload cdylib at {}",
        path.display()
    );
    path
}

fn host_target() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        other => panic!("unsupported preload client/server test host: {other:?}"),
    }
}

fn next_artifact_id() -> usize {
    ARTIFACT_ID.fetch_add(1, Ordering::Relaxed)
}

fn write_source(name: &str, artifact_id: usize, source: &str) -> PathBuf {
    let src = temp_dir().join(format!("{name}-{artifact_id}.c"));
    fs::write(&src, source).unwrap_or_else(|e| panic!("write {}: {e}", src.display()));
    src
}

fn build_c_program(name: &str, artifact_id: usize, source: &str) -> PathBuf {
    let src = write_source(name, artifact_id, source);
    let out = temp_dir().join(format!(
        "{name}-{artifact_id}{}",
        std::env::consts::EXE_SUFFIX
    ));
    let mut compiler = cc::Build::new()
        .compiler("clang")
        .host(host_target())
        .target(host_target())
        .opt_level(0)
        .cargo_metadata(false)
        .get_compiler()
        .to_command();
    let status = compiler
        .args(["-g", "-Wall", "-Wextra", "-pthread"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke clang for {}: {e}", src.display()));
    assert!(status.success(), "failed to build {}", src.display());
    out
}

fn write_script(artifact_id: usize) -> PathBuf {
    let script = temp_dir().join(format!("run_client_server-{artifact_id}.sh"));
    fs::write(
        &script,
        r#"#!/usr/bin/env bash
set -u
server=$1
client=$2
ipc=$3
out=$4
"$client" "$ipc" &
client_pid=$!
"$server" "$ipc" > "$out" &
server_pid=$!
wait "$client_pid"
client_status=$?
wait "$server_pid"
server_status=$?
while IFS= read -r line; do
    printf '%s\n' "$line"
done < "$out"
exit $((client_status != 0 ? client_status : server_status))
"#,
    )
    .unwrap_or_else(|e| panic!("write {}: {e}", script.display()));
    let mut perms = fs::metadata(&script)
        .unwrap_or_else(|e| panic!("stat {}: {e}", script.display()))
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms)
        .unwrap_or_else(|e| panic!("chmod {}: {e}", script.display()));
    script
}

fn server_source() -> &'static str {
    r#"
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s FIFO\n", argv[0]);
        return 2;
    }

    unlink(argv[1]);
    if (mkfifo(argv[1], 0600) != 0) {
        perror("mkfifo");
        return 2;
    }

    int fd = open(argv[1], O_RDONLY | O_NONBLOCK);
    if (fd < 0) {
        perror("open fifo");
        return 2;
    }
    sched_yield();

    for (int i = 0; i < 8; i++)
        sched_yield();

    char c = 0;
    ssize_t n = read(fd, &c, 1);
    if (n == 1 && (c == 'A' || c == 'B')) {
        printf("seen=%c\n", c);
    } else if (n == 0 || (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))) {
        printf("seen=NONE\n");
    } else {
        perror("read");
        return 2;
    }
    close(fd);
    unlink(argv[1]);
    return 0;
}
"#
}

fn client_source() -> &'static str {
    r#"
#include <pthread.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

struct thread_arg {
    const char *path;
    char msg;
};

static void *send_msg(void *raw) {
    struct thread_arg *arg = (struct thread_arg *)raw;
    for (int attempt = 0; attempt < 16; attempt++) {
        sched_yield();
        int fd = open(arg->path, O_WRONLY | O_NONBLOCK);
        if (fd >= 0) {
            if (write(fd, &arg->msg, 1) == 1) {
                close(fd);
                return 0;
            }
            close(fd);
        } else if (errno != ENOENT && errno != ENXIO) {
            return 0;
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s FIFO\n", argv[0]);
        return 2;
    }

    signal(SIGPIPE, SIG_IGN);
    pthread_t a;
    pthread_t b;
    struct thread_arg arg_a = { argv[1], 'A' };
    struct thread_arg arg_b = { argv[1], 'B' };
    if (pthread_create(&a, 0, send_msg, &arg_a) != 0)
        return 2;
    if (pthread_create(&b, 0, send_msg, &arg_b) != 0)
        return 2;
    if (pthread_join(a, 0) != 0)
        return 2;
    if (pthread_join(b, 0) != 0)
        return 2;
    return 0;
}
"#
}

fn run_script(script: &Path, server: &Path, client: &Path, seed: u64) -> RunOutput {
    let run_id = next_artifact_id();
    let out_file = temp_dir().join(format!("client-server-{seed}-{run_id}.out"));
    let ipc_path = temp_dir().join(format!("client-server-{seed}-{run_id}.fifo"));
    let _ = fs::remove_file(&ipc_path);
    let preload = preload_lib();

    let output = Command::new("timeout")
        .arg("--kill-after=5s")
        .arg(format!("{}s", TIMEOUT.as_secs()))
        .arg("env")
        .arg(format!("LD_PRELOAD={}", preload.display()))
        .arg(format!("RANDOM_SEED={seed}"))
        .arg("bash")
        .arg(script)
        .arg(server)
        .arg(client)
        .arg(ipc_path)
        .arg(out_file)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", script.display()));

    RunOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        timed_out: output.status.code() == Some(124) || output.status.code() == Some(137),
    }
}

fn seen_from_output(seed: u64, output: RunOutput) -> String {
    assert!(
        !output.timed_out,
        "client/server seed {seed} timed out\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.status.success(),
        "client/server seed {seed} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.stdout,
        output.stderr
    );
    let seen = output
        .stdout
        .split_whitespace()
        .find_map(|part| part.strip_prefix("seen="))
        .unwrap_or_else(|| {
            panic!(
                "client/server seed {seed} did not print seen=...\nstdout:\n{}\nstderr:\n{}",
                output.stdout, output.stderr
            )
        });
    assert!(
        seen == "A" || seen == "B" || seen == "NONE",
        "client/server seed {seed} produced invalid seen={seen:?}"
    );
    seen.to_owned()
}

fn build_subjects() -> (PathBuf, PathBuf, PathBuf) {
    let _ = preload_lib();
    let artifact_id = next_artifact_id();
    let server = build_c_program("preload_fifo_server", artifact_id, server_source());
    let client = build_c_program("preload_fifo_client", artifact_id, client_source());
    let script = write_script(artifact_id);
    (script, server, client)
}

#[test]
fn bash_client_server_is_deterministic_per_seed() {
    let (script, server, client) = build_subjects();
    for seed in 0..10 {
        let a = seen_from_output(seed, run_script(&script, &server, &client, seed));
        let b = seen_from_output(seed, run_script(&script, &server, &client, seed));
        assert_eq!(a, b, "seed {seed}: first seen={a} second seen={b}");
    }
}

#[test]
fn bash_client_server_explores_ipc_interleavings() {
    let (script, server, client) = build_subjects();
    let mut seen = HashSet::new();
    for seed in 0..120 {
        seen.insert(seen_from_output(
            seed,
            run_script(&script, &server, &client, seed),
        ));
    }
    let expected = HashSet::from(["A".to_owned(), "B".to_owned(), "NONE".to_owned()]);
    assert_eq!(
        seen, expected,
        "expected server to observe A, B, and an empty non-blocking read across seeds"
    );
}
