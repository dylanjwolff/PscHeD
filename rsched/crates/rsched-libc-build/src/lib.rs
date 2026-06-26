use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Native,
    Coro,
}

impl Provider {
    pub const ALL: [Self; 2] = [Self::Native, Self::Coro];

    pub fn feature(self) -> Option<&'static str> {
        match self {
            Self::Native => None,
            Self::Coro => Some("coro"),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native => f.write_str("native"),
            Self::Coro => f.write_str("coro"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Static,
    Glibc,
    Musl,
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static => f.write_str("static"),
            Self::Glibc => f.write_str("glibc"),
            Self::Musl => f.write_str("musl"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildProfile {
    Debug,
    Release,
}

impl BuildProfile {
    fn cargo_arg(self) -> Option<&'static str> {
        match self {
            Self::Debug => None,
            Self::Release => Some("--release"),
        }
    }

    fn directory(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

impl fmt::Display for BuildProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.directory())
    }
}

#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub workspace_root: PathBuf,
    pub cache_root: PathBuf,
    pub kind: ArtifactKind,
    pub provider: Provider,
    pub profile: BuildProfile,
}

impl BuildOptions {
    pub fn new(workspace_root: impl Into<PathBuf>, kind: ArtifactKind, provider: Provider) -> Self {
        let workspace_root = workspace_root.into();
        let cache_root = workspace_root.join("target/rsched-artifacts");
        Self {
            workspace_root,
            cache_root,
            kind,
            provider,
            profile: BuildProfile::Release,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArtifactManifest {
    pub schema: u32,
    pub kind: ArtifactKind,
    pub provider: Provider,
    pub profile: BuildProfile,
    pub target: String,
    pub fingerprint: String,
    pub root: PathBuf,
    pub static_library: Option<PathBuf>,
    pub llvm_plugin: PathBuf,
    #[serde(default)]
    pub gcc_plugin: Option<PathBuf>,
    pub libc: Option<PathBuf>,
    pub loader: Option<PathBuf>,
    pub library_path: Option<String>,
    pub runner: Option<PathBuf>,
    pub build_dir: Option<PathBuf>,
    pub install_dir: Option<PathBuf>,
    pub compiler: Option<PathBuf>,
    pub rsched_library: Option<PathBuf>,
    pub asan_runtime: Option<PathBuf>,
}

impl ArtifactManifest {
    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(self.manifest_path())
    }

    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let mut file =
            fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let manifest_path = if path.is_dir() {
            path.join("manifest.json")
        } else {
            path.to_path_buf()
        };
        let mut manifest: Self = serde_json::from_slice(
            &fs::read(&manifest_path)
                .with_context(|| format!("read {}", manifest_path.display()))?,
        )?;
        if manifest.schema != 1 {
            bail!(
                "unsupported rsched artifact schema {} in {}",
                manifest.schema,
                manifest_path.display()
            );
        }
        if !manifest.root.is_absolute() {
            let root = manifest_path
                .parent()
                .context("artifact manifest has no parent directory")?
                .join(&manifest.root);
            manifest.root = fs::canonicalize(&root).unwrap_or(root);
        }
        manifest.absolutize_paths();
        manifest.validate()?;
        Ok(manifest)
    }

    fn absolutize_paths(&mut self) {
        let root = self.root.clone();
        for path in [
            &mut self.static_library,
            &mut self.gcc_plugin,
            &mut self.libc,
            &mut self.loader,
            &mut self.runner,
            &mut self.build_dir,
            &mut self.install_dir,
            &mut self.compiler,
            &mut self.rsched_library,
            &mut self.asan_runtime,
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_absolute() {
                *path = root.join(&*path);
            }
        }
        if !self.llvm_plugin.is_absolute() {
            self.llvm_plugin = root.join(&self.llvm_plugin);
        }
    }

    fn validate(&self) -> Result<()> {
        for path in [
            Some(&self.llvm_plugin),
            self.gcc_plugin.as_ref(),
            self.static_library.as_ref(),
            self.libc.as_ref(),
            self.loader.as_ref(),
            self.runner.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !path.exists() {
                bail!("artifact is incomplete: {} is missing", path.display());
            }
        }
        Ok(())
    }
}

pub fn ensure_artifact(options: &BuildOptions) -> Result<ArtifactManifest> {
    let fingerprint = fingerprint(options)?;
    let artifact_root = options
        .cache_root
        .join(options.kind.to_string())
        .join(options.provider.to_string())
        .join(options.profile.to_string())
        .join(&fingerprint);
    let manifest_path = artifact_root.join("manifest.json");
    if manifest_path.exists() {
        return ArtifactManifest::load(manifest_path);
    }

    if artifact_root.exists() {
        fs::remove_dir_all(&artifact_root)
            .with_context(|| format!("remove incomplete {}", artifact_root.display()))?;
    }
    fs::create_dir_all(&artifact_root)
        .with_context(|| format!("create {}", artifact_root.display()))?;

    let plugin = ensure_llvm_plugin(options, &fingerprint)?;
    let mut manifest = match options.kind {
        ArtifactKind::Static => build_static(options, &artifact_root, &plugin, &fingerprint)?,
        ArtifactKind::Glibc => {
            let gcc_plugin = ensure_gcc_plugin(options, &fingerprint)?;
            build_glibc(options, &artifact_root, &plugin, &gcc_plugin, &fingerprint)?
        }
        ArtifactKind::Musl => build_musl(options, &artifact_root, &plugin, &fingerprint)?,
    };
    manifest.root = PathBuf::from(".");
    relativize_manifest(&mut manifest, &artifact_root);
    manifest.save_to(&manifest_path)?;
    ArtifactManifest::load(artifact_root)
}

fn ensure_llvm_plugin(options: &BuildOptions, artifact_fingerprint: &str) -> Result<PathBuf> {
    let component_root = options
        .cache_root
        .join("components/llvm-pass")
        .join(options.profile.to_string())
        .join(plugin_fingerprint(options, artifact_fingerprint)?);
    let plugin = component_root
        .join(options.profile.directory())
        .join("librsched_llvm_pass.so");
    if plugin.exists() {
        return Ok(plugin);
    }
    fs::create_dir_all(&component_root)?;
    let mut command = cargo(options);
    command
        .current_dir(&options.workspace_root)
        .env("CARGO_TARGET_DIR", &component_root)
        .args(["build", "-p", "rsched-llvm-pass"]);
    if let Some(arg) = options.profile.cargo_arg() {
        command.arg(arg);
    }
    run(command, "build the rsched LLVM pass")?;
    if !plugin.exists() {
        bail!("expected LLVM plugin at {}", plugin.display());
    }
    Ok(plugin)
}

fn ensure_gcc_plugin(options: &BuildOptions, artifact_fingerprint: &str) -> Result<PathBuf> {
    let component_root = options
        .cache_root
        .join("components/gcc-pass")
        .join(options.profile.to_string())
        .join(gcc_plugin_fingerprint(options, artifact_fingerprint)?);
    let plugin = component_root.join("rsched_gcc_pass.so");
    if plugin.exists() {
        return Ok(plugin);
    }

    fs::create_dir_all(&component_root)?;
    let include_dir = gcc_plugin_include_dir()?;
    let source = options.workspace_root.join("gcc-pass/rsched_gcc_pass.cc");
    let mut command = Command::new("g++");
    command
        .arg("-fPIC")
        .arg("-shared")
        .arg("-fno-rtti")
        .arg("-O2")
        .arg("-std=gnu++17")
        .arg(format!("-I{}", include_dir.display()))
        .arg(&source)
        .arg("-o")
        .arg(&plugin);
    run(command, "build the rsched GCC pass")?;
    if !plugin.exists() {
        bail!("expected GCC plugin at {}", plugin.display());
    }
    Ok(plugin)
}

fn build_static(
    options: &BuildOptions,
    root: &Path,
    plugin: &Path,
    fingerprint: &str,
) -> Result<ArtifactManifest> {
    let target_dir = root.join("cargo-target");
    let mut command = cargo(options);
    command
        .current_dir(&options.workspace_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "-p", "rsched", "--lib"]);
    if let Some(arg) = options.profile.cargo_arg() {
        command.arg(arg);
    }
    if let Some(feature) = options.provider.feature() {
        command.args(["--features", feature]);
    }
    run(command, "build the rsched static library")?;

