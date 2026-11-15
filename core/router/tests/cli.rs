//! The `azure-emulator` command line: `coverage`, `--help`, `--version`, the
//! refusals of bad options and environment variables, a server started from
//! the environment alone, `health`, and `--idle-timeout`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_azure-emulator");

fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    let home = std::env::temp_dir().join(format!("azure-emulator-cli-{}", std::process::id()));
    let mut command = Command::new(BINARY);
    command.args(args).env("AZURE_EMULATOR_HOME", &home);
    for name in [
        "AZURE_EMULATOR_PORT",
        "AZURE_EMULATOR_AMQP_PORT",
        "AZURE_EMULATOR_AMQPS_PORT",
        "AZURE_EMULATOR_HTTP",
        "AZURE_EMULATOR_CONFIG",
        "AZURE_EMULATOR_PERSIST",
        "AZURE_EMULATOR_IDLE_TIMEOUT",
    ] {
        command.env_remove(name);
    }
    command.envs(env.iter().copied());
    command.output().expect("the binary runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn coverage_summarises_and_lists_operations() {
    let summary = run(&["coverage"], &[]);
    assert!(summary.status.success(), "{}", text(&summary.stderr));
    let out = text(&summary.stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("Provider") && lines[0].contains("Operations"), "{out}");
    for provider in ["Microsoft.Storage", "Microsoft.ServiceBus", "Microsoft.KeyVault", "Microsoft.DocumentDB"] {
        assert!(lines.iter().any(|l| l.starts_with(provider)), "{provider} in {out}");
    }
    let total = lines.iter().find(|l| l.starts_with("Total")).expect("a total line");
    // Data planes follow, each with "implemented/all" operations.
    let key_vault = lines.iter().find(|l| l.starts_with("Key Vault")).expect("a Key Vault data-plane line");
    let (done, all) = key_vault.split_whitespace().last().and_then(|cell| cell.split_once('/')).expect("counts");
    let (done, all): (u32, u32) = (done.parse().unwrap(), all.parse().unwrap());
    assert!(done > 0 && done < all, "{key_vault}");
    // "implemented/all" for types and for operations.
    let counts: Vec<(u32, u32)> = total
        .split_whitespace()
        .filter_map(|cell| cell.split_once('/'))
        .map(|(done, all)| (done.parse().unwrap(), all.parse().unwrap()))
        .collect();
    assert_eq!(counts.len(), 2, "{total}");
    assert!(counts.iter().all(|(done, all)| done > &0 && done <= all), "{total}");

    let detailed = run(&["coverage", "--operations"], &[]);
    let out = text(&detailed.stdout);
    assert!(out.lines().any(|l| l.starts_with("ok ") && l.contains("PUT")), "an implemented operation is listed");
    assert!(out.lines().any(|l| l.starts_with("501")), "an unimplemented operation is listed");
    assert!(out.lines().any(|l| l.starts_with("generic")), "a schema-served operation is listed");
    assert!(out.lines().any(|l| l.starts_with("data ok") && l.contains("/secrets/{secret-name}")), "an implemented data-plane operation");
    assert!(out.lines().any(|l| l.starts_with("data 501") && l.contains("BackupSecret")), "an unimplemented data-plane operation");

    let refused = run(&["coverage", "--everything"], &[]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(text(&refused.stderr).contains("coverage takes only --operations"));
}

/// Arguments, environment, and what the error says.
type Refusal = (&'static [&'static str], &'static [(&'static str, &'static str)], &'static str);

#[test]
fn help_and_refusals() {
    let help = run(&["--help"], &[]);
    assert!(help.status.success());
    assert!(text(&help.stdout).contains("USAGE:"));
    assert!(text(&run(&["-h"], &[]).stdout).contains("--enforce-rbac"));

    let cases: [Refusal; 10] = [
        (&["--idle-timeout"], &[], "--idle-timeout needs a value"),
        (&["--idle-timeout", "0"], &[], "--idle-timeout is not a number of seconds"),
        (&[], &[("AZURE_EMULATOR_IDLE_TIMEOUT", "soon")], "AZURE_EMULATOR_IDLE_TIMEOUT is not a number of seconds"),
        (&["--frobnicate"], &[], "unknown option '--frobnicate'"),
        (&["--port"], &[], "--port needs a value"),
        (&["--port", "eighty"], &[], "--port is not a port"),
        (&["--amqp-port", "70000"], &[], "--amqp-port is not a port"),
        (&["--bind", "not-an-address", "--port", "1"], &[], "--bind is not an IP address"),
        (&[], &[("AZURE_EMULATOR_PORT", "x")], "AZURE_EMULATOR_PORT is not a port"),
        (&[], &[("AZURE_EMULATOR_AMQPS_PORT", "-1")], "AZURE_EMULATOR_AMQPS_PORT is not a port"),
    ];
    for (args, env, message) in cases {
        let output = run(args, env);
        assert_eq!(output.status.code(), Some(2), "{args:?} {env:?}");
        let stderr = text(&output.stderr);
        assert!(stderr.contains(message) && stderr.contains("USAGE:"), "{args:?}: {stderr}");
    }
}

