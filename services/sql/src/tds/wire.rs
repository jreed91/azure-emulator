//! TDS framing over TCP, with TLS either inside PRELOGIN packets (TDS 7.x) or
//! around the whole connection (TDS 8, `Encrypt=strict`).

use std::io::{self, Read, Write};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::rustls::{ServerConfig, ServerConnection};

pub const PACKET_SQL_BATCH: u8 = 1;
pub const PACKET_RPC: u8 = 3;
pub const PACKET_REPLY: u8 = 4;
pub const PACKET_ATTENTION: u8 = 6;
pub const PACKET_TRANSACTION: u8 = 14;
pub const PACKET_LOGIN7: u8 = 16;
pub const PACKET_PRELOGIN: u8 = 18;

pub const STATUS_EOM: u8 = 0x01;
pub const STATUS_RESET: u8 = 0x08;
pub const STATUS_RESET_KEEP_TRANSACTION: u8 = 0x10;

/// The largest message the emulator accepts, to bound memory.
const MAX_MESSAGE: usize = 256 * 1024 * 1024;

pub struct Message {
    pub kind: u8,
    pub status: u8,
    pub data: Vec<u8>,
}

pub struct Conn<S> {
    stream: S,
    tls: Option<ServerConnection>,
    /// Plain bytes read but not yet used.
    input: Vec<u8>,
    pub packet_size: usize,
    pub spid: u16,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conn<S> {
    pub fn new(stream: S, spid: u16) -> Self {
        Conn { stream, tls: None, input: Vec::new(), packet_size: 4096, spid }
    }

    /// Reads more plain bytes into `input`. Returns false at the end of the stream.
    async fn fill(&mut self) -> io::Result<bool> {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let n = self.stream.read(&mut buffer).await?;
            if n == 0 {
                return Ok(false);
            }
            match &mut self.tls {
                None => {
                    self.input.extend_from_slice(&buffer[..n]);
                    return Ok(true);
                }
                Some(tls) => {
                    let mut slice = &buffer[..n];
                    while !slice.is_empty() {
                        tls.read_tls(&mut slice)?;
                        tls.process_new_packets().map_err(|e| invalid(&e.to_string()))?;
                    }
                    let before = self.input.len();
                    let mut plain = [0u8; 16 * 1024];
                    loop {
                        match tls.reader().read(&mut plain) {
                            Ok(0) => break,
                            Ok(m) => self.input.extend_from_slice(&plain[..m]),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) => return Err(e),
                        }
                    }
                    self.flush_tls().await?;
                    if self.input.len() > before {
                        return Ok(true);
                    }
                }
            }
        }
    }

    async fn flush_tls(&mut self) -> io::Result<()> {
        if let Some(tls) = &mut self.tls {
            let mut out = Vec::new();
            while tls.wants_write() {
                tls.write_tls(&mut out)?;
            }
            if !out.is_empty() {
                self.stream.write_all(&out).await?;
                self.stream.flush().await?;
            }
        }
        Ok(())
    }

    async fn need(&mut self, n: usize) -> io::Result<bool> {
        while self.input.len() < n {
            if !self.fill().await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The first byte of the connection, without consuming it.
    pub async fn peek_byte(&mut self) -> io::Result<Option<u8>> {
        if !self.need(1).await? {
            return Ok(None);
        }
        Ok(Some(self.input[0]))
    }

    /// Reads one message (all its packets). `None` when the client has gone.
    pub async fn read_message(&mut self) -> io::Result<Option<Message>> {
        let mut data = Vec::new();
        let mut first_status = None;
        loop {
            if !self.need(8).await? {
                return Ok(None);
            }
            let length = u16::from_be_bytes([self.input[2], self.input[3]]) as usize;
            if length < 8 {
                return Err(invalid("TDS packet shorter than its header"));
            }
            if !self.need(length).await? {
                return Ok(None);
            }
            let packet: Vec<u8> = self.input.drain(..length).collect();
            let kind = packet[0];
            let status = packet[1];
            first_status.get_or_insert(status);
            data.extend_from_slice(&packet[8..]);
            if data.len() > MAX_MESSAGE {
                return Err(invalid("TDS message too large"));
            }
            if status & STATUS_EOM != 0 {
                return Ok(Some(Message { kind, status: first_status.unwrap_or(0), data }));
            }
        }
    }

    /// Writes one message, split into packets of the negotiated size.
    pub async fn write_message(&mut self, kind: u8, data: &[u8]) -> io::Result<()> {
        let room = self.packet_size.max(512) - 8;
        let mut out = Vec::with_capacity(data.len() + 8 * (data.len() / room + 1));
        let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(room).collect() };
        let last = chunks.len() - 1;
        for (i, chunk) in chunks.iter().enumerate() {
            out.push(kind);
            out.push(if i == last { STATUS_EOM } else { 0 });
            out.extend_from_slice(&((chunk.len() + 8) as u16).to_be_bytes());
            out.extend_from_slice(&self.spid.to_be_bytes());
            out.push((i + 1) as u8);
            out.push(0);
            out.extend_from_slice(chunk);
        }
        match &mut self.tls {
            None => {
                self.stream.write_all(&out).await?;
                self.stream.flush().await
            }
            Some(tls) => {
                tls.writer().write_all(&out)?;
                self.flush_tls().await
            }
        }
    }

    /// TLS whose handshake travels in PRELOGIN messages (TDS 7.x `Encrypt=true`).
    pub async fn start_tls_in_prelogin(&mut self, config: Arc<ServerConfig>) -> io::Result<()> {
        let mut tls = ServerConnection::new(config).map_err(|e| invalid(&e.to_string()))?;
        while tls.is_handshaking() {
            let Some(message) = self.read_message().await? else {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client left during the TLS handshake"));
            };
            if message.kind != PACKET_PRELOGIN {
                return Err(invalid("expected a TLS handshake in a PRELOGIN packet"));
            }
            let mut slice = message.data.as_slice();
            while !slice.is_empty() {
                tls.read_tls(&mut slice)?;
                if let Err(e) = tls.process_new_packets() {
                    let mut alert = Vec::new();
                    let _ = tls.write_tls(&mut alert);
                    if !alert.is_empty() {
                        let _ = self.write_message(PACKET_PRELOGIN, &alert).await;
                    }
                    return Err(invalid(&e.to_string()));
                }
            }
            let mut reply = Vec::new();
            while tls.wants_write() {
                tls.write_tls(&mut reply)?;
            }
            if !reply.is_empty() {
                self.write_message(PACKET_PRELOGIN, &reply).await?;
            }
        }
        self.tls = Some(tls);
        Ok(())
    }

    /// TLS around the whole connection (TDS 8): the client's first bytes are a ClientHello.
    pub async fn start_tls_strict(&mut self, config: Arc<ServerConfig>) -> io::Result<()> {
        let mut tls = ServerConnection::new(config).map_err(|e| invalid(&e.to_string()))?;
        let pending: Vec<u8> = std::mem::take(&mut self.input);
        let mut slice = pending.as_slice();
        while !slice.is_empty() {
            tls.read_tls(&mut slice)?;
            tls.process_new_packets().map_err(|e| invalid(&e.to_string()))?;
        }
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let mut out = Vec::new();
            while tls.wants_write() {
                tls.write_tls(&mut out)?;
            }
            if !out.is_empty() {
                self.stream.write_all(&out).await?;
                self.stream.flush().await?;
            }
            if !tls.is_handshaking() {
                break;
            }
            let n = self.stream.read(&mut buffer).await?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client left during the TLS handshake"));
            }
            let mut slice = &buffer[..n];
            while !slice.is_empty() {
                tls.read_tls(&mut slice)?;
                tls.process_new_packets().map_err(|e| invalid(&e.to_string()))?;
            }
        }
        // Application data that arrived with the handshake's last flight.
        let mut plain = [0u8; 16 * 1024];
        loop {
            match tls.reader().read(&mut plain) {
                Ok(0) => break,
                Ok(m) => self.input.extend_from_slice(&plain[..m]),
                Err(_) => break,
            }
        }
        self.tls = Some(tls);
        Ok(())
    }

    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }
}