    let runtime = root.join("runtime");
    fs::create_dir_all(runtime.join("include"))?;
    let static_library = runtime.join("librsched.a");
    fs::copy(
        target_dir
            .join(options.profile.directory())
            .join("librsched.a"),
        &static_library,
    )?;
    fs::copy(
        options.workspace_root.join("include/rsched.h"),
        runtime.join("include/rsched.h"),
    )?;
    fs::copy(
        options.workspace_root.join("include/rsched_atomic.h"),
        runtime.join("include/rsched_atomic.h"),
    )?;
    let packaged_plugin = runtime.join("librsched_llvm_pass.so");
    fs::copy(plugin, &packaged_plugin)?;

    Ok(base_manifest(
        options,
        root,
        fingerprint,
        packaged_plugin,
        Some(static_library),
    ))
}

fn build_glibc(
    options: &BuildOptions,
    root: &Path,
    plugin: &Path,
    gcc_plugin: &Path,
    fingerprint: &str,
) -> Result<ArtifactManifest> {
    require_program("gcc")?;
    require_program("g++")?;
    require_program("make")?;
    require_program("objcopy")?;
    require_program("ld")?;

    let cargo_target = root.join("rsched-target");
    let rsched = build_embedded_rsched(options, &cargo_target, None)?;
    let build_dir = root.join("build");
    let install_dir = root.join("install");
    fs::create_dir_all(&build_dir)?;

    let compiler_driver = options.workspace_root.join("glibc/instrument-gcc.sh");
    let configure = options.workspace_root.join("glibc/glibc/configure");
    let mut command = Command::new(configure);
    command
        .current_dir(&build_dir)
        .env("RSCHED_GCC_PLUGIN", gcc_plugin)
        .env("RSCHED_WORKSPACE", &options.workspace_root)
        .env("CC", &compiler_driver)
        .env("CFLAGS", "-g -O2")
        .env("CXXFLAGS", "-g -O2")
        .env_remove("CPPFLAGS")
        .arg(format!("--prefix={}", install_dir.display()))
        .arg("--disable-static")
        .arg("--disable-werror");
    run(command, "configure instrumented glibc")?;

    let jobs = jobs();
    let shared_gnulib = format!("{} -lgcc_s -lgcc", rsched.display());
    let mut command = Command::new("make");
    command
        .current_dir(&build_dir)
        .env("RSCHED_GCC_PLUGIN", gcc_plugin)
        .env("RSCHED_WORKSPACE", &options.workspace_root)
        .arg("--silent")
        .arg(format!("-j{jobs}"))
        .arg(build_dir.join("versions.stmp"));
    run(command, "generate glibc symbol version maps")?;
    export_glibc_hooks(&build_dir.join("libc.map"))?;

    let mut command = Command::new("make");
    command
        .current_dir(&build_dir)
        .env("RSCHED_GCC_PLUGIN", gcc_plugin)
        .env("RSCHED_WORKSPACE", &options.workspace_root)
        .arg("--silent")
        .arg(format!("-j{jobs}"));
    add_glibc_link_args(&mut command, &shared_gnulib);
    command.arg("lib");
    run(command, "build instrumented glibc libraries")?;

    for (subdir, library, soname) in [
        ("math", "libm.so", "libm.so.6"),
        ("resolv", "libresolv.so", "libresolv.so.2"),
    ] {
        let library = build_dir.join(subdir).join(library);
        let mut command = Command::new("make");
        command
            .current_dir(options.workspace_root.join("glibc/glibc").join(subdir))
            .env("RSCHED_GCC_PLUGIN", gcc_plugin)
            .env("RSCHED_WORKSPACE", &options.workspace_root)
            .arg("--silent")
            .arg(format!("-j{jobs}"));
        add_glibc_link_args(&mut command, &shared_gnulib);
        command
            .arg(format!("subdir={subdir}"))
            .arg("..=../")
            .arg(format!("objdir={}", build_dir.display()))
            .arg(&library);
        run(command, &format!("build instrumented glibc {library:?}"))?;
        let soname = build_dir.join(subdir).join(soname);
        if !soname.exists() {
            symlink(
                library.file_name().context("glibc library file name")?,
                &soname,
            )?;
        }
    }

    let support_archive = build_dir.join("support/libsupport_nonshared.a");
    let mut command = Command::new("make");
    command
        .current_dir(options.workspace_root.join("glibc/glibc/support"))
        .env("RSCHED_GCC_PLUGIN", gcc_plugin)
        .env("RSCHED_WORKSPACE", &options.workspace_root)
        .arg("--silent")
        .arg(format!("-j{jobs}"))
        .arg("subdir=support")
        .arg("..=../")
        .arg(format!("objdir={}", build_dir.display()))
        .arg(&support_archive);
    run(command, "build the glibc test support library")?;

    copy_compiler_library("gcc", "libgcc_s.so.1", &build_dir)?;
    let asan_runtime = if options.provider == Provider::Native {
        Some(prepare_clang_asan_runtime(options, root, &build_dir)?)
    } else {
        None
    };

    let runtime = root.join("runtime");
    let runtime_lib = runtime.join("lib");
    fs::create_dir_all(&runtime_lib)?;
    let loader = runtime.join("ld-linux-x86-64.so.2");
    fs::copy(build_dir.join("elf/ld-linux-x86-64.so.2"), &loader)?;
    let libc = runtime_lib.join("libc.so.6");
    fs::copy(build_dir.join("libc.so"), &libc)?;
    for (source, name) in [
        (build_dir.join("math/libm.so"), "libm.so.6"),
        (build_dir.join("resolv/libresolv.so"), "libresolv.so.2"),
        (build_dir.join("libgcc_s.so.1"), "libgcc_s.so.1"),
    ] {
        fs::copy(source, runtime_lib.join(name))?;
    }
    if let Some(path) = &asan_runtime {
        fs::copy(
            path,
            runtime_lib.join(path.file_name().context("ASAN runtime file name")?),
        )?;
        fs::copy(
            build_dir.join("libstdc++.so.6"),
            runtime_lib.join("libstdc++.so.6"),
        )?;
    }
    let packaged_plugin = runtime.join("librsched_llvm_pass.so");
    fs::copy(plugin, &packaged_plugin)?;
    let packaged_gcc_plugin = runtime.join("rsched_gcc_pass.so");
    fs::copy(gcc_plugin, &packaged_gcc_plugin)?;
    let runner = runtime.join("run");
    write_glibc_runner(&runner)?;

    let library_paths = glibc_build_library_paths(&build_dir);
    let mut manifest = base_manifest(options, root, fingerprint, packaged_plugin, None);
    manifest.libc = Some(libc);
    manifest.loader = Some(loader);
    manifest.library_path = Some(library_paths);
    manifest.runner = Some(runner);
    manifest.build_dir = Some(build_dir);
    manifest.install_dir = Some(install_dir);
    manifest.rsched_library = Some(rsched);
    manifest.gcc_plugin = Some(packaged_gcc_plugin);
    manifest.asan_runtime = asan_runtime.map(|path| runtime_lib.join(path.file_name().unwrap()));
    Ok(manifest)
}

fn build_musl(
    options: &BuildOptions,
    root: &Path,
    plugin: &Path,
    fingerprint: &str,
) -> Result<ArtifactManifest> {
    require_program("clang-17")?;
    require_program("make")?;
    require_program("musl-gcc")?;
    require_program("opt-17")?;

    let cargo_target = root.join("rsched-target");
    let rsched = build_embedded_rsched(options, &cargo_target, Some("x86_64-unknown-linux-musl"))?;
    let build_dir = root.join("build");
    let install_dir = root.join("install");
    fs::create_dir_all(&build_dir)?;

    let compiler_driver = options.workspace_root.join("musl/instrument-clang.sh");
    let configure = options.workspace_root.join("musl/musl/configure");
    let mut command = Command::new(configure);
    command
        .current_dir(&build_dir)
        .env("RSCHED_LLVM_PLUGIN", plugin)
        .env("RSCHED_WORKSPACE", &options.workspace_root)
        .env(
            "LDFLAGS",
            "-Wl,--export-dynamic-symbol=rsched_atomic_instrument \
             -Wl,--export-dynamic-symbol=rsched_atomic_instrument_ra \
             -Wl,--export-dynamic-symbol=rsched_fuzzer_test_one_input",
        )
        .arg(format!("--prefix={}", install_dir.display()))
        .arg(format!("--syslibdir={}/lib", install_dir.display()))
        .env("CC", &compiler_driver);
    run(command, "configure instrumented musl")?;

    let libcc = format!("{} -lgcc -lgcc_eh", rsched.display());
    let mut command = Command::new("make");
    command
        .current_dir(&build_dir)
        .env("RSCHED_LLVM_PLUGIN", plugin)
        .env("RSCHED_WORKSPACE", &options.workspace_root)
        .arg(format!("-j{}", jobs()))
        .arg(format!("LIBCC={libcc}"));
    run(command, "build instrumented musl")?;

    let mut command = Command::new("make");
    command
        .current_dir(&build_dir)
        .env("RSCHED_LLVM_PLUGIN", plugin)
        .env("RSCHED_WORKSPACE", &options.workspace_root)
        .arg("install");
    run(command, "install instrumented musl")?;

    fs::copy(&rsched, install_dir.join("lib/librsched.a"))?;
    let compiler = install_dir.join("bin/musl-clang");
    if !compiler.exists() {
        bail!(
            "expected instrumented musl compiler at {}",
            compiler.display()
        );
    }
    patch_musl_compiler_for_static_rsched(&compiler)?;

    let runtime = root.join("runtime");
    let runtime_lib = runtime.join("lib");
    fs::create_dir_all(&runtime_lib)?;
    let libc = runtime_lib.join("libc.so");
    let loader = runtime.join("ld-musl-x86_64.so.1");
    fs::copy(install_dir.join("lib/libc.so"), &libc)?;
    fs::copy(install_dir.join("lib/ld-musl-x86_64.so.1"), &loader)?;
    let packaged_plugin = runtime.join("librsched_llvm_pass.so");
    fs::copy(plugin, &packaged_plugin)?;
    let runner = runtime.join("run");
    write_musl_runner(&runner)?;

    let mut manifest = base_manifest(options, root, fingerprint, packaged_plugin, None);
    manifest.libc = Some(libc);
    manifest.loader = Some(loader);
    manifest.library_path = Some(runtime_lib.display().to_string());
    manifest.runner = Some(runner);
    manifest.build_dir = Some(build_dir);
    manifest.install_dir = Some(install_dir);
    manifest.compiler = Some(compiler);
    manifest.rsched_library = Some(rsched);
    Ok(manifest)
}

fn patch_musl_compiler_for_static_rsched(compiler: &Path) -> Result<()> {
    let script = fs::read_to_string(compiler)
        .with_context(|| format!("read musl compiler wrapper {}", compiler.display()))?;
    if script.contains("librsched.a") {
        return Ok(());
    }
    let script = script.replace("sflags=\neflags=\n", "sflags=\neflags=\nstatic_rsched=\n");
    let script = script.replace(
        "    case \"$x\" in\n        -l*) input=1 ;;\n        *) input= ;;\n    esac\n",
        "    case \"$x\" in\n        -static|--static) static_rsched=\"-Wl,--whole-archive $libc_lib/librsched.a -Wl,--no-whole-archive\" ;;\n    esac\n    case \"$x\" in\n        -l*) input=1 ;;\n        *) input= ;;\n    esac\n",
    );
    let script = script.replace(
        "    \"$@\" \\\n    $eflags \\\n",
        "    \"$@\" \\\n    $static_rsched \\\n    $eflags \\\n",
    );
    fs::write(compiler, script)
        .with_context(|| format!("patch musl compiler wrapper {}", compiler.display()))?;
    Ok(())
}

fn build_embedded_rsched(
    options: &BuildOptions,
    target_dir: &Path,
    target: Option<&str>,
) -> Result<PathBuf> {
    let mut features = vec!["instrumented-libc"];
    if let Some(feature) = options.provider.feature() {
        features.push(feature);
    }
    let mut command = cargo(options);
    command
        .current_dir(&options.workspace_root)
        .env("CARGO_TARGET_DIR", target_dir)
        .env(
            "RUSTFLAGS",
            append_env_flag(std::env::var_os("RUSTFLAGS"), "-C panic=abort"),
        )
        .args(["build", "-p", "rsched", "--features", &features.join(",")]);
    if let Some(arg) = options.profile.cargo_arg() {
        command.arg(arg);
    }
    if let Some(target) = target {
        command
            .env("CC_x86_64_unknown_linux_musl", "musl-gcc")
            .args(["--target", target]);
    }
    run(command, "build rsched for instrumented libc")?;
    let mut path = target_dir.to_path_buf();
    if let Some(target) = target {
        path.push(target);
    }
    path.push(options.profile.directory());
    path.push("librsched.a");
    if !path.exists() {
        bail!("expected rsched static library at {}", path.display());
    }
    Ok(path)
}

fn base_manifest(
    options: &BuildOptions,
    root: &Path,
    fingerprint: &str,
    plugin: PathBuf,
    static_library: Option<PathBuf>,
) -> ArtifactManifest {
    ArtifactManifest {
        schema: 1,
        kind: options.kind,
        provider: options.provider,
        profile: options.profile,
        target: "x86_64-unknown-linux-gnu".to_owned(),
        fingerprint: fingerprint.to_owned(),
        root: root.to_path_buf(),
        static_library,
        llvm_plugin: plugin,
        gcc_plugin: None,
        libc: None,
        loader: None,
        library_path: None,
        runner: None,
        build_dir: None,
        install_dir: None,
        compiler: None,
        rsched_library: None,
        asan_runtime: None,
    }
}

fn relativize_manifest(manifest: &mut ArtifactManifest, root: &Path) {
    for path in [
        &mut manifest.static_library,
        &mut manifest.gcc_plugin,
        &mut manifest.libc,
        &mut manifest.loader,
        &mut manifest.runner,
        &mut manifest.build_dir,
        &mut manifest.install_dir,
        &mut manifest.compiler,
        &mut manifest.rsched_library,
        &mut manifest.asan_runtime,
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(relative) = path.strip_prefix(root) {
            *path = relative.to_path_buf();
        }
    }
    if let Ok(relative) = manifest.llvm_plugin.strip_prefix(root) {
        manifest.llvm_plugin = relative.to_path_buf();
    }
}

fn fingerprint(options: &BuildOptions) -> Result<String> {
    let mut hash = StableHash::new();
    hash.add(options.kind.to_string().as_bytes());
    hash.add(options.provider.to_string().as_bytes());
    hash.add(options.profile.to_string().as_bytes());
    hash.add(std::env::consts::ARCH.as_bytes());
    hash.add(std::env::consts::OS.as_bytes());

    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "src",
        "include",
        "c-examples",
        "libc-instrumentation",
        "crates/rsched-libc-build/Cargo.toml",
        "crates/rsched-libc-build/src",
        "llvm-pass/Cargo.toml",
        "llvm-pass/src",
        "gcc-pass",
    ] {
        hash_path(
            &mut hash,
            &options.workspace_root.join(path),
            &options.workspace_root,
        )?;
    }
    match options.kind {
        ArtifactKind::Static => {}
        ArtifactKind::Glibc => {
            hash_path(
                &mut hash,
                &options.workspace_root.join("glibc/instrument-gcc.sh"),
                &options.workspace_root,
            )?;
            hash_source_checkout(
                &mut hash,
                &options.workspace_root.join("glibc/glibc"),
                "glibc",
            )?;
        }
        ArtifactKind::Musl => {
            hash_path(
                &mut hash,
                &options.workspace_root.join("musl/instrument-clang.sh"),
                &options.workspace_root,
            )?;
            hash_source_checkout(&mut hash, &options.workspace_root.join("musl/musl"), "musl")?;
        }
    }
    for program in ["cargo", "rustc", "clang-17", "opt-17", "gcc", "ld"] {
        hash.add(program.as_bytes());
        hash.add(&program_version(program));
    }
    Ok(hash.finish())
}

