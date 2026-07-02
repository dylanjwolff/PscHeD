use rsched_libc_build::{ArtifactManifest, InstrumentationMode, Provider};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

fn artifact() -> ArtifactManifest {
    let path = std::env::var_os("RSCHED_GLIBC_ARTIFACT").expect(
        "RSCHED_GLIBC_ARTIFACT is required; run `cargo xtask test libc glibc --provider native`",
    );
    ArtifactManifest::load(path).expect("load instrumented glibc artifact")
}

fn jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

fn tail_text(text: &str, max_lines: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(max_lines)..].join("\n")
}

fn read_optional(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn summarize_results(build_dir: &Path) -> String {
    let tests_sum = read_optional(&build_dir.join("tests.sum"));
    let tests_log = read_optional(&build_dir.join("tests.log"));
    let failures = tests_sum
        .lines()
        .filter(|line| line.starts_with("FAIL:") || line.starts_with("XPASS:"))
        .collect::<Vec<_>>();
    let unresolved = tests_sum
        .lines()
        .filter(|line| line.starts_with("UNRESOLVED:") || line.starts_with("ERROR:"))
        .collect::<Vec<_>>();

    let mut summary = format!(
        "glibc tests.sum failures: {}, unresolved/errors: {}",
        failures.len(),
        unresolved.len()
    );
    if !failures.is_empty() {
        summary.push_str("\nfirst failing tests:");
        for failure in failures.iter().take(80) {
            summary.push('\n');
            summary.push_str(failure);
        }
    }
    if !unresolved.is_empty() {
        summary.push_str("\nfirst unresolved/error tests:");
        for line in unresolved.iter().take(40) {
            summary.push('\n');
            summary.push_str(line);
        }
    }
    if tests_sum.is_empty() && tests_log.is_empty() {
        summary.push_str("\nno tests.sum or tests.log was produced");
    } else if failures.is_empty() && unresolved.is_empty() {
        summary.push_str("\nno FAIL/XPASS/UNRESOLVED/ERROR entries found");
    } else {
        summary.push_str("\nlast tests.log lines:");
        summary.push('\n');
        summary.push_str(&tail_text(&tests_log, 120));
    }
    summary
}

fn add_instrumented_link_args(command: &mut Command, artifact: &ArtifactManifest) {
    let rsched = artifact
        .rsched_library
        .as_ref()
        .expect("instrumented glibc artifact rsched library");
    let gcc_plugin = artifact
        .gcc_plugin
        .as_ref()
        .expect("instrumented glibc artifact GCC plugin");
    let shared_gnulib = format!("-lgcc_s -lgcc {}", rsched.display());
    let static_gnulib = format!(
        "{} -lgcc -lgcc_eh -Wl,--allow-multiple-definition",
        rsched.display()
    );
    command
        .env("RSCHED_GCC_PLUGIN", gcc_plugin)
        .arg(format!("libc.so-gnulib={shared_gnulib}"))
        .arg(format!("gnulib={shared_gnulib}"))
        // The conformance subjects should exercise the instrumented libc,
        // not pull Rust/compiler_builtins from librsched.a into every test.
        .arg("gnulib-tests=-lgcc_s -lgcc")
        .arg(format!("static-gnulib={static_gnulib}"))
        .arg("static-gnulib-tests=-lgcc -lgcc_eh");
}

fn glibc_build_library_path(build_dir: &Path) -> String {
    [
        "", "math", "elf", "dlfcn", "nss", "nis", "rt", "resolv", "mathvec", "support", "crypt",
        "nptl", "nptl_db", "login", "locale", "malloc",
    ]
    .into_iter()
    .map(|directory| build_dir.join(directory).display().to_string())
    .collect::<Vec<_>>()
    .join(":")
}

fn run_in_process_group(command: &mut Command) -> std::io::Result<ExitStatus> {
    command.process_group(0);
    let mut child = command.spawn()?;
    let child_pid = child.id() as libc::pid_t;
    let status = child.wait();

    unsafe {
        libc::kill(-child_pid, libc::SIGTERM);
        libc::kill(-child_pid, libc::SIGKILL);
    }

    status
}

#[test]
fn glibc_tests_with_built_libc() {
    let artifact = artifact();
    let build_dir = artifact
        .build_dir
        .as_ref()
        .expect("glibc artifact build directory");

    let mut command = Command::new("make");
    command
        .current_dir(build_dir)
        .arg("--silent")
        .arg("-k")
        .arg(format!("-j{}", jobs()))
        .arg("check")
        .arg("CC=gcc")
        .arg(format!(
            "test-wrapper-env=timeout 120s env LD_LIBRARY_PATH={}",
            glibc_build_library_path(build_dir)
        ))
        .env("TIMEOUTFACTOR", "1")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env_remove("LANGUAGE")
        .env_remove("LC_CTYPE")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if artifact.instrumentation == InstrumentationMode::Rsched {
        add_instrumented_link_args(&mut command, &artifact);
    }
    if artifact.provider == Provider::Coro {
        command.env("GLIBC_TUNABLES", "glibc.pthread.rseq=0");
    }

    let status = run_in_process_group(&mut command)
        .unwrap_or_else(|error| panic!("failed to run glibc tests: {error}"));
    let summary = summarize_results(build_dir);
    eprintln!("{summary}");
    assert!(
        status.success(),
        "glibc test suite failed with {status}\n{summary}"
    );
}

#[allow(dead_code)]
fn _assert_paths_are_paths(_: &Path, _: &PathBuf) {}
