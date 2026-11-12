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
//! Hand-written handlers are never touched: a type is implemented by registering a
//! `ResourceTypeHandler` for it before the scaffold in the crate's `register`.

mod model;
mod render;
mod spec;

use std::path::PathBuf;

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
}