fn plugin_fingerprint(options: &BuildOptions, fallback: &str) -> Result<String> {
    let mut hash = StableHash::new();
    for path in ["Cargo.lock", "llvm-pass"] {
        hash_path(
            &mut hash,
            &options.workspace_root.join(path),
            &options.workspace_root,
        )?;
    }
    hash.add(options.profile.to_string().as_bytes());
    hash.add(&program_version("rustc"));
    hash.add(&program_version("llvm-config-17"));
    let value = hash.finish();
    Ok(if value.is_empty() {
        fallback.to_owned()
    } else {
        value
    })
}

fn gcc_plugin_fingerprint(options: &BuildOptions, fallback: &str) -> Result<String> {
    let mut hash = StableHash::new();
    for path in ["gcc-pass", "libc-instrumentation/instrument-gcc.sh"] {
        hash_path(
            &mut hash,
            &options.workspace_root.join(path),
            &options.workspace_root,
        )?;
    }
    hash.add(options.profile.to_string().as_bytes());
    hash.add(&program_version("gcc"));
    hash.add(&program_version("g++"));
    let value = hash.finish();
    Ok(if value.is_empty() {
        fallback.to_owned()
    } else {
        value
    })
}

fn hash_path(hash: &mut StableHash, path: &Path, root: &Path) -> Result<()> {
    if path.is_file() {
        hash_file(hash, path, root)?;
        return Ok(());
    }
    let mut files = WalkDir::new(path)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some("target" | ".git" | "autom4te.cache")
            )
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    for file in files {
        hash_file(hash, &file, root)?;
    }
    Ok(())
}

