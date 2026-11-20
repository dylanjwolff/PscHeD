//! Runs c-examples/io_cases.c, compiled without rsched.h, under an
//! instrumented libc. Only the libc read/write/socket wrappers can keep a
//! blocking read from hanging the only running task.

use std::path::Path;
use std::process::Command;

const CASES: &[&str] = &[
    "CASE_IO_PIPE_THREADS",
    "CASE_IO_SOCKETPAIR_PROCESSES",
    "CASE_IO_TCP_THREADS",
    "CASE_IO_FILE_THREADS",
];
const SEEDS: u32 = 4;

/// Each run must also log an I/O scheduling point, including the
/// socketpair case's forked child.
pub fn check_io_cases(compiler: &str, runner: &Path, temp: &Path) {
    check_reserved_fds(compiler, runner, temp);
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("c-examples/io_cases.c");
    for &case in CASES {
        let binary = temp.join(case);
        let status = Command::new(compiler)
            .args(["-O0", "-pthread", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-D{case}"))
            .arg("-o")
            .arg(&binary)
            .arg(&source)
            .status()
            .unwrap_or_else(|error| panic!("run {compiler}: {error}"));
        assert!(status.success(), "{compiler} failed to build {case}");

        for seed in 0..SEEDS {
            let mut command = Command::new("timeout");
            command
                .args(["--kill-after=5s", "30s"])
                .arg(runner)
                .arg(&binary)
                .env("RANDOM_SEED", seed.to_string())
                .env("RSCHED_LOG", "1");
            let output = command
                .output()
                .expect("run io case under instrumented libc");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() && stdout.contains("trace="),
                "{case} with seed {seed} failed with {} (124 means it hung)\n\
                 stdout:\n{stdout}\nstderr tail:\n{}",
                output.status,
                tail(&stderr),
            );
            assert!(
                stderr.contains("[rsched] Io("),
                "{case} with seed {seed} reached no I/O scheduling point\nstderr tail:\n{}",
                tail(&stderr),
            );
        }
    }
}

fn tail(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// An unmodified program that closes every descriptor it did not open, and
/// dups over every number, must leave rsched's memfd and shared epoll open.
fn check_reserved_fds(compiler: &str, runner: &Path, temp: &Path) {
    let source = temp.join("reserved_fds.c");
    std::fs::write(
        &source,
        r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void *worker(void *arg) { return arg; }

/* rsched's descriptors, found by what they point to. */
static int count_reserved(void) {
    int n = 0;
    for (int fd = 3; fd < 4096; fd++) {
        char path[64], target[256];
        snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
        ssize_t len = readlink(path, target, sizeof target - 1);
        if (len <= 0)
            continue;
        target[len] = 0;
        n += strstr(target, "rsched-process-shared") || !strcmp(target, "anon_inode:[eventpoll]");
    }
    return n;
}

int main(void) {
    pthread_t thread;
    pthread_create(&thread, 0, worker, 0);
    pthread_join(thread, 0);
    int before = count_reserved();
    for (int fd = 3; fd < 4096; fd++)
        close(fd);
    for (int fd = 3; fd < 4096; fd++) {
        dup2(1, fd);
        close(fd);
    }
#ifdef __GLIBC__
    closefrom(3);
    close_range(3, ~0U, 0);
#endif
    int after = count_reserved();
    printf("before=%d after=%d\n", before, after);
    return before == 2 && after == 2 ? 0 : 1;
}
"#,
    )
    .expect("write reserved fd subject");
    let binary = temp.join("reserved_fds");
    let status = Command::new(compiler)
        .args(["-O0", "-pthread", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .unwrap_or_else(|error| panic!("run {compiler}: {error}"));
    assert!(
        status.success(),
        "{compiler} failed to build reserved_fds.c"
    );
    let output = Command::new("timeout")
        .args(["--kill-after=5s", "30s"])
        .arg(runner)
        .arg(&binary)
        .output()
        .expect("run reserved fd subject under instrumented libc");
    assert!(
        output.status.success(),
        "the program closed rsched's descriptors ({})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
