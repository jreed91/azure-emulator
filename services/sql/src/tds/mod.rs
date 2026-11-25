//! The TDS endpoint: login, SQL batches, RPC calls and transaction requests.

pub mod codec;
pub mod wire;

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use emulator_core::Engine;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::rustls::{self, ServerConfig};

use crate::catalog::SqlState;
use crate::exec::{Arg, Event, Executor, OutputValue, Reported, Session, SqlError};
use crate::value::Value;
use codec::*;
use wire::*;

/// TLS settings for both ways clients encrypt.
pub struct TlsConfigs {
    /// TDS 7.x: the handshake runs inside PRELOGIN packets; TLS 1.2, as clients expect.
    pub prelogin: Arc<ServerConfig>,
    /// TDS 8 (`Encrypt=strict`): TLS first, with the `tds/8.0` protocol.
    pub strict: Arc<ServerConfig>,
}

/// Builds the TLS settings from a PEM certificate chain and key.
pub fn tls_configs(chain_pem: &str, key_pem: &str) -> Result<TlsConfigs, String> {
    let read = || -> Result<(Vec<rustls::pki_types::CertificateDer<'static>>, rustls::pki_types::PrivateKeyDer<'static>), String> {
        let chain = rustls_pemfile::certs(&mut chain_pem.as_bytes()).collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        let key = rustls_pemfile::private_key(&mut key_pem.as_bytes()).map_err(|e| e.to_string())?.ok_or("the private key PEM holds no key")?;
        Ok((chain, key))
    };
    let (chain, key) = read()?;
    let prelogin = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| e.to_string())?;
    let (chain, key) = read()?;
    let mut strict = ServerConfig::builder().with_no_client_auth().with_single_cert(chain, key).map_err(|e| e.to_string())?;
    strict.alpn_protocols = vec![b"tds/8.0".to_vec()];
    strict.send_tls13_tickets = 0;
    Ok(TlsConfigs { prelogin: Arc::new(prelogin), strict: Arc::new(strict) })
}

static NEXT_SPID: AtomicU16 = AtomicU16::new(51);

const ENCRYPT_OFF: u8 = 0;
const ENCRYPT_ON: u8 = 1;
const ENCRYPT_NOT_SUP: u8 = 2;
const ENCRYPT_REQ: u8 = 3;

fn prelogin_encryption(data: &[u8]) -> u8 {
    let mut i = 0;
    while i + 5 <= data.len() && data[i] != 0xFF {
        let token = data[i];
        let offset = u16::from_be_bytes([data[i + 1], data[i + 2]]) as usize;
        let length = u16::from_be_bytes([data[i + 3], data[i + 4]]) as usize;
        if token == 1 && length >= 1 {
            return data.get(offset).copied().unwrap_or(ENCRYPT_NOT_SUP);
        }
        i += 5;
    }
    ENCRYPT_NOT_SUP
}

fn prelogin_response(encryption: u8) -> Vec<u8> {
    let options: [(u8, Vec<u8>); 5] = [
        (0, vec![16, 0, 0x03, 0xE8, 0, 0]),
        (1, vec![encryption]),
        (2, vec![0]),
        (3, Vec::new()),
        (4, vec![0]),
    ];
    let header_length = options.len() * 5 + 1;
    let mut header = Vec::new();
    let mut body = Vec::new();
    for (token, data) in &options {
        header.push(*token);
        header.extend_from_slice(&((header_length + body.len()) as u16).to_be_bytes());
        header.extend_from_slice(&(data.len() as u16).to_be_bytes());
        body.extend_from_slice(data);
    }
    header.push(0xFF);
    header.extend(body);
    header
}

struct Login {
    tds_version: u32,
    packet_size: u32,
    integrated: bool,
    host: String,
    user: String,
    password: String,
    app: String,
    database: String,
}

fn login_field(data: &[u8], at: usize, password: bool) -> io::Result<String> {
    let mut header = codec::Reader::new(data);
    header.bytes(at)?;
    let offset = header.u16()? as usize;
    let chars = header.u16()? as usize;
    let bytes = data.get(offset..offset + chars * 2).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "LOGIN7 field out of range"))?;
    let decoded: Vec<u8> = if password { bytes.iter().map(|b| (b ^ 0xA5).rotate_left(4)).collect() } else { bytes.to_vec() };
    let units: Vec<u16> = decoded.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Ok(String::from_utf16_lossy(&units))
}