fn hash_file(hash: &mut StableHash, path: &Path, root: &Path) -> Result<()> {
    hash.add(
        path.strip_prefix(root)
            .unwrap_or(path)
            .as_os_str()
            .as_encoded_bytes(),
    );
    hash.add(&fs::read(path).with_context(|| format!("read {}", path.display()))?);
    Ok(())
}

fn hash_source_checkout(hash: &mut StableHash, checkout: &Path, label: &str) -> Result<()> {
    hash.add(label.as_bytes());
    let root = checkout
        .parent()
        .context("libc source checkout has no parent")?;
    hash_path(hash, checkout, root)
}

struct StableHash {
    first: u64,
    second: u64,
}

impl StableHash {
    fn new() -> Self {
        Self {
            first: 0xcbf29ce484222325,
            second: 0x84222325cbf29ce4,
        }
    }

    fn add(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.first ^= u64::from(byte);
            self.first = self.first.wrapping_mul(0x100000001b3);
            self.second ^= u64::from(byte).rotate_left(1);
            self.second = self.second.wrapping_mul(0x100000001b3);
        }
        self.first ^= 0xff;
        self.second ^= 0x7f;
    }

    fn finish(&self) -> String {
        format!("{:016x}{:016x}", self.first, self.second)
    }
}

fn prepare_clang_asan_runtime(
    options: &BuildOptions,
    root: &Path,
    build_dir: &Path,
) -> Result<PathBuf> {
    let libasan = compiler_library("clang", "libclang_rt.asan-x86_64.so")?;
    let mut command = Command::new("objdump");
    command.args(["-p"]).arg(&libasan);
    let output = run(command, "read the Clang ASAN runtime SONAME")?;
    let output = String::from_utf8(output.stdout)?;
    let soname = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("SONAME").map(str::trim))
        .context("find Clang ASAN runtime SONAME")?;
    let asan_runtime = build_dir.join(soname);
    fs::copy(&libasan, &asan_runtime)?;

    let libsupcxx = compiler_library("g++", "libsupc++.a")?;
    let cxxabi_map = root.join("asan-cxxabi.map");
    fs::write(
        &cxxabi_map,
        "CXXABI_1.3 {\n\
         global:\n\
           __cxa_*;\n\
           __dynamic_cast;\n\
           _ZTIN10__cxxabiv1*;\n\
           _ZTSN10__cxxabiv1*;\n\
         };\n\
         GLIBCXX_3.4 {\n\
         global:\n\
           _ZTISt9type_info;\n\
           _ZTSSt9type_info;\n\
         local: *;\n\
         } CXXABI_1.3;\n",
    )?;
    let mut command = Command::new("clang-17");
    command
        .args(["-shared", "-static-libgcc"])
        .arg("-Wl,-soname,libstdc++.so.6")
        .arg(format!("-Wl,--version-script={}", cxxabi_map.display()))
        .arg("-Wl,--whole-archive")
        .arg(&libsupcxx)
        .arg("-Wl,--no-whole-archive")
        .arg("-o")
        .arg(build_dir.join("libstdc++.so.6"));
    run(command, "build the minimal ASAN C++ ABI library")?;
    let _ = options;
    Ok(asan_runtime)
}

