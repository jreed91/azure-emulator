//! `emulator-scaffold`: turns a resource provider's OpenAPI documents (as published
//! in Azure/azure-rest-api-specs) into the table the emulator mounts.
//!
//! For a provider it writes, under `services/<service>/`:
//!
//! * `src/scaffold.rs` — every resource type, the `api-version`s it is defined in,
//!   and every operation, as a `ScaffoldedProvider`. Regenerated on every run.
//! * `CONTRACT.md` — what the specification says about each type: operations, name
//!   constraints and properties. Regenerated on every run.
//!
//! and, the first time only, the crate itself plus its workspace and router wiring.
//! It also records where the specification came from (repository, path and
//! commit) in `.emulator/scaffolded-specs.json`, so a scaffold can be regenerated
//! from the same input.
//! Hand-written handlers are never touched: a type is implemented by registering a
//! `ResourceTypeHandler` for it before the scaffold in the crate's `register`.

mod model;
mod render;
mod spec;

use std::path::{Path, PathBuf};

const USAGE: &str = "\
emulator-scaffold — generate a service's scaffold from its OpenAPI specification

USAGE:
    emulator-scaffold --namespace <Microsoft.X> --service <name> --spec <dir> [--repo <dir>]

OPTIONS:
    --namespace <NS>   Resource provider namespace, e.g. Microsoft.Storage
    --service <NAME>   Crate directory under services/, e.g. storage
    --spec <DIR>       Directory holding the provider's stable/ and preview/ version
                       directories (searched recursively)
    --repo <DIR>       Emulator repository root [default: .]
";

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("error: {message}\n\n{USAGE}");
    std::process::exit(2);
}

fn main() {
    let (mut namespace, mut service, mut spec_dir) = (None, None, None);
    let mut repo = PathBuf::from(".");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| fail(format!("{arg} needs a value")));
        match arg.as_str() {
            "--namespace" => namespace = Some(value()),
            "--service" => service = Some(value()),
            "--spec" => spec_dir = Some(PathBuf::from(value())),
            "--repo" => repo = PathBuf::from(value()),
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => fail(format!("unknown option '{other}'")),
        }
    }
    let namespace = namespace.unwrap_or_else(|| fail("--namespace is required"));
    let service = service.unwrap_or_else(|| fail("--service is required"));
    let spec_dir = spec_dir.unwrap_or_else(|| fail("--spec is required"));
    if !service.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) || service.is_empty() {
        fail("--service must be lowercase letters and digits");
    }

    let versions = spec::discover_versions(&spec_dir);
    if versions.is_empty() {
        fail(format!("no stable/ or preview/ version directories under {}", spec_dir.display()));
    }
    let mut resolver = spec::Resolver::default();
    let provider = model::build(&namespace, &versions, &mut resolver);
    if provider.types.is_empty() && provider.operations.is_empty() {
        fail(format!("no operations for {namespace} found under {}", spec_dir.display()));
    }

    let crate_dir = repo.join("services").join(&service);
    let created = render::write_crate(&repo, &crate_dir, &service, &provider)
        .unwrap_or_else(|e| fail(format!("writing {}: {e}", crate_dir.display())));

    let operations: usize =
        provider.types.values().map(|t| t.operations.len()).sum::<usize>() + provider.operations.len();
    println!(
        "{namespace}: {} resource types, {operations} operations, {} api-versions",
        provider.types.len(),
        versions.len()
    );
    println!("  wrote {}", crate_dir.join("src/scaffold.rs").display());
    println!("  wrote {}", crate_dir.join("CONTRACT.md").display());
    if created {
        println!("  created crate service-{service} and wired it into the workspace and router");
    }
    match record_source(&repo, &service, &namespace, &spec_dir) {
        Ok(commit) => println!("  recorded specification commit {commit} in {SOURCES}"),
        Err(e) => eprintln!("warning: {SOURCES} not updated: {e}"),
    }
}

/// Where each provider's specification came from, relative to the repository root.
const SOURCES: &str = ".emulator/scaffolded-specs.json";

/// `git <args>` in `dir`, trimmed.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        return Err(format!("{} is not in a git checkout", dir.display()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Records the specification checkout `spec_dir` is in (its repository, the
/// path inside it and the commit) as the source of `service`'s scaffold.
/// Returns the commit.
fn record_source(repo: &Path, service: &str, namespace: &str, spec_dir: &Path) -> Result<String, String> {
    if !git(spec_dir, &["status", "--porcelain", "--", "."])?.is_empty() {
        return Err(format!("{} has uncommitted changes, so no commit describes it", spec_dir.display()));
    }
    let commit = git(spec_dir, &["rev-parse", "HEAD"])?;
    let path = git(spec_dir, &["rev-parse", "--show-prefix"])?;
    let remote = git(spec_dir, &["remote", "get-url", "origin"]).unwrap_or_default();
    let repository = remote.strip_suffix(".git").unwrap_or(&remote);
    let file = repo.join(SOURCES);
    let mut sources: serde_json::Value = match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{SOURCES} is not JSON: {e}"))?,
        Err(_) => serde_json::json!({}),
    };
    let providers = sources.as_object_mut().ok_or(format!("{SOURCES} is not an object"))?;
    providers.insert(
        service.to_string(),
        serde_json::json!({
            "namespace": namespace,
            "repository": repository,
            "path": path.trim_end_matches('/'),
            "commit": commit,
        }),
    );
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&sources).map_err(|e| e.to_string())?;
    std::fs::write(&file, text + "\n").map_err(|e| e.to_string())?;
    Ok(commit)
}
