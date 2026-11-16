fn main() {
    // Compile all C example files into a single static archive (libcexamples.a).
    // Cargo automatically emits `cargo:rustc-link-lib=static=cexamples` and
    // places the archive where integration-test binaries can find it.
    cc::Build::new()
        .file("c-examples/interleavings.c")
        .file("c-examples/benchmarks.c")
        .include("include")
        .define("RSCHED", None)
        .compile("cexamples");

    println!("cargo:rerun-if-changed=c-examples/");
    println!("cargo:rerun-if-changed=include/");

    // Required at link time for pthread primitives used by both the C examples
    // and the scheduler itself.
    println!("cargo:rustc-link-lib=pthread");

    // Syntax check: compile each example *without* -DRSCHED so that the code
    // is compiled against the real <stdatomic.h>.  Nothing is linked; the
    // object files are written to OUT_DIR and discarded.  This build step
    // fails fast if the standard-C11 syntax ever drifts from what rsched
    // expects.
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let compiler = cc::Build::new().include("include").get_compiler();

    let c_files = ["c-examples/interleavings.c", "c-examples/benchmarks.c"];
    for (i, file) in c_files.iter().enumerate() {
        let obj = format!("{}/stdatomic_check_{}.o", out_dir, i);
        let status = compiler
            .to_command()
            .args(["-c", "-Iinclude", "-o", obj.as_str(), file])
            .status()
            .unwrap_or_else(|e| panic!("failed to invoke C compiler: {e}"));
        assert!(
            status.success(),
            "{file} does not compile without -DRSCHED (real <stdatomic.h>)"
        );
    }
}