fn export_glibc_hooks(libc_map: &Path) -> Result<()> {
    let map = fs::read_to_string(libc_map)?;
    if map.contains("rsched_atomic_instrument;") {
        return Ok(());
    }
    let marker = "  local:\n    *;\n};";
    let replacement = "    rsched_atomic_instrument;\n\
                       rsched_atomic_instrument_ra;\n\
                       rsched_fuzzer_test_one_input;\n\
                       local:\n    *;\n};";
    let index = map
        .rfind(marker)
        .context("find final GLIBC_PRIVATE local block in libc.map")?;
    let mut output = map;
    output.replace_range(index..index + marker.len(), replacement);
    fs::write(libc_map, output)?;
    Ok(())
}

fn add_glibc_link_args(command: &mut Command, shared_gnulib: &str) {
    command
        .arg(format!("libc.so-gnulib={shared_gnulib}"))
        .arg(format!("gnulib={shared_gnulib}"))
        .arg(format!("gnulib-tests={shared_gnulib}"))
        .arg("static-gnulib=-lgcc -lgcc_eh")
        .arg("static-gnulib-tests=-lgcc -lgcc_eh");
}

fn glibc_build_library_paths(build_dir: &Path) -> String {
    [
        "", "math", "elf", "dlfcn", "nss", "nis", "rt", "resolv", "mathvec", "support", "crypt",
        "nptl",
    ]
    .into_iter()
    .map(|directory| build_dir.join(directory).display().to_string())
    .collect::<Vec<_>>()
    .join(":")
}

