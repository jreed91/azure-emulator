//! `azure-emulator`: serves the emulated Azure APIs on one local port.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use azure_emulator::idle::Activity;
use azure_emulator::{tls, Config, Emulator};

const USAGE: &str = "\
azure-emulator — a local emulator of the Azure APIs

USAGE:
    azure-emulator [OPTIONS]
    azure-emulator coverage [--operations]
    azure-emulator health [--port <PORT>] [--http]

COMMANDS:
    coverage               Print how much of each provider's API specification is
                           implemented, served generically from the schema, or
                           answered with 501, and how much of each listed data
                           plane's; --operations lists every operation
    health                 Ask the emulator running on this machine whether it is
                           ready (GET /__admin/health); exits 0 if it is. Reads the
                           port, scheme and bind address from the same options and
                           environment variables as the server

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
        --persist          Keep state across restarts, in <home>/state.json
        --state-file <PATH>
                           With --persist, keep the state in this file instead
        --enforce-rbac     Require a role assignment for every Resource Manager
                           request (by default any valid token may do anything)
        --rbac-owner <ID>  With --enforce-rbac, treat this object id (for the
                           emulator's tokens, the client id) as subscription
                           Owner; repeatable
        --idle-timeout <SECONDS>
                           Exit (saving state with --persist, as on Ctrl-C) after
                           this many seconds without an HTTP request or an open
                           AMQP connection
    -v, --version          Print the name and version
    -h, --help             Print this help

Every option can also be set in the environment: AZURE_EMULATOR_PORT,
AZURE_EMULATOR_AMQP_PORT, AZURE_EMULATOR_AMQPS_PORT,
AZURE_EMULATOR_BIND, AZURE_EMULATOR_HOST,
AZURE_EMULATOR_DNS_SUFFIX, AZURE_EMULATOR_HTTP=1, AZURE_EMULATOR_CONFIG,
AZURE_EMULATOR_ENFORCE_RBAC=1, AZURE_EMULATOR_RBAC_OWNERS (comma-separated),
AZURE_EMULATOR_PERSIST=1, AZURE_EMULATOR_STATE_FILE, AZURE_EMULATOR_IDLE_TIMEOUT.
";

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("error: {message}\n\n{USAGE}");
    std::process::exit(2);
}

/// Prints, per provider, how many modeled resource types and operations are
/// implemented, how many are served generically from the schema, and how many
/// answer 501.
async fn print_coverage(list_operations: bool) {
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
    // Data planes: each operation in the specification, probed for a 501.
    let data = Emulator::new(Config::default())
        .data_plane_coverage()
        .await
        .unwrap_or_else(|e| fail(format!("probing the data planes failed: {e}")));
    let mut planes: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for entry in &data {
        let counts = planes.entry(entry.plane).or_default();
        counts.0 += usize::from(entry.implemented);
        counts.1 += 1;
    }
    println!();
    println!("{:<30} {:>9} {:>12}", "Data plane", "", "Operations");
    for (plane, (implemented, all)) in &planes {
        println!("{plane:<30} {:>9} {:>12}", "", format!("{implemented}/{all}"));
    }
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
        // Prefixed with `data` so a reader of the lines above can skip them.
        for entry in &data {
            let status = if entry.implemented { "ok     " } else { "501    " };
            println!("data {status}  {:<6} {}  ({}: {})", entry.operation.method, entry.operation.path, entry.plane, entry.operation.id);
        }
    }
}

