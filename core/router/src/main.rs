//! `azure-emulator`: serves the emulated Azure APIs on one local port.

use std::net::SocketAddr;

use azure_emulator::{tls, Config, Emulator};

const USAGE: &str = "\
azure-emulator — a local emulator of the Azure APIs

USAGE:
    azure-emulator [OPTIONS]
    azure-emulator coverage [--operations]

COMMANDS:
    coverage               Print how much of each provider's API specification is
                           implemented, served generically from the schema, or
                           answered with 501; --operations lists every operation

OPTIONS:
    -p, --port <PORT>      Port to listen on [env: AZURE_EMULATOR_PORT] [default: 10443]
        --bind <ADDRESS>   Address to bind [default: 127.0.0.1]
        --host <NAME>      Hostname clients use for Resource Manager [default: localhost]
        --dns-suffix <S>   Suffix of data-plane hostnames [default: azure.localhost]
        --http             Serve plain HTTP instead of HTTPS
    -h, --help             Print this help
";

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("error: {message}\n\n{USAGE}");
    std::process::exit(2);
}

/// Prints, per provider, how many modeled resource types and operations are
/// implemented, how many are served generically from the schema, and how many
/// answer 501.
fn print_coverage(list_operations: bool) {
    use std::collections::{BTreeMap, BTreeSet};
    let coverage = Emulator::new(Config::default()).coverage();
    #[derive(Default)]
    struct Totals<'a> {
        types: BTreeSet<&'a str>,
        implemented_types: BTreeSet<&'a str>,
        generic_types: BTreeSet<&'a str>,
        operations: usize,
        implemented: usize,
        generic: usize,
    }
    let mut providers: BTreeMap<&str, Totals> = BTreeMap::new();
    for entry in &coverage {
        let totals = providers.entry(entry.namespace).or_default();
        totals.operations += 1;
        totals.implemented += usize::from(entry.implemented);
        totals.generic += usize::from(entry.generic);
        if let Some(kind) = entry.resource_type {
            totals.types.insert(kind);
            // A type counts once any of its operations works.
            if entry.implemented {
                totals.implemented_types.insert(kind);
            } else if entry.generic {
                totals.generic_types.insert(kind);
            }
        }
    }
    let row = |name: &str, types: String, operations: String, generic: String| {
        println!("{name:<30} {types:>9} {operations:>12} {generic:>9}");
    };
    row("Provider", "Types".into(), "Operations".into(), "Generic".into());
    let mut all = Totals::default();
    for (namespace, totals) in &providers {
        row(
            namespace,
            format!("{}/{}", totals.implemented_types.len(), totals.types.len()),
            format!("{}/{}", totals.implemented, totals.operations),
            totals.generic.to_string(),
        );
        all.types.extend(&totals.types);
        all.implemented_types.extend(&totals.implemented_types);
        all.operations += totals.operations;
        all.implemented += totals.implemented;
        all.generic += totals.generic;
    }
    row(
        "Total",
        format!("{}/{}", all.implemented_types.len(), all.types.len()),
        format!("{}/{}", all.implemented, all.operations),
        all.generic.to_string(),
    );
    if list_operations {
        println!();
        for entry in &coverage {
            let status = match (entry.implemented, entry.generic) {
                (true, _) => "ok     ",
                (false, true) => "generic",
                (false, false) => "501    ",
            };
            println!("{status}  {:<6} {}  ({})", entry.operation.method, entry.operation.path, entry.operation.id);
        }
    }
}

#[tokio::main]
async fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|a| a == "coverage") {
        match &arguments[1..] {
            [] => print_coverage(false),
            [flag] if flag == "--operations" => print_coverage(true),
            _ => fail("coverage takes only --operations"),
        }
        return;
    }
    let mut config = Config::default();
    let mut bind = "127.0.0.1".to_string();
    if let Ok(port) = std::env::var("AZURE_EMULATOR_PORT") {
        config.port = port.parse().unwrap_or_else(|_| fail("AZURE_EMULATOR_PORT is not a port"));
    }
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| fail(format!("{name} needs a value")));
        match arg.as_str() {
            "-p" | "--port" => {
                config.port = value("--port").parse().unwrap_or_else(|_| fail("--port is not a port"))
            }
            "--bind" => bind = value("--bind"),
            "--host" => config.host = value("--host"),
            "--dns-suffix" => config.dns_suffix = value("--dns-suffix"),
            "--http" => config.scheme = "http".into(),
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => fail(format!("unknown option '{other}'")),
        }
    }
    let address: SocketAddr = format!("{bind}:{}", config.port)
        .parse()
        .unwrap_or_else(|_| fail("--bind is not an IP address"));

    let emulator = Emulator::new(config.clone());
    let app = emulator.router().into_make_service();
    println!("Azure Emulator");
    println!("  Resource Manager + login: {}", config.management_endpoint());
    println!("  Blob:                     {}", config.blob_endpoint("<account>"));
    println!("  Key Vault:                {}", config.vault_uri("<vault>"));
    println!("  Subscription:             {}", config.subscription_id);
    println!("  Tenant:                   {}", config.tenant_id);

    let result = if config.scheme == "https" {
        let identity = tls::identity(&config, &bind).unwrap_or_else(|e| fail(format!("TLS setup failed: {e}")));
        println!("  CA certificate to trust:  {}", identity.ca_path.display());
        let rustls = axum_server::tls_rustls::RustlsConfig::from_pem(
            identity.certificate_chain_pem.into_bytes(),
            identity.private_key_pem.into_bytes(),
        )
        .await
        .unwrap_or_else(|e| fail(format!("TLS setup failed: {e}")));
        axum_server::bind_rustls(address, rustls).serve(app).await
    } else {
        axum_server::bind(address).serve(app).await
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