fn write_glibc_runner(path: &Path) -> Result<()> {
    fs::write(
        path,
        "#!/bin/sh\n\
         set -eu\n\
         root=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n\
         library_path=\"$root/lib\"\n\
         if [ -n \"${RSCHED_EXTRA_LIBRARY_PATH:-}\" ]; then\n\
           library_path=\"$RSCHED_EXTRA_LIBRARY_PATH:$library_path\"\n\
         fi\n\
         preload=\"$root/lib/libc.so.6\"\n\
         if [ -n \"${RSCHED_EXTRA_PRELOAD:-}\" ]; then\n\
           preload=\"$RSCHED_EXTRA_PRELOAD:$preload\"\n\
         fi\n\
         if [ -n \"${RSCHED_DIRECT_EXEC:-}\" ]; then\n\
           GLIBC_TUNABLES='glibc.pthread.rseq=0' LD_PRELOAD=\"$preload\" \
             LD_LIBRARY_PATH=\"$library_path\" exec \"$@\"\n\
         fi\n\
         GLIBC_TUNABLES='glibc.pthread.rseq=0' LD_PRELOAD=\"$preload\" \
           exec \"$root/ld-linux-x86-64.so.2\" --library-path \"$library_path\" \"$@\"\n",
    )?;
    executable(path)
}