#[tokio::main]
async fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|a| a == "coverage") {
        match &arguments[1..] {
            [] => print_coverage(false).await,
            [flag] if flag == "--operations" => print_coverage(true).await,
            _ => fail("coverage takes only --operations"),
        }
        return;
    }
    let health = arguments.first().is_some_and(|a| a == "health");
    let mut config = Config::default();
    let mut bind = "127.0.0.1".to_string();
    let mut config_file = std::env::var("AZURE_EMULATOR_CONFIG").ok();
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let mut persist = env("AZURE_EMULATOR_PERSIST").is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"));
    let mut state_file = env("AZURE_EMULATOR_STATE_FILE").map(std::path::PathBuf::from);
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
    if env("AZURE_EMULATOR_ENFORCE_RBAC").is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false")) {
        config.enforce_rbac = true;
    }
    if let Some(value) = env("AZURE_EMULATOR_RBAC_OWNERS") {
        config.rbac_owners.extend(value.split(',').map(str::trim).filter(|v| !v.is_empty()).map(String::from));
    }
    let mut idle_timeout = env("AZURE_EMULATOR_IDLE_TIMEOUT")
        .map(|v| seconds(&v).unwrap_or_else(|| fail("AZURE_EMULATOR_IDLE_TIMEOUT is not a number of seconds")));
    let mut args = std::env::args().skip(if health { 2 } else { 1 });
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
            "--enforce-rbac" => config.enforce_rbac = true,
            "--persist" => persist = true,
            "--state-file" => state_file = Some(value("--state-file").into()),
            "--rbac-owner" => config.rbac_owners.push(value("--rbac-owner")),
            "--idle-timeout" => {
                idle_timeout = Some(
                    seconds(&value("--idle-timeout")).unwrap_or_else(|| fail("--idle-timeout is not a number of seconds")),
                )
            }
            "-v" | "--version" => {
                println!("azure-emulator {}", azure_emulator::VERSION);
                return;
            }
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
    if health {
        std::process::exit(match check_health(address, &config).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("unhealthy: {error}");
                1
            }
        });
    }

    let emulator = Emulator::new(config.clone());
    // A snapshot is loaded before provisioning, which then updates what it holds.
    let state_file = persist.then(|| state_file.unwrap_or_else(azure_emulator::persist::default_path));
    if let Some(path) = &state_file {
        azure_emulator::persist::register(&emulator.engine());
        match azure_emulator::persist::load(&emulator.engine(), path) {
            Ok(true) => println!("Restored state from {}", path.display()),
            Ok(false) => {}
            Err(error) => eprintln!("warning: {error}"),
        }
    }
    if let Some(path) = &config_file {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| fail(format!("cannot read {path}: {e}")));
        let file: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| fail(format!("{path} is not JSON: {e}")));
        let done = azure_emulator::provision::provision(&emulator, &file)
            .await
            .unwrap_or_else(|e| fail(format!("provisioning from {path} failed: {e}")));
        println!("Provisioned {} resource(s) from {path}", done.len());
    }
    // What `--idle-timeout` watches: HTTP requests while they are handled, and
    // AMQP connections while they are open.
    let activity = Activity::new();
    let app = {
        let activity = activity.clone();
        emulator.router().layer(axum::middleware::from_fn(move |request, next: axum::middleware::Next| {
            let busy = activity.begin();
            async move {
                let response = next.run(request).await;
                drop(busy);
                response
            }
        }))
    }
    .into_make_service();
    println!("Azure Emulator");
    println!("  Resource Manager + login: {}", config.management_endpoint());
    println!("  Blob:                     {}", config.blob_endpoint("<account>"));
    println!("  Key Vault:                {}", config.vault_uri("<vault>"));
    println!("  Subscription:             {}", config.subscription_id);
    println!("  Tenant:                   {}", config.tenant_id);
    if config.enforce_rbac {
        println!("  Role assignments:         enforced (owners: {})", config.rbac_owners.join(", "));
    }
    if let Some(timeout) = idle_timeout {
        println!("  Idle timeout:             {}s", timeout.as_secs_f64());
    }
    if let Some(path) = &state_file {
        println!("  State kept in:            {}", path.display());
        tokio::spawn(azure_emulator::persist::keep(emulator.engine(), path.clone()));
    }

    let amqp_address = format!("{bind}:{}", config.amqp_port);
    let amqp = amqp_address
        .parse()
        .map_err(|e| format!("{e}"))
        .and_then(|address| service_servicebus::broker::listen(address).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| fail(format!("cannot listen for AMQP on {amqp_address}: {e}")));
    println!("  Service Bus (AMQP):       {}", config.servicebus_amqp_endpoint("<namespace>"));
    tokio::spawn(serve_amqp(emulator.engine(), amqp, None, activity.clone()));

    // The same listener over TLS, with the emulator's certificate: what
    // clients that connect by namespace name (and an Entra ID credential) use.
    let identity = tls::identity(&config, &bind).unwrap_or_else(|e| fail(format!("TLS setup failed: {e}")));
    let amqps_address = format!("{bind}:{}", config.amqps_port);
    let amqps = amqps_address
        .parse()
        .map_err(|e| format!("{e}"))
        .and_then(|address| service_servicebus::broker::listen(address).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| fail(format!("cannot listen for AMQP over TLS on {amqps_address}: {e}")));
    let amqps_tls = tls::acceptor(&identity).unwrap_or_else(|e| fail(format!("TLS setup failed: {e}")));
    println!("  Service Bus (AMQP + TLS): amqps://<namespace>.servicebus.{}:{}/", config.dns_suffix, config.amqps_port);
    tokio::spawn(serve_amqp(emulator.engine(), amqps, Some(amqps_tls), activity.clone()));
    println!("  CA certificate to trust:  {}", identity.ca_path.display());

    let serving = async {
        if config.scheme == "https" {
            let rustls = axum_server::tls_rustls::RustlsConfig::from_pem(
                identity.certificate_chain_pem.into_bytes(),
                identity.private_key_pem.into_bytes(),
            )
            .await
            .unwrap_or_else(|e| fail(format!("TLS setup failed: {e}")));
            axum_server::bind_rustls(address, rustls).serve(app).await
        } else {
            axum_server::bind(address).serve(app).await
        }
    };
    let idle = async {
        match idle_timeout {
            Some(timeout) => {
                activity.idle_for(timeout).await;
                println!("No activity for {}s; shutting down", timeout.as_secs_f64());
            }
            None => std::future::pending().await,
        }
    };
    let result = tokio::select! {
        result = serving => result,
        () = async { tokio::select! { () = shutdown_signal() => {}, () = idle => {} } } => {
            // The last changes, made since the last periodic save.
            if let Some(path) = &state_file {
                if let Err(error) = azure_emulator::persist::save(&emulator.engine(), path, None) {
                    eprintln!("warning: saving state failed: {error}");
                }
            }
            Ok(())
        }
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// A number of seconds, whole or not, and more than zero.
fn seconds(text: &str) -> Option<Duration> {
    text.trim().parse::<f64>().ok().filter(|s| s.is_finite() && *s > 0.0).and_then(|s| Duration::try_from_secs_f64(s).ok())
}

/// Serves AMQP connections from `listener` (over TLS when given an acceptor),
/// as `broker::serve` does, marking the emulator busy while each is open.
async fn serve_amqp(
    engine: Arc<emulator_core::Engine>,
    listener: tokio::net::TcpListener,
    tls: Option<tokio_rustls::TlsAcceptor>,
    activity: Arc<Activity>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let busy = activity.begin();
        let engine = engine.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            match tls {
                Some(tls) => {
                    if let Ok(stream) = tls.accept(stream).await {
                        service_servicebus::broker::serve_stream(engine, stream).await;
                    }
                }
                None => service_servicebus::broker::serve_stream(engine, stream).await,
            }
            drop(busy);
        });
    }
}

