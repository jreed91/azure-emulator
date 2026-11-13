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
        --amqp-port <PORT> Port of the Service Bus AMQP listener [default: 5672]
        --amqps-port <PORT>
                           Port of the Service Bus AMQP-over-TLS listener [default: 5671]
        --http             Serve plain HTTP instead of HTTPS
        --config <FILE>    Provision resources from a JSON file at startup
                           (see the README)

Every option can also be set in the environment: AZURE_EMULATOR_PORT,
AZURE_EMULATOR_AMQP_PORT, AZURE_EMULATOR_AMQPS_PORT,
AZURE_EMULATOR_BIND, AZURE_EMULATOR_HOST,
AZURE_EMULATOR_DNS_SUFFIX, AZURE_EMULATOR_HTTP=1, AZURE_EMULATOR_CONFIG.
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
    let mut config_file = std::env::var("AZURE_EMULATOR_CONFIG").ok();
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    if let Some(port) = env("AZURE_EMULATOR_PORT") {
        config.port = port.parse().unwrap_or_else(|_| fail("AZURE_EMULATOR_PORT is not a port"));
    }
    if let Some(port) = env("AZURE_EMULATOR_AMQP_PORT") {
        config.amqp_port = port.parse().unwrap_or_else(|_| fail("AZURE_EMULATOR_AMQP_PORT is not a port"));
    }
    if let Some(port) = env("AZURE_EMULATOR_AMQPS_PORT") {
        config.amqps_port = port.parse().unwrap_or_else(|_| fail("AZURE_EMULATOR_AMQPS_PORT is not a port"));
    }
    if let Some(value) = env("AZURE_EMULATOR_BIND") {
        bind = value;
    }
    if let Some(value) = env("AZURE_EMULATOR_HOST") {
        config.host = value;
    }
    if let Some(value) = env("AZURE_EMULATOR_DNS_SUFFIX") {
        config.dns_suffix = value;
    }
    if env("AZURE_EMULATOR_HTTP").is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false")) {
        config.scheme = "http".into();
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
            "--amqp-port" => {
                config.amqp_port = value("--amqp-port").parse().unwrap_or_else(|_| fail("--amqp-port is not a port"))
            }
            "--amqps-port" => {
                config.amqps_port = value("--amqps-port").parse().unwrap_or_else(|_| fail("--amqps-port is not a port"))
            }
            "--http" => config.scheme = "http".into(),
            "--config" => config_file = Some(value("--config")),
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
    if let Some(path) = &config_file {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| fail(format!("cannot read {path}: {e}")));
        let file: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| fail(format!("{path} is not JSON: {e}")));
        let done = azure_emulator::provision::provision(&emulator, &file)
            .await
            .unwrap_or_else(|e| fail(format!("provisioning from {path} failed: {e}")));
        println!("Provisioned {} resource(s) from {path}", done.len());
    }
    let app = emulator.router().into_make_service();
    println!("Azure Emulator");
    println!("  Resource Manager + login: {}", config.management_endpoint());
    println!("  Blob:                     {}", config.blob_endpoint("<account>"));
    println!("  Key Vault:                {}", config.vault_uri("<vault>"));
    println!("  Subscription:             {}", config.subscription_id);
    println!("  Tenant:                   {}", config.tenant_id);

    let amqp_address = format!("{bind}:{}", config.amqp_port);
    let amqp = tokio::net::TcpListener::bind(&amqp_address)
        .await
        .unwrap_or_else(|e| fail(format!("cannot listen for AMQP on {amqp_address}: {e}")));
    println!("  Service Bus (AMQP):       {}", config.servicebus_amqp_endpoint("<namespace>"));
    tokio::spawn(service_servicebus::broker::serve(emulator.engine(), amqp, None));

    // The same listener over TLS, with the emulator's certificate: what
    // clients that connect by namespace name (and an Entra ID credential) use.
    let identity = tls::identity(&config, &bind).unwrap_or_else(|e| fail(format!("TLS setup failed: {e}")));
    let amqps_address = format!("{bind}:{}", config.amqps_port);
    let amqps = tokio::net::TcpListener::bind(&amqps_address)
        .await
        .unwrap_or_else(|e| fail(format!("cannot listen for AMQP over TLS on {amqps_address}: {e}")));
    let amqps_tls = tls::acceptor(&identity).unwrap_or_else(|e| fail(format!("TLS setup failed: {e}")));
    println!("  Service Bus (AMQP + TLS): amqps://<namespace>.servicebus.{}:{}/", config.dns_suffix, config.amqps_port);
    tokio::spawn(service_servicebus::broker::serve(emulator.engine(), amqps, Some(amqps_tls)));
    println!("  CA certificate to trust:  {}", identity.ca_path.display());

    let result = if config.scheme == "https" {
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
