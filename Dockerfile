FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# System dependencies:
#   build-essential    – CC toolchain for build.rs (compiles c-examples/*.c)
#   gcc-13-plugin-dev  – GCC plugin headers used by the glibc instrumentation pass
#   clang-18           – used by integration tests (sanitizer builds, seccomp, binary instrumentation)
#   libclang-rt-18-dev – Clang sanitizer runtimes used by the sanitizer tests
#   llvm-18-dev        – provides llvm-config-18 and LLVM 18 libraries required by rsched-llvm-pass
#                        (llvm-plugin crate, features = ["llvm18-0"] → llvm-sys 180.x)
RUN apt-get update && apt-get install -y \
    build-essential \
    bison \
    ca-certificates \
    clang-18 \
    curl \
    gcc-13-plugin-dev \
    gawk \
    libclang-rt-18-dev \
    llvm-18-dev \
    musl-tools \
    pkg-config \
    zstd \
    && rm -rf /var/lib/apt/lists/*

# Expose versioned tools under canonical names so build scripts and tests
# can invoke `clang` and `llvm-config` without hard-coding the suffix.
RUN update-alternatives --install /usr/bin/clang      clang      /usr/bin/clang-18      100 \
 && update-alternatives --install /usr/bin/llvm-config llvm-config /usr/bin/llvm-config-18 100

# Tell llvm-sys 180.x where the LLVM 18 installation lives.
ENV LLVM_SYS_180_PREFIX=/usr/lib/llvm-18

# Install Rust nightly (rsched uses unstable features such as link_llvm_intrinsics).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain nightly --no-modify-path
ENV PATH="/root/.cargo/bin:${PATH}"

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /workspace
COPY rsched/ .

CMD ["cargo", "xtask", "test", "source", "--provider", "all"]