/// `azure-emulator health`: one `GET /__admin/health` to the emulator on this
/// machine, over HTTPS (trusting the emulator's own CA) unless `--http`.
async fn check_health(address: SocketAddr, config: &Config) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // A server bound to every interface is reached on loopback.
    let mut target = address;
    if target.ip().is_unspecified() {
        target.set_ip(if target.is_ipv4() { [127, 0, 0, 1].into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", azure_emulator::HEALTH_PATH);
    let exchange = async {
        let stream = tokio::net::TcpStream::connect(target).await.map_err(|e| format!("cannot connect to {target}: {e}"))?;
        let mut reply = Vec::new();
        if config.scheme == "https" {
            use tokio_rustls::rustls;
            let ca_path = tls::home_dir().join("ca.pem");
            let ca = std::fs::read(&ca_path).map_err(|e| format!("cannot read {}: {e}", ca_path.display()))?;
            let mut roots = rustls::RootCertStore::empty();
            for certificate in rustls_pemfile::certs(&mut ca.as_slice()) {
                roots.add(certificate.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            }
            let client = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
            let name = rustls::pki_types::ServerName::try_from("localhost").map_err(|e| e.to_string())?;
            let mut stream = tokio_rustls::TlsConnector::from(Arc::new(client))
                .connect(name, stream)
                .await
                .map_err(|e| format!("TLS with {target} failed: {e}"))?;
            stream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
            // A server that closes without a TLS close_notify still sent its answer.
            let _ = stream.read_to_end(&mut reply).await;
        } else {
            let mut stream = stream;
            stream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
            let _ = stream.read_to_end(&mut reply).await;
        }
        Ok::<_, String>(reply)
    };
    let reply = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .map_err(|_| format!("{target} did not answer within 5s"))??;
    let reply = String::from_utf8_lossy(&reply);
    let status = reply.lines().next().unwrap_or("");
    if status.starts_with("HTTP/1.1 200") || status.starts_with("HTTP/1.0 200") {
        println!("ok: {target}");
        Ok(())
    } else if status.is_empty() {
        Err(format!("{target} closed the connection without answering"))
    } else if !status.starts_with("HTTP/") {
        Err(format!("{target} did not answer in HTTP (does it serve HTTPS? then leave out --http)"))
    } else {
        Err(format!("{target} answered {status}"))
    }
}

/// Ctrl-C, or SIGTERM where there is one (as `docker stop` sends).
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
