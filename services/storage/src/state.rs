//! In-memory state of the Storage service.

use std::collections::BTreeMap;
use std::time::SystemTime;

use axum::body::Bytes;

#[derive(Default)]
pub struct StorageState {
    /// Keyed by account name, which is globally unique.
    pub(crate) accounts: BTreeMap<String, Account>,
    /// File share leases, keyed by the share's lowercased resource id.
    pub(crate) share_leases: BTreeMap<String, Lease>,
    /// Lowercased local user id → its shared key, minted on first use.
    pub(crate) local_user_keys: BTreeMap<String, String>,
    /// How many local users have ever been created; numbers their user ids.
    pub(crate) local_user_count: u64,
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
    /// Object replication policies installed on this account, by policy id.
    pub replication: BTreeMap<String, crate::replication::Policy>,
}

pub(crate) struct Container {
    pub etag: String,
    pub last_modified: SystemTime,
    pub metadata: Vec<(String, String)>,
    /// `None`, `Blob` or `Container`: what anonymous callers may read.
    pub public_access: String,
    /// Settings only Resource Manager can set, fixed when the container is created.
    pub default_encryption_scope: String,
    pub deny_encryption_scope_override: bool,
    /// Tags of the legal hold, if any; while there is one, no blob can be
    /// overwritten or deleted.
    pub legal_hold: Vec<LegalHoldTag>,
    /// The time-based retention policy, if any.
    pub immutability: Option<ImmutabilityPolicy>,
    pub lease: Option<Lease>,
    pub blobs: BTreeMap<String, Blob>,
}

pub(crate) struct LegalHoldTag {
    pub tag: String,
    pub timestamp: String,
}

pub(crate) struct ImmutabilityPolicy {
    pub days: i64,
    pub locked: bool,
    pub allow_protected_append_writes: Option<bool>,
    pub allow_protected_append_writes_all: Option<bool>,
    pub etag: String,
    /// `(update, days, timestamp)`: how the policy got where it is.
    pub history: Vec<(&'static str, i64, String)>,
}

pub(crate) struct Lease {
    pub id: String,
    /// Seconds, or `None` for an infinite lease.
    pub duration: Option<u64>,
    pub acquired: SystemTime,
    /// When a broken lease stops blocking, once a break has been asked for.
    pub broken_at: Option<SystemTime>,
}

/// A container lease's state at one moment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LeaseState {
    Available,
    Leased,
    Expired,
    Breaking,
    Broken,
}

impl LeaseState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Leased => "Leased",
            Self::Expired => "Expired",
            Self::Breaking => "Breaking",
            Self::Broken => "Broken",
        }
    }

    /// Whether the lease currently blocks writes and deletion.
    pub(crate) fn blocks(self) -> bool {
        matches!(self, Self::Leased | Self::Breaking)
    }
}

/// The state of `lease` (container or file share) at `now`.
pub(crate) fn lease_state(lease: Option<&Lease>, now: SystemTime) -> LeaseState {
    let Some(lease) = lease else {
        return LeaseState::Available;
    };
    if let Some(broken_at) = lease.broken_at {
        return if now >= broken_at { LeaseState::Broken } else { LeaseState::Breaking };
    }
    match lease.duration {
        Some(seconds) if now >= lease.acquired + std::time::Duration::from_secs(seconds) => LeaseState::Expired,
        _ => LeaseState::Leased,
    }
}

/// Why a blob cannot be overwritten or deleted.
pub(crate) enum Protection {
    LegalHold,
    Retention,
}

impl Container {
    /// A new, empty, private container.
    pub(crate) fn new(etag: String, now: SystemTime, metadata: Vec<(String, String)>) -> Self {
        Self {
            etag,
            last_modified: now,
            metadata,
            public_access: "None".into(),
            default_encryption_scope: "$account-encryption-key".into(),
            deny_encryption_scope_override: false,
            legal_hold: Vec::new(),
            immutability: None,
            lease: None,
            blobs: BTreeMap::new(),
        }
    }

    pub(crate) fn lease_state(&self, now: SystemTime) -> LeaseState {
        lease_state(self.lease.as_ref(), now)
    }

    /// Whether `blob` is protected from being overwritten or deleted now.
    pub(crate) fn protection(&self, blob: &Blob, now: SystemTime) -> Option<Protection> {
        if !self.legal_hold.is_empty() {
            return Some(Protection::LegalHold);
        }
        let policy = self.immutability.as_ref()?;
        let retained_until = blob.created + std::time::Duration::from_secs(policy.days.max(0) as u64 * 86_400);
        (now < retained_until).then_some(Protection::Retention)
    }

    /// Why the container itself cannot be deleted, if it cannot: a legal hold,
    /// or a blob still under retention.
    pub(crate) fn deletion_blocker(&self, now: SystemTime) -> Option<Protection> {
        if !self.legal_hold.is_empty() {
            return Some(Protection::LegalHold);
        }
        self.blobs.values().find_map(|blob| self.protection(blob, now))
    }
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
    /// On a source blob: the `{policy}_{rule}` pairs it has been replicated by.
    pub replicated_by: Vec<String>,
    /// On a replica: the policy that wrote it.
    pub replica_of: Option<String>,
}
