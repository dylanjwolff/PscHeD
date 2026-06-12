FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# System dependencies:
#   build-essential  – CC toolchain for build.rs (compiles c-examples/*.c)
#   clang-17         – used by integration tests (sanitizer builds, seccomp, binary instrumentation)
#   llvm-17-dev      – provides llvm-config-17 and LLVM 17 libraries required by rsched-llvm-pass
#                      (llvm-plugin crate, features = ["llvm17-0"] → llvm-sys 170.x)
RUN apt-get update && apt-get install -y \
    build-essential \
    bison \
    ca-certificates \
    clang-17 \
    curl \
    gawk \
    llvm-17-dev \
    musl-tools \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Expose versioned tools under canonical names so build scripts and tests
# can invoke `clang` and `llvm-config` without hard-coding the suffix.
RUN update-alternatives --install /usr/bin/clang      clang      /usr/bin/clang-17      100 \
 && update-alternatives --install /usr/bin/llvm-config llvm-config /usr/bin/llvm-config-17 100

# Tell llvm-sys 170.x where the LLVM 17 installation lives.
ENV LLVM_SYS_170_PREFIX=/usr/lib/llvm-17

# Install Rust (stable; edition 2024 requires ≥ 1.85).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain nightly --no-modify-path
ENV PATH="/root/.cargo/bin:${PATH}"

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /workspace
COPY rsched/ .

CMD ["cargo", "test", "--workspace"]