fn parse_login(data: &[u8]) -> io::Result<Login> {
    let mut r = codec::Reader::new(data);
    r.u32()?;
    let tds_version = r.u32()?;
    let packet_size = r.u32()?;
    r.bytes(12)?;
    let _flags1 = r.u8()?;
    let flags2 = r.u8()?;
    Ok(Login {
        tds_version,
        packet_size,
        integrated: flags2 & 0x80 != 0,
        host: login_field(data, 36, false)?,
        user: login_field(data, 40, false)?,
        password: login_field(data, 44, true)?,
        app: login_field(data, 48, false)?,
        database: login_field(data, 68, false)?,
    })
}

/// Serves one TDS connection until the client leaves.
pub async fn serve_stream<S: AsyncRead + AsyncWrite + Unpin>(engine: Arc<Engine>, stream: S, tls: Option<Arc<TlsConfigs>>) {
    let spid = NEXT_SPID.fetch_add(1, Ordering::Relaxed).max(51);
    let mut conn = Conn::new(stream, spid);
    let _ = serve(&engine, &mut conn, tls).await;
}

/// Per-connection state kept outside the engine: prepared statements.
#[derive(Default)]
struct Prepared {
    next: i32,
    statements: HashMap<i32, (String, String)>,
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(engine: &Engine, conn: &mut Conn<S>, tls: Option<Arc<TlsConfigs>>) -> io::Result<()> {
    let state = engine.state::<SqlState>();
    // TDS 8 starts with a TLS ClientHello.
    let strict = match (conn.peek_byte().await?, &tls) {
        (Some(0x16), Some(tls)) => {
            conn.start_tls_strict(tls.strict.clone()).await?;
            true
        }
        (None, _) => return Ok(()),
        _ => false,
    };
    let Some(prelogin) = conn.read_message().await? else { return Ok(()) };
    if prelogin.kind != PACKET_PRELOGIN {
        return Ok(());
    }
    let wanted = prelogin_encryption(&prelogin.data);
    let encryption = if strict {
        ENCRYPT_NOT_SUP
    } else {
        match (wanted, &tls) {
            (ENCRYPT_ON | ENCRYPT_REQ, Some(_)) => ENCRYPT_ON,
            (ENCRYPT_OFF, _) | (ENCRYPT_NOT_SUP, _) | (_, None) => ENCRYPT_NOT_SUP,
            _ => ENCRYPT_NOT_SUP,
        }
    };
    conn.write_message(PACKET_REPLY, &prelogin_response(encryption)).await?;
    if encryption == ENCRYPT_ON {
        if let Some(tls) = &tls {
            conn.start_tls_in_prelogin(tls.prelogin.clone()).await?;
        }
    }

    // LOGIN7.
    let Some(message) = conn.read_message().await? else { return Ok(()) };
    if message.kind != PACKET_LOGIN7 {
        return Ok(());
    }
    let login = parse_login(&message.data)?;
    let mut tokens = Tokens::default();
    let fail = |tokens: &mut Tokens, number: i32, text: String| {
        tokens.message(true, &line_one(SqlError::new(number, 14, text)));
        tokens.done(TOKEN_DONE, DONE_ERROR, 0);
    };
    if login.integrated || login.user.is_empty() {
        fail(&mut tokens, 18456, "Login failed. The emulator's SQL data plane accepts SQL authentication (a login and password) only.".into());
        conn.write_message(PACKET_REPLY, &tokens.buf).await?;
        return Ok(());
    }
    let (login_name, database) = {
        let guard = state.lock();
        match guard.authenticate(&login.user, &login.password) {
            None => (None, None),
            Some(found) => {
                let database = if login.database.is_empty() { found.default_database.clone() } else { login.database.clone() };
                let database = guard.database(&database).map(|d| d.name.clone()).ok_or(database);
                (Some(found.name.clone()), Some(database))
            }
        }
    };
    let Some(login_name) = login_name else {
        fail(&mut tokens, 18456, format!("Login failed for user '{}'.", login.user));
        conn.write_message(PACKET_REPLY, &tokens.buf).await?;
        return Ok(());
    };
    let database = match database {
        Some(Ok(d)) => d,
        Some(Err(name)) => {
            tokens.message(true, &line_one(SqlError::new(4060, 11, format!("Cannot open database \"{name}\" requested by the login. The login failed."))));
            fail(&mut tokens, 18456, format!("Login failed for user '{}'.", login.user));
            conn.write_message(PACKET_REPLY, &tokens.buf).await?;
            return Ok(());
        }
        None => "master".into(),
    };
    let tds_version = login.tds_version.clamp(0x7100_0001, 0x7400_0004);
    let packet_size = (login.packet_size as usize).clamp(512, 32767);
    let packet_size = if login.packet_size == 0 { 4096 } else { packet_size };
    tokens.env_change_text(1, &database, "master");
    tokens.message(false, &line_one(SqlError::new(5701, 0, format!("Changed database context to '{database}'.")).state(2)));
    tokens.env_change_bytes(7, &COLLATION, &[]);
    tokens.env_change_text(2, "us_english", "");
    tokens.message(false, &line_one(SqlError::new(5703, 0, "Changed language setting to us_english.")));
    tokens.login_ack(tds_version);
    tokens.env_change_text(4, &packet_size.to_string(), "4096");
    tokens.done(TOKEN_DONE, 0, 0);
    conn.write_message(PACKET_REPLY, &tokens.buf).await?;
    conn.packet_size = packet_size;

    let mut session = Session::new(conn.spid, &login_name, &database);
    session.app_name = login.app.clone();
    session.host_name = login.host.clone();
    let mut prepared = Prepared::default();
    let result = requests(&state, conn, &mut session, &mut prepared, &database).await;
    // A connection that ends rolls back its open transaction.
    if session.in_transaction() {
        let mut guard = state.lock();
        let mut out = Vec::new();
        Executor::new(&mut guard, &mut session, &mut out).rollback_all();
    }
    result
}

async fn requests<S: AsyncRead + AsyncWrite + Unpin>(
    state: &emulator_core::engine::ServiceState<SqlState>,
    conn: &mut Conn<S>,
    session: &mut Session,
    prepared: &mut Prepared,
    login_database: &str,
) -> io::Result<()> {
    loop {
        let Some(message) = conn.read_message().await? else { return Ok(()) };
        let mut tokens = Tokens::default();
        if message.kind == PACKET_ATTENTION {
            tokens.done(TOKEN_DONE, DONE_ATTN, 0);
            conn.write_message(PACKET_REPLY, &tokens.buf).await?;
            continue;
        }
        let mut events = Vec::new();
        let reset = message.status & (STATUS_RESET | STATUS_RESET_KEEP_TRANSACTION) != 0;
        let mut final_token = TOKEN_DONE;
        let mut trailer = Tokens::default();
        {
            let mut guard = state.lock();
            let mut exec = Executor::new(&mut guard, session, &mut events);
            if reset {
                if message.status & STATUS_RESET != 0 {
                    exec.rollback_all();
                }
                exec.reset_session(login_database);
            }
            let mut r = codec::Reader::new(&message.data);
            match message.kind {
                PACKET_SQL_BATCH => {
                    r.skip_all_headers()?;
                    let chars = r.remaining() / 2;
                    let text = r.utf16(chars)?;
                    exec.run_batch(&text);
                }
                PACKET_RPC => {
                    r.skip_all_headers()?;
                    final_token = TOKEN_DONEPROC;
                    loop {
                        let failed = rpc(&mut exec, &mut r, prepared, &mut trailer)?;
                        // Several calls in one request are separated by 0x80 (or 0xFF).
                        if r.remaining() > 0 && matches!(r.u8()?, 0x80 | 0xFF) && !failed {
                            continue;
                        }
                        break;
                    }
                }
                PACKET_TRANSACTION => {
                    r.skip_all_headers()?;
                    let request = r.u16()?;
                    let result = match request {
                        5 => {
                            let _isolation = r.u8()?;
                            exec.begin_transaction();
                            Ok(())
                        }
                        7 => {
                            let _name = r.b_varchar()?;
                            let begin_new = r.remaining() > 0 && r.u8()? & 0x01 != 0;
                            let result = exec.commit();
                            if result.is_ok() && begin_new {
                                exec.begin_transaction();
                            }
                            result
                        }
                        8 => {
                            let name = r.b_varchar()?;
                            let begin_new = r.remaining() > 0 && r.u8()? & 0x01 != 0;
                            let result = exec.rollback(if name.is_empty() { None } else { Some(&name) });
                            if result.is_ok() && begin_new {
                                exec.begin_transaction();
                            }
                            result
                        }
                        9 => {
                            let name = r.b_varchar()?;
                            exec.save_transaction(&name)
                        }
                        _ => Err(SqlError::new(40517, 16, "This transaction manager request is not supported by the Azure emulator's SQL data plane.")),
                    };
                    if let Err(error) = result {
                        exec.report_error(error);
                    }
                }
                _ => return Ok(()),
            }
        }
        if reset {
            tokens.env_change_bytes(18, &[], &[]);
        }
        let in_transaction = session.in_transaction();
        encode(&mut tokens, &events, in_transaction);
        tokens.buf.extend_from_slice(&trailer.buf);
        let had_error = events.iter().any(|e| matches!(e, Event::Error(_)));
        let mut status = if in_transaction { DONE_INXACT } else { 0 };
        if had_error && final_token == TOKEN_DONEPROC {
            status |= DONE_ERROR;
        }
        tokens.done(final_token, status, 0);
        conn.write_message(PACKET_REPLY, &tokens.buf).await?;
    }
}

/// Runs one RPC call. Returns whether it failed.
fn rpc(exec: &mut Executor, r: &mut codec::Reader, prepared: &mut Prepared, trailer: &mut Tokens) -> io::Result<bool> {
    let name_length = r.u16()?;
    let procedure = if name_length == 0xFFFF {
        match r.u16()? {
            10 => "sp_executesql".to_string(),
            11 => "sp_prepare".to_string(),
            12 => "sp_execute".to_string(),
            13 => "sp_prepexec".to_string(),
            15 => "sp_unprepare".to_string(),
            other => format!("#{other}"),
        }
    } else {
        r.utf16(name_length as usize)?
    };
    let _options = r.u16()?;
    let mut args = Vec::new();
    while r.remaining() > 0 {
        // The batch separator ends this call's parameters.
        let next = r.bytes(1)?[0];
        r.pos -= 1;
        if next == 0x80 || next == 0xFF {
            break;
        }
        let name = r.b_varchar()?;
        let status = r.u8()?;
        let wire = codec::read_type_info(r)?;
        let value = codec::read_value(r, &wire)?;
        args.push(Arg { name: if name.is_empty() { None } else { Some(name) }, value, ty: wire.sql_type(), output: status & 0x01 != 0, default: status & 0x02 != 0 });
    }
    let lower = procedure.to_lowercase();
    let result: Result<(i32, Vec<OutputValue>), Reported> = match lower.as_str() {
        "sp_executesql" => {
            let text = args.first().and_then(|a| a.value.as_string()).unwrap_or_default();
            let params = args.get(1).and_then(|a| a.value.as_string()).unwrap_or_default();
            let rest: Vec<Arg> = args.iter().skip(2).cloned().collect();
            exec.execute_sql(&text, &params, rest).map(|outputs| (0, outputs.into_iter().map(|o| OutputValue { ordinal: o.ordinal + 2, ..o }).collect()))
        }
        "sp_prepare" | "sp_prepexec" => {
            // @handle OUTPUT, @params, @stmt[, @options][, values for sp_prepexec]
            let params = args.get(1).and_then(|a| a.value.as_string()).unwrap_or_default();
            let text = args.get(2).and_then(|a| a.value.as_string()).unwrap_or_default();
            prepared.next += 1;
            let handle = prepared.next;
            prepared.statements.insert(handle, (text.clone(), params.clone()));
            let handle_output = OutputValue { ordinal: 0, name: args.first().and_then(|a| a.name.clone()).unwrap_or_else(|| "@handle".into()), value: Value::Int(handle as i64), ty: crate::value::SqlType::Int };
            if lower == "sp_prepexec" {
                let rest: Vec<Arg> = args.iter().skip(3).cloned().collect();
                exec.execute_sql(&text, &params, rest).map(|outputs| {
                    let mut all = vec![handle_output];
                    all.extend(outputs.into_iter().map(|o| OutputValue { ordinal: o.ordinal + 3, ..o }));
                    (0, all)
                })
            } else {
                Ok((0, vec![handle_output]))
            }
        }
        "sp_execute" => {
            let handle = args.first().map(|a| a.value.as_i64().ok().flatten().unwrap_or(0)).unwrap_or(0) as i32;
            match prepared.statements.get(&handle).cloned() {
                Some((text, params)) => {
                    let rest: Vec<Arg> = args.iter().skip(1).cloned().collect();
                    exec.execute_sql(&text, &params, rest).map(|outputs| (0, outputs.into_iter().map(|o| OutputValue { ordinal: o.ordinal + 1, ..o }).collect()))
                }
                None => {
                    exec.report_error(SqlError::new(8179, 16, format!("Could not find prepared statement with handle {handle}.")));
                    Err(Reported)
                }
            }
        }
        "sp_unprepare" => {
            let handle = args.first().map(|a| a.value.as_i64().ok().flatten().unwrap_or(0)).unwrap_or(0) as i32;
            prepared.statements.remove(&handle);
            Ok((0, Vec::new()))
        }
        _ if procedure.starts_with('#') => {
            exec.report_error(SqlError::new(40517, 16, format!("The special procedure {procedure} is not supported by the Azure emulator's SQL data plane.")));
            Err(Reported)
        }
        _ => exec.call(&procedure, args),
    };
    match result {
        Ok((status, outputs)) => {
            trailer.return_status(status);
            for output in outputs {
                trailer.return_value(output.ordinal, &output.name, &output.ty, &output.value);
            }
            Ok(false)
        }
        Err(Reported) => Ok(true),
    }
}

/// Turns the executor's events into tokens.
fn encode(tokens: &mut Tokens, events: &[Event], in_transaction: bool) {
    let inxact = if in_transaction { DONE_INXACT } else { 0 };
    for event in events {
        match event {
            Event::ResultSet { columns, rows } => tokens.result_set(columns, rows),
            Event::Done { count, in_proc, error } => {
                let mut status = DONE_MORE | inxact;
                if count.is_some() {
                    status |= DONE_COUNT;
                }
                if *error {
                    status |= DONE_ERROR;
                }
                tokens.done(if *in_proc { TOKEN_DONEINPROC } else { TOKEN_DONE }, status, count.unwrap_or(0));
            }
            Event::DoneProc { count, error } => {
                let mut status = DONE_MORE | inxact;
                if count.is_some() {
                    status |= DONE_COUNT;
                }
                if *error {
                    status |= DONE_ERROR;
                }
                tokens.done(TOKEN_DONEPROC, status, count.unwrap_or(0));
            }
            Event::ReturnStatus(status) => tokens.return_status(*status),
            Event::Info { number, class, state, message, line, procedure } => {
                let info = SqlError { line: *line, procedure: procedure.clone(), ..SqlError::new(*number, *class, message.clone()).state(*state) };
                tokens.message(false, &info)
            }
            Event::Error(error) => tokens.error(error),
            Event::Database { new, old } => tokens.env_change_text(1, new, old),
            Event::BeginTransaction(d) => tokens.env_change_bytes(8, &d.to_le_bytes(), &[]),
            Event::CommitTransaction(d) => tokens.env_change_bytes(9, &[], &d.to_le_bytes()),
            Event::RollbackTransaction(d) => tokens.env_change_bytes(10, &[], &d.to_le_bytes()),
        }
    }
}

/// A login-time message, which SQL Server reports on line 1.
fn line_one(error: SqlError) -> SqlError {
    SqlError { line: 1, ..error }
}
