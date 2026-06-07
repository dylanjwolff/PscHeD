use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static MUSL_LIB: OnceLock<PathBuf> = OnceLock::new();

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsched-musl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create musl test temp dir");
    dir
}

fn musl_static_lib() -> PathBuf {
    MUSL_LIB
        .get_or_init(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
            let target_dir = temp_dir().join("target");
            let status = Command::new(cargo)
                .current_dir(repo_root())
                .env("CARGO_TARGET_DIR", &target_dir)
                // Tell the cc crate to use musl-gcc for C files when targeting musl.
                .env("CC_x86_64_unknown_linux_musl", "musl-gcc")
                .args([
                    "build",
                    "-p",
                    "rsched",
                    "--target",
                    "x86_64-unknown-linux-musl",
                ])
                .status()
                .expect("spawn cargo build for musl rsched staticlib");
            assert!(
                status.success(),
                "cargo build --target x86_64-unknown-linux-musl failed"
            );

            let path = target_dir
                .join("x86_64-unknown-linux-musl")
                .join("debug")
                .join("librsched.a");
            assert!(
                path.exists(),
                "expected musl rsched staticlib at {}",
                path.display()
            );
            path
        })
        .clone()
}

fn build_musl_binary(lib: &Path) -> PathBuf {
    let root = repo_root();
    let out = temp_dir().join("musl_counter");
    let src = root.join("c-examples").join("interleavings.c");

    // GCC 13's libgcc_eh.a (Ubuntu 24.04) references _dl_find_object (glibc 2.35+),
    // which is absent in musl. Provide a stub; the function is only called during
    // stack unwinding, which never happens in a clean test run.
    let stub = temp_dir().join("musl_compat.c");
    std::fs::write(
        &stub,
        "int _dl_find_object(void *a, void *r) { (void)a; (void)r; return -1; }\n",
    )
    .expect("write _dl_find_object stub");

    let status = Command::new("musl-gcc")
        .args(["-g", "-Wall", "-Wextra", "-DRSCHED", "-DSTANDALONE_COUNTER"])
        .arg(format!("-I{}", root.join("include").display()))
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .arg(&stub)
        .args(["-Wl,--start-group"])
        .arg(lib)
        .args(["-Wl,--end-group", "-lpthread", "-ldl"])
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke musl-gcc: {e}"));
    assert!(
        status.success(),
        "failed to build interleavings.c counter variant with musl-gcc"
    );

    out
}

#[test]
fn musl_counter() {
    let lib = musl_static_lib();
    let binary = build_musl_binary(&lib);

    let out = Command::new(&binary)
        .output()
        .unwrap_or_else(|e| panic!("failed to run musl_counter: {e}"));

    assert!(
        out.status.success(),
        "musl_counter exited with {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
