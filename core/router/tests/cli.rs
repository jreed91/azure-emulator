//! The `azure-emulator` command line: `coverage`, `--help`, the refusals of
//! bad options and environment variables, and a server started from the
//! environment alone.

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
    let total = lines.last().expect("a total line");
    assert!(total.starts_with("Total"), "{out}");
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

    let refused = run(&["coverage", "--everything"], &[]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(text(&refused.stderr).contains("coverage takes only --operations"));
}

#[test]
fn help_and_refusals() {
    let help = run(&["--help"], &[]);
    assert!(help.status.success());
    assert!(text(&help.stdout).contains("USAGE:"));
    assert!(text(&run(&["-h"], &[]).stdout).contains("--enforce-rbac"));

    let cases: [(&[&str], &[(&str, &str)], &str); 7] = [
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
