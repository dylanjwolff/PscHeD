use anyhow::{Context, Result, bail};
use rsched_libc_build::{
    ArtifactKind, BuildOptions, BuildProfile, Provider, ensure_artifact, package_artifact,
};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> Result<()> {
    let root = workspace_root()?;
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        usage();
        bail!("missing xtask command");
    }
    if matches!(args[0].as_str(), "-h" | "--help" | "help") {
        usage();
        return Ok(());
    }

    let command = args.remove(0);
    match command.as_str() {
        "build" => build_command(&root, args),
        "test" => test_command(&root, args),
        "package" => package_command(&root, args),
        other => {
            usage();
            bail!("unknown xtask command {other:?}");
        }
    }
}

fn build_command(root: &Path, mut args: Vec<String>) -> Result<()> {
    let Some(target) = args.first().cloned() else {
        bail!("usage: cargo xtask build <static|libc|all> [...]");
    };
    args.remove(0);
    let libc = (target == "libc")
        .then(|| args_error("build libc requires <glibc|musl>", &mut args))
        .transpose()?;
    let providers = take_providers(&mut args)?;
    let profile = take_profile(&mut args)?;
    reject_extra(&args)?;

    let kinds = match target.as_str() {
        "static" => vec![ArtifactKind::Static],
        "libc" => vec![parse_libc(libc.as_deref().unwrap())?],
        "all" => vec![
            ArtifactKind::Static,
            ArtifactKind::Glibc,
            ArtifactKind::Musl,
        ],
        other => bail!("unknown build target {other:?}"),
    };

    for provider in providers {
        for kind in &kinds {
            let manifest = ensure(root, *kind, provider, profile)?;
            println!("{}", manifest.manifest_path().display());
        }
    }
    Ok(())
}

fn test_command(root: &Path, mut args: Vec<String>) -> Result<()> {
    let suite = args_error(
        "usage: cargo xtask test <source|libc|preload> [...]",
        &mut args,
    )?;
    let libc = matches!(suite.as_str(), "libc" | "preload")
        .then(|| args_error(&format!("test {suite} requires <glibc|musl>"), &mut args))
        .transpose()?;
    let providers = take_providers(&mut args)?;
    let profile = take_profile(&mut args)?;
    reject_extra(&args)?;

    match suite.as_str() {
        "source" => {
            for provider in providers {
                let mut command = cargo(root);
                command.current_dir(root).args([
                    "test",
                    "-p",
                    "rsched",
                    "--test",
                    "benchmarks",
                    "--test",
                    "sanitizers_static",
                    "--test",
                    "seccomp",
                    "--test",
                    "task_creation",
                    "--test",
                    "uaf",
                    "--test",
                    "uniform",
                    "--test",
                    "uniform_lock",
                ]);
                if provider == Provider::Coro {
                    command.args(["--features", "coro"]);
                }
                run(
                    command,
                    &format!("run source tests with {provider} provider"),
                )?;
            }
            let mut command = cargo(root);
            command
                .current_dir(root)
                .args(["test", "-p", "rsched-llvm-pass"]);
            run(command, "run LLVM pass tests")?;
        }
        "libc" | "preload" => {
            let libc = libc.as_deref().unwrap();
            let kind = parse_libc(libc)?;
            for provider in providers {
                let manifest = ensure(root, kind, provider, profile)?;
                let variable = match kind {
                    ArtifactKind::Glibc => "RSCHED_GLIBC_ARTIFACT",
                    ArtifactKind::Musl => "RSCHED_MUSL_ARTIFACT",
                    ArtifactKind::Static => unreachable!(),
                };
                let test = match (suite.as_str(), kind) {
                    ("libc", ArtifactKind::Glibc) => "glibc_conformance",
                    ("libc", ArtifactKind::Musl) => "musl_conformance",
                    ("preload", ArtifactKind::Glibc) => "glibc_preload",
                    ("preload", ArtifactKind::Musl) => "musl_preload",
                    _ => unreachable!(),
                };
                let mut command = cargo(root);
                let mut features = vec![match suite.as_str() {
                    "libc" => "libc-tests",
                    "preload" => "preload-tests",
                    _ => unreachable!(),
                }];
                if provider == Provider::Coro {
                    features.push("coro");
                }
                command
                    .current_dir(root)
                    .env(variable, manifest.manifest_path())
                    .args([
                        "test",
                        "-p",
                        "rsched",
                        "--test",
                        test,
                        "--features",
                        &features.join(","),
                    ]);
                command.args(["--", "--nocapture"]);
                run(
                    command,
                    &format!("run {libc} {suite} tests with {provider} provider"),
                )?;
            }
        }
        other => bail!("unknown test suite {other:?}"),
    }
    Ok(())
}