#[test]
fn version() {
    for flag in ["--version", "-v"] {
        let output = run(&[flag], &[]);
        assert!(output.status.success(), "{flag}");
        let expected = format!("azure-emulator {}\n", option_env!("AZURE_EMULATOR_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")));
        assert_eq!(text(&output.stdout), expected, "{flag}");
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn a_server_configured_from_the_environment() {
    let (port, amqp, amqps) = (free_port(), free_port(), free_port());
    let home = std::env::temp_dir().join(format!("azure-emulator-cli-env-{}", std::process::id()));
    let mut child = Command::new(BINARY)
        .env("AZURE_EMULATOR_HOME", &home)
        .env("AZURE_EMULATOR_PORT", port.to_string())
        .env("AZURE_EMULATOR_AMQP_PORT", amqp.to_string())
        .env("AZURE_EMULATOR_AMQPS_PORT", amqps.to_string())
        .env("AZURE_EMULATOR_HTTP", "1")
        .env("AZURE_EMULATOR_HOST", "emulator.test")
        .env("AZURE_EMULATOR_DNS_SUFFIX", "example.internal")
        .env("AZURE_EMULATOR_ENFORCE_RBAC", "true")
        .env("AZURE_EMULATOR_RBAC_OWNERS", "owner-a, owner-b")
        .env_remove("AZURE_EMULATOR_CONFIG")
        .env_remove("AZURE_EMULATOR_PERSIST")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the server starts");

    // Plain HTTP on the port from the environment.
    let deadline = Instant::now() + Duration::from_secs(20);
    let reply = loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let request = "GET /metadata/endpoints?api-version=2022-09-01 HTTP/1.1\r\nHost: emulator.test\r\nConnection: close\r\n\r\n";
            stream.write_all(request.as_bytes()).unwrap();
            let mut reply = String::new();
            if stream.read_to_string(&mut reply).is_ok() && !reply.is_empty() {
                break reply;
            }
        }
        assert!(Instant::now() < deadline, "the server did not answer");
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = child.kill();
    let _ = child.wait();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("http://emulator.test:"), "the host and scheme come from the environment: {reply}");
    assert!(reply.contains("example.internal"), "the DNS suffix comes from the environment: {reply}");
}

/// Starts a plain-HTTP server on free ports with `extra` arguments.
fn start(home: &std::path::Path, extra: &[&str]) -> (std::process::Child, u16) {
    let (port, amqp, amqps) = (free_port(), free_port(), free_port());
    let mut args = vec!["--http".to_string(), "--port".into(), port.to_string()];
    args.extend(["--amqp-port".into(), amqp.to_string(), "--amqps-port".into(), amqps.to_string()]);
    args.extend(extra.iter().map(|a| a.to_string()));
    let child = Command::new(BINARY)
        .args(&args)
        .env("AZURE_EMULATOR_HOME", home)
        .env_remove("AZURE_EMULATOR_PORT")
        .env_remove("AZURE_EMULATOR_HTTP")
        .env_remove("AZURE_EMULATOR_CONFIG")
        .env_remove("AZURE_EMULATOR_PERSIST")
        .env_remove("AZURE_EMULATOR_IDLE_TIMEOUT")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the server starts");
    (child, port)
}

/// One plain-HTTP request; the whole reply, once the server answers.
fn get(port: u16, host: &str, path: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).unwrap();
            let mut reply = String::new();
            if stream.read_to_string(&mut reply).is_ok() && !reply.is_empty() {
                return reply;
            }
        }
        assert!(Instant::now() < deadline, "the server did not answer");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn health_endpoint_and_subcommand() {
    let home = std::env::temp_dir().join(format!("azure-emulator-cli-health-{}", std::process::id()));
    let (mut child, port) = start(&home, &[]);
    let reply = get(port, "whatever.example", "/__admin/health");
    let port_text = port.to_string();
    let probe = run(&["health", "--http", "--port", &port_text], &[]);
    let probe_from_env = run(&["health"], &[("AZURE_EMULATOR_HTTP", "1"), ("AZURE_EMULATOR_PORT", &port_text)]);
    let _ = child.kill();
    let _ = child.wait();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains(r#""status":"ok""#), "{reply}");
    assert!(probe.status.success(), "{}", text(&probe.stderr));
    assert!(probe_from_env.status.success(), "{}", text(&probe_from_env.stderr));

    // Nothing listening: unhealthy.
    let down = run(&["health", "--http", "--port", &free_port().to_string()], &[]);
    assert_eq!(down.status.code(), Some(1));
    assert!(text(&down.stderr).contains("unhealthy"), "{}", text(&down.stderr));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn idle_timeout_exits_cleanly_and_saves_state() {
    let home = std::env::temp_dir().join(format!("azure-emulator-cli-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let state = home.join("state.json");
    let state_text = state.to_string_lossy().into_owned();
    let started = Instant::now();
    let (mut child, port) = start(&home, &["--idle-timeout", "2", "--persist", "--state-file", &state_text]);
    // Requests keep it alive past the timeout.
    let deadline = started + Duration::from_secs(4);
    while Instant::now() < deadline {
        assert!(get(port, "localhost", "/__admin/health").starts_with("HTTP/1.1 200"));
        assert!(child.try_wait().unwrap().is_none(), "exited while in use");
        std::thread::sleep(Duration::from_millis(500));
    }
    // Then, left alone, it exits by itself.
    let quiet = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if quiet.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("still running after the idle timeout");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "{status}");
    assert!(quiet.elapsed() >= Duration::from_millis(1500), "not before the timeout: {:?}", quiet.elapsed());
    let mut stdout = String::new();
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    assert!(stdout.contains("No activity for 2s"), "{stdout}");
    assert!(state.exists(), "the state is saved on the way out");
    let _ = std::fs::remove_dir_all(home);
}