fn write_musl_runner(path: &Path) -> Result<()> {
    fs::write(
        path,
        "#!/bin/sh\n\
         set -eu\n\
         root=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n\
         LD_PRELOAD=\"$root/lib/libc.so\" \
           exec \"$root/ld-musl-x86_64.so.1\" --library-path \"$root/lib\" \"$@\"\n",
    )?;
    executable(path)
}

fn executable(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn copy_compiler_library(program: &str, name: &str, destination: &Path) -> Result<PathBuf> {
    let source = compiler_library(program, name)?;
    let output = destination.join(name);
    fs::copy(source, &output)?;
    Ok(output)
}

fn gcc_plugin_include_dir() -> Result<PathBuf> {
    let mut command = Command::new("gcc");
    command.arg("-print-file-name=plugin");
    let output = run(command, "locate the GCC plugin directory")?;
    let plugin_dir = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    let include_dir = plugin_dir.join("include");
    if !include_dir.join("gcc-plugin.h").is_file() {
        bail!(
            "GCC plugin headers are missing at {}; install gcc-plugin-dev for the selected GCC",
            include_dir.display()
        );
    }
    Ok(include_dir)
}

fn compiler_library(program: &str, name: &str) -> Result<PathBuf> {
    let mut command = Command::new(program);
    command.arg(format!("--print-file-name={name}"));
    let output = run(command, &format!("locate {name}"))?;
    let path = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    if !path.is_file() {
        bail!("{program} did not locate {name}");
    }
    Ok(path)
}

fn require_program(name: &str) -> Result<()> {
    let status = Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("{name} is required"))?;
    if !status.success() {
        bail!("{name} --version failed");
    }
    Ok(())
}

