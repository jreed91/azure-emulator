//! `azure-emulator`: serves the emulated Azure APIs on one local port.

use std::net::SocketAddr;

use azure_emulator::{tls, Config, Emulator};

const USAGE: &str = "\
azure-emulator — a local emulator of the Azure APIs

USAGE:
    azure-emulator [OPTIONS]

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

#[tokio::main]
async fn main() {
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