fn package_command(root: &Path, mut args: Vec<String>) -> Result<()> {
    let target = args_error(
        "usage: cargo xtask package <static|glibc|musl|all> [--provider ...]",
        &mut args,
    )?;
    let providers = take_providers(&mut args)?;
    let profile = take_profile(&mut args)?;
    let destination = take_option(&mut args, "--out")?
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("dist"));
    reject_extra(&args)?;

    let kinds = match target.as_str() {
        "static" => vec![ArtifactKind::Static],
        "glibc" => vec![ArtifactKind::Glibc],
        "musl" => vec![ArtifactKind::Musl],
        "all" => vec![
            ArtifactKind::Static,
            ArtifactKind::Glibc,
            ArtifactKind::Musl,
        ],
        other => bail!("unknown package target {other:?}"),
    };
    for provider in providers {
        for kind in &kinds {
            let manifest = ensure(root, *kind, provider, profile)?;
            println!("{}", package_artifact(&manifest, &destination)?.display());
        }
    }
    Ok(())
}

fn ensure(
    root: &Path,
    kind: ArtifactKind,
    provider: Provider,
    profile: BuildProfile,
) -> Result<rsched_libc_build::ArtifactManifest> {
    let mut options = BuildOptions::new(root, kind, provider);
    options.profile = profile;
    ensure_artifact(&options)
}

fn take_providers(args: &mut Vec<String>) -> Result<Vec<Provider>> {
    match take_option(args, "--provider")?.as_deref() {
        None | Some("all") => Ok(Provider::ALL.to_vec()),
        Some("native") => Ok(vec![Provider::Native]),
        Some("coro") => Ok(vec![Provider::Coro]),
        Some(value) => bail!("unknown provider {value:?}; expected native, coro, or all"),
    }
}

fn take_profile(args: &mut Vec<String>) -> Result<BuildProfile> {
    match take_option(args, "--profile")?.as_deref() {
        None | Some("release") => Ok(BuildProfile::Release),
        Some("debug") => Ok(BuildProfile::Debug),
        Some(value) => bail!("unknown profile {value:?}; expected debug or release"),
    }
}

fn take_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    args.remove(index);
    if index >= args.len() {
        bail!("{name} requires a value");
    }
    Ok(Some(args.remove(index)))
}

fn args_error(message: &str, args: &mut Vec<String>) -> Result<String> {
    if args.is_empty() {
        bail!("{message}");
    }
    Ok(args.remove(0))
}

fn reject_extra(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("unexpected arguments: {}", args.join(" "));
    }
    Ok(())
}

fn parse_libc(value: &str) -> Result<ArtifactKind> {
    match value {
        "glibc" => Ok(ArtifactKind::Glibc),
        "musl" => Ok(ArtifactKind::Musl),
        other => bail!("unknown libc {other:?}; expected glibc or musl"),
    }
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .context("xtask crate has no workspace parent")
}

fn cargo(root: &Path) -> Command {
    let _ = root;
    if let Some(path) = env::var_os("CARGO") {
        return Command::new(path);
    }
    if let Some(home) = env::var_os("HOME") {
        let cargo = PathBuf::from(home).join(".cargo/bin/cargo");
        if cargo.is_file() {
            return Command::new(cargo);
        }
    }
    Command::new("cargo")
}

fn run(mut command: Command, description: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to {description}"))?;
    if !status.success() {
        bail!("{description} failed with {status}");
    }
    Ok(())
}

fn usage() {
    eprintln!(
        "usage:\n\
         cargo xtask build static [--provider native|coro|all]\n\
         cargo xtask build libc <glibc|musl> [--provider native|coro|all]\n\
         cargo xtask build all [--provider native|coro|all]\n\
         cargo xtask test source [--provider native|coro|all]\n\
         cargo xtask test libc <glibc|musl> [--provider native|coro|all]\n\
         cargo xtask test preload <glibc|musl> [--provider native|coro|all]\n\
         cargo xtask package <static|glibc|musl|all> [--provider ...] [--out DIR]"
    );
}
