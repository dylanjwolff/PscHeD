//! Runs c-examples/io_cases.c, compiled without rsched.h, under an
//! instrumented libc. Only the libc read/write/socket wrappers can keep a
//! blocking read from hanging the only running task.

use std::path::Path;
use std::process::Command;

/// `(case, scheduled)`: whether rsched schedules the case's tasks. Preload
/// mode does not intercept libc's fork(), so the socketpair processes run
/// unscheduled and their I/O passes straight through.
const CASES: &[(&str, bool)] = &[
    ("CASE_IO_PIPE_THREADS", true),
    ("CASE_IO_SOCKETPAIR_PROCESSES", false),
    ("CASE_IO_TCP_THREADS", true),
    ("CASE_IO_FILE_THREADS", true),
];
const SEEDS: u32 = 4;

/// Each scheduled run must also log an I/O scheduling point.
pub fn check_io_cases(compiler: &str, runner: &Path, temp: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("c-examples/io_cases.c");
    for &(case, scheduled) in CASES {
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
                !scheduled || stderr.contains("[rsched] Io("),
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
