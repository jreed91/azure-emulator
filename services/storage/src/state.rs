//! In-memory state of the Storage service.

use std::collections::BTreeMap;
use std::time::SystemTime;

use axum::body::Bytes;

#[derive(Default)]
pub struct StorageState {
    /// Keyed by account name, which is globally unique.
    pub(crate) accounts: BTreeMap<String, Account>,
    etag_sequence: u64,
}

impl StorageState {
    /// A new entity tag, unique within this emulator, in the service's format.
    pub(crate) fn next_etag(&mut self) -> String {
        self.etag_sequence += 1;
        format!("\"0x8DE{:012X}\"", self.etag_sequence)
    }
}

pub(crate) struct Account {
    /// `key1` and `key2`, base64 encoded.
    pub keys: [String; 2],
    pub key_created: [String; 2],
    pub containers: BTreeMap<String, Container>,
}

pub(crate) struct Container {
    pub etag: String,
    pub last_modified: SystemTime,
    pub metadata: Vec<(String, String)>,
    pub blobs: BTreeMap<String, Blob>,
}

#[derive(Clone)]
pub(crate) struct Blob {
    pub data: Bytes,
    pub content_type: String,
    /// Base64 MD5 of the content.
    pub content_md5: String,
    pub etag: String,
    pub created: SystemTime,
    pub last_modified: SystemTime,
    pub metadata: Vec<(String, String)>,
}
