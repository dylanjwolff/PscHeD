fn main() {
    // Compile all C example files into a single static archive (libcexamples.a).
    // Cargo automatically emits `cargo:rustc-link-lib=static=cexamples` and
    // places the archive where integration-test binaries can find it.
    cc::Build::new()
        .file("c-examples/uniform.c")
        .file("c-examples/uniform-lock.c")
        .file("c-examples/uaf.c")
        .include("include")
        .compile("cexamples");

    println!("cargo:rerun-if-changed=c-examples/");
    println!("cargo:rerun-if-changed=include/");

    // Required at link time for pthread primitives used by both the C examples
    // and the scheduler itself.
    println!("cargo:rustc-link-lib=pthread");
}