fn jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .min(4)
}

fn cargo(options: &BuildOptions) -> Command {
    let _ = options;
    Command::new(tool_program("cargo"))
}

fn tool_program(program: &str) -> OsString {
    let variable = program.to_ascii_uppercase();
    if let Some(path) = std::env::var_os(variable) {
        return path;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".cargo/bin").join(program);
        if path.is_file() {
            return path.into_os_string();
        }
    }
    OsString::from(program)
}

fn append_env_flag(current: Option<OsString>, extra: &str) -> OsString {
    let mut value = current.unwrap_or_default();
    if !value.is_empty() {
        value.push(" ");
    }
    value.push(extra);
    value
}

fn program_version(program: &str) -> Vec<u8> {
    Command::new(tool_program(program))
        .arg("--version")
        .output()
        .map(|output| {
            let mut bytes = output.stdout;
            bytes.extend(output.stderr);
            bytes
        })
        .unwrap_or_else(|error| error.to_string().into_bytes())
}

fn run(mut command: Command, description: &str) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("failed to {description}"))?;
    if !output.status.success() {
        bail!(
            "{description} failed with {}\nlast stdout lines:\n{}\nlast stderr lines:\n{}",
            output.status,
            tail(&output.stdout, 200),
            tail(&output.stderr, 200)
        );
    }
    Ok(output)
}

fn tail(bytes: &[u8], maximum_lines: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines = text.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(maximum_lines)..].join("\n")
}

pub fn package_artifact(
    manifest: &ArtifactManifest,
    destination: impl AsRef<Path>,
) -> Result<PathBuf> {
    let destination = destination.as_ref();
    fs::create_dir_all(destination)?;
    let archive = destination.join(format!(
        "rsched-{}-{}-x86_64-linux.tar.zst",
        manifest.kind, manifest.provider
    ));
    let runtime = manifest.root.join("runtime");
    if !runtime.is_dir() {
        return Err(anyhow!(
            "artifact runtime directory is missing: {}",
            runtime.display()
        ));
    }
    let staging = destination.join(format!(
        ".rsched-package-{}-{}-{}",
        std::process::id(),
        manifest.kind,
        manifest.provider
    ));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    copy_directory(&runtime, &staging.join("runtime"))?;

    let mut release_manifest = manifest.clone();
    relativize_manifest(&mut release_manifest, &manifest.root);
    release_manifest.root = PathBuf::from(".");
    release_manifest.library_path = manifest.libc.as_ref().map(|_| "runtime/lib".to_owned());
    release_manifest.build_dir = None;
    release_manifest.install_dir = None;
    release_manifest.compiler = None;
    release_manifest.rsched_library = None;
    release_manifest.save_to(staging.join("manifest.json"))?;

    let mut command = Command::new("tar");
    command
        .arg("--zstd")
        .arg("-cf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .arg("runtime")
        .arg("manifest.json");
    let result = run(command, "package rsched artifact");
    let cleanup = fs::remove_dir_all(&staging);
    result?;
    cleanup?;
    Ok(archive)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in WalkDir::new(source).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let output = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&output)?;
        } else if entry.file_type().is_symlink() {
            symlink(fs::read_link(entry.path())?, &output)?;
        } else {
            fs::copy(entry.path(), &output)?;
        }
    }
    Ok(())
}
