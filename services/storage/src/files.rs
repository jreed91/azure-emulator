//! The Azure Files data plane, on `<account>.file.core.<suffix>`.
//!
//! Implemented: shares (list, create, properties, metadata, quota, stats,
//! delete), directories (create, properties, metadata, list files and
//! directories, delete when empty) and files (create at a size, put and
//! clear ranges, download with ranges, properties, HTTP headers and resize,
//! metadata, list ranges, delete). Names are case-insensitive and keep the
//! case they were created with, as in Azure Files. Requests are authorized
//! with Shared Key, an account SAS or a file or share SAS signed with an
//! account key, or an Entra ID token. Shares are the same ones Resource
//! Manager lists under `fileServices/default/shares`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use axum::body::{Body, Bytes};
use axum::response::Response;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use emulator_core::arm::{ArmState, Resource};
use emulator_core::auth::{self, AuthFailure};
use emulator_core::http_util::{parse_query, percent_decode, read_body, request_id};
use emulator_core::Engine;
use http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use md5::{Digest, Md5};
use serde_json::json;

use crate::sas;
use crate::shared_key;
use crate::state::StorageState;

const DEFAULT_VERSION: &str = "2025-05-05";
const SHARE_TYPE: &str = "Microsoft.Storage/storageAccounts/fileServices/shares";
/// The largest file the emulator makes; files are held in memory.
const MAX_FILE: u64 = 256 * 1024 * 1024;
/// The permission key every file and directory reports.
const PERMISSION_KEY: &str = "4066528134148476695*1";

/// One share: its settings, and every directory and file in it by
/// lowercased path (`dir/sub/file`); the root directory is the empty path.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Share {
    pub metadata: BTreeMap<String, String>,
    /// The quota in GiB.
    pub quota: u64,
    pub etag: String,
    pub last_modified: SystemTime,
    items: BTreeMap<String, Item>,
    next_id: u64,
    /// The `SignedIdentifiers` XML last set, as sent.
    acl: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Item {
    /// The last path segment, as created.
    name: String,
    directory: bool,
    id: u64,
    parent_id: u64,
    metadata: BTreeMap<String, String>,
    etag: String,
    created: SystemTime,
    last_write: SystemTime,
    changed: SystemTime,
    /// Files only: content, HTTP headers and the byte ranges written.
    #[serde(with = "crate::persist::bytes")]
    data: Bytes,
    headers: BTreeMap<String, String>,
    ranges: Vec<(u64, u64)>,
}

impl Share {
    pub(crate) fn new(quota: u64, etag: String) -> Self {
        let now = SystemTime::now();
        let mut items = BTreeMap::new();
        items.insert(String::new(), Item::directory("", 0, 0, etag.clone(), BTreeMap::new()));
        Share { metadata: BTreeMap::new(), quota, etag, last_modified: now, items, next_id: 1, acl: None }
    }

    fn usage(&self) -> u64 {
        self.items.values().map(|i| i.data.len() as u64).sum()
    }
}

impl Item {
    fn directory(name: &str, id: u64, parent_id: u64, etag: String, metadata: BTreeMap<String, String>) -> Self {
        let now = SystemTime::now();
        Item {
            name: name.to_string(),
            directory: true,
            id,
            parent_id,
            metadata,
            etag,
            created: now,
            last_write: now,
            changed: now,
            data: Bytes::new(),
            headers: BTreeMap::new(),
            ranges: Vec::new(),
        }
    }

    fn file_id(&self) -> String {
        format!("{}", 13835128424026341376u64 + self.id)
    }
}

/// Adds `[start, end]` to merged ranges, or removes it.
fn mark(ranges: &mut Vec<(u64, u64)>, start: u64, end: u64, written: bool) {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(a, b) in ranges.iter() {
        if b < start || a > end {
            out.push((a, b));
            continue;
        }
        if !written {
            if a < start {
                out.push((a, start - 1));
            }
            if b > end {
                out.push((end + 1, b));
            }
        } else {
            out.push((a.min(start), b.max(end)));
        }
    }
    if written {
        out.push((start, end));
    }
    out.sort();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (a, b) in out {
        match merged.last_mut() {
            Some((_, last)) if a <= *last + 1 => *last = (*last).max(b),
            _ => merged.push((a, b)),
        }
    }
    *ranges = merged;
}

struct Failure {
    status: StatusCode,
    code: &'static str,
    message: String,
}

fn fail(status: StatusCode, code: &'static str, message: impl Into<String>) -> Failure {
    Failure { status, code, message: message.into() }
}

fn share_not_found() -> Failure {
    fail(StatusCode::NOT_FOUND, "ShareNotFound", "The specified share does not exist.")
}

fn resource_not_found() -> Failure {
    fail(StatusCode::NOT_FOUND, "ResourceNotFound", "The specified resource does not exist.")
}

fn parent_not_found() -> Failure {
    fail(StatusCode::NOT_FOUND, "ParentNotFound", "The specified parent path does not exist.")
}

fn bad_header() -> Failure {
    fail(StatusCode::BAD_REQUEST, "InvalidHeaderValue", "The value for one of the HTTP headers is not in the correct format.")
}

fn xml_escape(input: &str) -> String {
    input.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// XML without its leading `<?xml ...?>` declaration, whatever its quoting,
/// so it can be sent under one of our own.
pub(crate) fn without_declaration(xml: &str) -> &str {
    let xml = xml.trim_start();
    match xml.strip_prefix("<?xml").and_then(|rest| rest.find("?>").map(|end| &rest[end + 2..])) {
        Some(rest) => rest.trim_start(),
        None => xml,
    }
}

fn http_date(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// `2024-01-02T03:04:05.1234567Z`, as the `x-ms-file-*-time` headers carry times.
fn file_time(time: SystemTime) -> String {
    let nanos = DateTime::<Utc>::from(time).format("%Y-%m-%dT%H:%M:%S%.9f").to_string();
    format!("{}Z", &nanos[..nanos.len() - 2])
}

struct Reply {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Option<Body>,
}

fn reply(status: StatusCode, headers: Vec<(String, String)>) -> Reply {
    Reply { status, headers, body: None }
}

fn xml_reply(body: String) -> Reply {
    Reply {
        status: StatusCode::OK,
        headers: vec![("content-type".into(), "application/xml".into())],
        body: Some(Body::from(format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>{body}"))),
    }
}

/// Handles one Azure Files data-plane request.
pub async fn handle(engine: Arc<Engine>, account: String, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let id = request_id();
    let body = match read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let (status, mut headers, body) = match serve(&engine, &account, &parts, body) {
        Ok(r) => (r.status, r.headers, r.body),
        Err(f) => (
            f.status,
            vec![("x-ms-error-code".to_string(), f.code.to_string()), ("content-type".into(), "application/xml".into())],
            (parts.method != Method::HEAD).then(|| {
                Body::from(format!(
                    "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{}</Code><Message>{}\nRequestId:{id}\nTime:{}</Message></Error>",
                    f.code,
                    xml_escape(&f.message),
                    emulator_core::http_util::iso_timestamp()
                ))
            }),
        ),
    };
    let version = parts.headers.get("x-ms-version").and_then(|v| v.to_str().ok()).unwrap_or(DEFAULT_VERSION).to_string();
    headers.push(("x-ms-version".into(), version));
    headers.push(("x-ms-request-id".into(), id));
    if let Some(client) = parts.headers.get("x-ms-client-request-id").and_then(|v| v.to_str().ok()) {
        headers.push(("x-ms-client-request-id".into(), client.to_string()));
    }
    headers.push(("date".into(), http_date(SystemTime::now())));
    let mut builder = Response::builder().status(status).header(header::SERVER, "Azure-Emulator-File/0.1");
    for (name, value) in headers {
        if let Ok(value) = HeaderValue::from_str(&value) {
            builder = builder.header(name.as_str(), value);
        }
    }
    builder.body(body.unwrap_or_else(Body::empty)).expect("valid response")
}

/// The operations, for authorization.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    ListShares,
    ServiceProperties,
    SetServiceProperties,
    CreateShare,
    ShareProperties,
    DeleteShare,
    SetShareMetadata,
    SetShareProperties,
    ShareStats,
    ShareAcl,
    SetShareAcl,
    CreateDirectory,
    DirectoryProperties,
    DeleteDirectory,
    SetDirectoryMetadata,
    SetDirectoryProperties,
    List,
    CreateFile,
    GetFile,
    FileProperties,
    DeleteFile,
    PutRange,
    ListRanges,
    SetFileProperties,
    SetFileMetadata,
}

impl Op {
    /// The SAS resource level (`s`ervice, `c`ontainer, `o`bject) and the
    /// permissions any one of which allows the operation.
    fn sas(self) -> (char, &'static str) {
        use Op::*;
        match self {
            ListShares | ServiceProperties => ('s', "lr"),
            SetServiceProperties => ('s', "w"),
            CreateShare => ('c', "cw"),
            ShareProperties | ShareStats | ShareAcl => ('c', "r"),
            DeleteShare => ('c', "d"),
            SetShareMetadata | SetShareProperties | SetShareAcl => ('c', "w"),
            List => ('c', "l"),
            CreateDirectory | CreateFile => ('o', "cw"),
            DirectoryProperties | GetFile | FileProperties | ListRanges => ('o', "r"),
            DeleteDirectory | DeleteFile => ('o', "d"),
            SetDirectoryMetadata | SetDirectoryProperties | PutRange | SetFileProperties | SetFileMetadata => ('o', "w"),
        }
    }

    /// Whether the operation reads, writes or deletes, for role assignments.
    fn verb(self) -> &'static str {
        use Op::*;
        match self {
            ListShares | ServiceProperties | ShareProperties | ShareStats | ShareAcl | DirectoryProperties | List | GetFile | FileProperties | ListRanges => "read",
            DeleteShare | DeleteDirectory | DeleteFile => "delete",
            _ => "write",
        }
    }
}

fn route(method: &Method, segments: &[String], query: &BTreeMap<String, String>) -> Option<Op> {
    use Op::*;
    let comp = query.get("comp").map(String::as_str);
    let restype = query.get("restype").map(String::as_str);
    Some(match (segments.len(), method.as_str(), restype, comp) {
        (0, "GET", _, Some("list")) => ListShares,
        (0, "GET", Some("service"), Some("properties")) => ServiceProperties,
        (0, "PUT", Some("service"), Some("properties")) => SetServiceProperties,
        (1, "PUT", Some("share"), None) => CreateShare,
        (1, "GET" | "HEAD", Some("share"), None) => ShareProperties,
        (1, "DELETE", Some("share"), None) => DeleteShare,
        (1, "PUT", Some("share"), Some("metadata")) => SetShareMetadata,
        (1, "PUT", Some("share"), Some("properties")) => SetShareProperties,
        (1, "GET", Some("share"), Some("stats")) => ShareStats,
        (1, "GET" | "HEAD", Some("share"), Some("acl")) => ShareAcl,
        (1, "PUT", Some("share"), Some("acl")) => SetShareAcl,
        (_, "GET", Some("directory"), Some("list")) => List,
        (2.., "PUT", Some("directory"), None) => CreateDirectory,
        (_, "GET" | "HEAD", Some("directory"), None) => DirectoryProperties,
        (2.., "DELETE", Some("directory"), None) => DeleteDirectory,
        (_, "PUT", Some("directory"), Some("metadata")) => SetDirectoryMetadata,
        (_, "PUT", Some("directory"), Some("properties")) => SetDirectoryProperties,
        (2.., "PUT", None, None) => CreateFile,
        (2.., "GET", None, None) => GetFile,
        (2.., "HEAD", None, None) => FileProperties,
        (2.., "DELETE", None, None) => DeleteFile,
        (2.., "PUT", None, Some("range")) => PutRange,
        (2.., "GET", None, Some("rangelist")) => ListRanges,
        (2.., "PUT", None, Some("properties")) => SetFileProperties,
        (2.., "PUT", None, Some("metadata")) => SetFileMetadata,
        _ => return None,
    })
}

/// A file or share SAS's string to sign, for versions from 2015-04-05.
fn file_sas_string(account: &str, share: &str, path: &str, fields: &sas::Fields<'_>) -> String {
    let resource = match fields.get("sr") {
        "s" => format!("/file/{account}/{share}"),
        _ => format!("/file/{account}/{share}/{path}"),
    };
    let mut parts = vec![fields.get("sp"), fields.get("st"), fields.get("se")];
    let resource_ref = resource.as_str();
    parts.push(resource_ref);
    for name in ["si", "sip", "spr", "sv", "rscc", "rscd", "rsce", "rscl", "rsct"] {
        parts.push(fields.get(name));
    }
    parts.join("\n")
}

fn authorize(engine: &Engine, account: &str, parts: &http::request::Parts, query: &BTreeMap<String, String>, share: &str, path: &str, op: Op) -> Result<(), Failure> {
    let failed = || {
        fail(
            StatusCode::FORBIDDEN,
            "AuthenticationFailed",
            "Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature.",
        )
    };
    let keys = {
        let state = engine.state::<StorageState>();
        let state = state.lock();
        state.accounts.get(account).map(|a| a.keys.clone())
    };
    let authorization = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    if let Some(credential) = authorization.strip_prefix("SharedKey ") {
        let (signed_account, signature) = credential.split_once(':').ok_or_else(failed)?;
        let keys = keys.ok_or_else(failed)?;
        if signed_account != account {
            return Err(failed());
        }
        let to_sign = shared_key::string_to_sign(&parts.method, &parts.uri, &parts.headers, account);
        return keys.iter().any(|key| shared_key::verify(&to_sign, key, signature)).then_some(()).ok_or_else(failed);
    }
    if query.contains_key("sig") {
        let keys = keys.ok_or_else(failed)?;
        let fields = sas::Fields(query);
        if !fields.get("si").is_empty() {
            return Err(fail(StatusCode::NOT_IMPLEMENTED, "NotImplemented", "The emulator verifies shared access signatures signed with an account key only."));
        }
        let is_account = !fields.get("ss").is_empty() || !fields.get("srt").is_empty();
        let to_sign = if is_account { sas::account_string_to_sign(account, &fields) } else { file_sas_string(account, share, path, &fields) };
        if fields.get("sv").is_empty() || !sas::signature_matches(&keys, &to_sign, fields.get("sig")) {
            return Err(failed());
        }
        let now = Utc::now();
        let expires = sas::parse_time(fields.get("se")).ok_or_else(failed)?;
        if now >= expires || sas::parse_time(fields.get("st")).is_some_and(|start| now < start) {
            return Err(failed());
        }
        let (level, permissions) = op.sas();
        let mismatch = |code: &'static str| fail(StatusCode::FORBIDDEN, code, "This request is not authorized to perform this operation.");
        if is_account {
            if !fields.get("ss").contains('f') {
                return Err(mismatch("AuthorizationServiceMismatch"));
            }
            if !fields.get("srt").contains(level) {
                return Err(mismatch("AuthorizationResourceTypeMismatch"));
            }
        } else {
            // A share SAS covers the share's directories and files; a file
            // SAS, that file.
            let allowed = match fields.get("sr") {
                "s" => level != 's',
                "f" => level == 'o' && !path.is_empty(),
                _ => false,
            };
            if !allowed {
                return Err(mismatch("AuthorizationResourceTypeMismatch"));
            }
        }
        if !permissions.chars().any(|p| fields.get("sp").contains(p)) {
            return Err(mismatch("AuthorizationPermissionMismatch"));
        }
        return Ok(());
    }
    let config = engine.config();
    let audiences = ["https://storage.azure.com".to_string(), format!("https://{account}.file.core.windows.net"), config.file_endpoint(account)];
    match auth::authorize(&parts.headers, &audiences) {
        Ok(claims) => check_roles(engine, account, &claims.oid, share, op),
        Err(AuthFailure::Missing) => Err(fail(StatusCode::UNAUTHORIZED, "NoAuthenticationInformation", "Server failed to authenticate the request.")),
        Err(_) => Err(fail(StatusCode::UNAUTHORIZED, "InvalidAuthenticationInfo", "Server failed to authenticate the request.")),
    }
}

fn check_roles(engine: &Engine, account: &str, oid: &str, share: &str, op: Op) -> Result<(), Failure> {
    use emulator_core::arm::{data_access, Permission};
    if !engine.config().enforce_rbac {
        return Ok(());
    }
    let account_id = account_resource_id(engine, account).unwrap_or_default();
    let (action, scope) = match op {
        Op::ListShares | Op::ServiceProperties => ("Microsoft.Storage/storageAccounts/fileServices/shares/read".to_string(), account_id),
        _ => (
            format!("Microsoft.Storage/storageAccounts/fileServices/fileshares/files/{}", op.verb()),
            format!("{account_id}/fileServices/default/fileshares/{share}"),
        ),
    };
    let needed = match op {
        Op::ListShares | Op::ServiceProperties => vec![Permission::Action(&action)],
        _ => vec![Permission::Data(&action)],
    };
    if data_access(engine, oid, &needed, &scope) {
        return Ok(());
    }
    Err(fail(StatusCode::FORBIDDEN, "AuthorizationPermissionMismatch", "This request is not authorized to perform this operation using this permission."))
}

fn account_resource_id(engine: &Engine, account: &str) -> Option<String> {
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let id = state
        .resources()
        .find(|r| r.resource_type.eq_ignore_ascii_case("Microsoft.Storage/storageAccounts") && r.name.eq_ignore_ascii_case(account))
        .map(|r| r.id.clone());
    id
}

fn metadata_from(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str().strip_prefix("x-ms-meta-")?.to_string(), value.to_str().ok()?.to_string())))
        .collect()
}

fn metadata_headers(out: &mut Vec<(String, String)>, metadata: &BTreeMap<String, String>) {
    for (key, value) in metadata {
        out.push((format!("x-ms-meta-{key}"), value.clone()));
    }
}

/// The HTTP headers a file keeps, under the names a download returns them with.
const FILE_HEADERS: [(&str, &str); 6] = [
    ("x-ms-content-type", "content-type"),
    ("x-ms-content-md5", "content-md5"),
    ("x-ms-content-encoding", "content-encoding"),
    ("x-ms-content-language", "content-language"),
    ("x-ms-cache-control", "cache-control"),
    ("x-ms-content-disposition", "content-disposition"),
];

fn file_headers_from(headers: &HeaderMap) -> BTreeMap<String, String> {
    FILE_HEADERS
        .iter()
        .filter_map(|(sent, kept)| Some((kept.to_string(), headers.get(*sent)?.to_str().ok()?.to_string())))
        .collect()
}

/// What a request does to one share: the account's shares, and the etag
/// the change gets.
fn with_shares<T>(engine: &Engine, account: &str, f: impl FnOnce(&mut BTreeMap<String, Share>, String) -> Result<T, Failure>) -> Result<T, Failure> {
    let state = engine.state::<StorageState>();
    let mut state = state.lock();
    let etag = state.next_etag();
    let account = state.accounts.get_mut(account).ok_or_else(resource_not_found)?;
    f(&mut account.shares, etag)
}

/// The common headers of a file or directory.
fn item_headers(item: &Item) -> Vec<(String, String)> {
    vec![
        ("etag".into(), item.etag.clone()),
        ("last-modified".into(), http_date(item.last_write)),
        ("x-ms-request-server-encrypted".into(), "true".into()),
        ("x-ms-server-encrypted".into(), "true".into()),
        ("x-ms-file-permission-key".into(), PERMISSION_KEY.into()),
        ("x-ms-file-attributes".into(), if item.directory { "Directory".into() } else { "Archive".into() }),
        ("x-ms-file-creation-time".into(), file_time(item.created)),
        ("x-ms-file-last-write-time".into(), file_time(item.last_write)),
        ("x-ms-file-change-time".into(), file_time(item.changed)),
        ("x-ms-file-id".into(), item.file_id()),
        ("x-ms-file-parent-id".into(), format!("{}", 13835128424026341376u64 + item.parent_id)),
    ]
}

fn serve(engine: &Engine, account: &str, parts: &http::request::Parts, body: Bytes) -> Result<Reply, Failure> {
    let query: BTreeMap<String, String> = parse_query(parts.uri.query()).into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
    let segments: Vec<String> = parts.uri.path().split('/').filter(|s| !s.is_empty()).map(|s| percent_decode(s, false)).collect();
    let op = route(&parts.method, &segments, &query)
        .ok_or_else(|| fail(StatusCode::NOT_IMPLEMENTED, "NotImplemented", format!("{} {} is not implemented by the emulator.", parts.method, parts.uri.path())))?;
    let share_name = segments.first().cloned().unwrap_or_default();
    let path = segments.iter().skip(1).cloned().collect::<Vec<_>>().join("/");
    authorize(engine, account, parts, &query, &share_name, &path, op)?;
    let share_key = share_name.to_ascii_lowercase();
    let key = path.to_ascii_lowercase();
    let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());
    let request = FileRequest { engine, account, parts, query: &query, share: &share_key, path: &path, key: &key, header: &header };
    use Op::*;
    match op {
        ListShares => request.list_shares(),
        ServiceProperties => Ok(xml_reply("<StorageServiceProperties><HourMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></HourMetrics><MinuteMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></MinuteMetrics><Cors /></StorageServiceProperties>".into())),
        SetServiceProperties => Ok(reply(StatusCode::ACCEPTED, Vec::new())),
        CreateShare => request.create_share(),
        ShareProperties => request.share_properties(),
        DeleteShare => request.delete_share(),
        SetShareMetadata | SetShareProperties => request.set_share(op == SetShareMetadata),
        ShareStats => request.share_stats(),
        ShareAcl => request.share_acl(None),
        SetShareAcl => request.share_acl(Some(String::from_utf8_lossy(&body).trim().to_string())),
        CreateDirectory => request.create(true, Bytes::new()),
        DirectoryProperties | FileProperties => request.properties(op == DirectoryProperties),
        DeleteDirectory | DeleteFile => request.delete(op == DeleteDirectory),
        SetDirectoryMetadata | SetFileMetadata => request.set_metadata(op == SetDirectoryMetadata),
        SetDirectoryProperties => request.touch(true),
        List => request.list(),
        CreateFile => request.create(false, body),
        GetFile => request.download(),
        PutRange => request.put_range(body),
        ListRanges => request.list_ranges(),
        SetFileProperties => request.set_file_properties(),
    }
}

struct FileRequest<'a> {
    engine: &'a Engine,
    account: &'a str,
    parts: &'a http::request::Parts,
    query: &'a BTreeMap<String, String>,
    /// The share's lowercased name, and the item's path as given and lowercased.
    share: &'a str,
    path: &'a str,
    key: &'a str,
    header: &'a dyn Fn(&str) -> Option<&'a str>,
}

impl FileRequest<'_> {
    fn with_share<T>(&self, f: impl FnOnce(&mut Share, String) -> Result<T, Failure>) -> Result<T, Failure> {
        with_shares(self.engine, self.account, |shares, etag| f(shares.get_mut(self.share).ok_or_else(share_not_found)?, etag))
    }

    fn list_shares(&self) -> Result<Reply, Failure> {
        let prefix = self.query.get("prefix").cloned().unwrap_or_default();
        let with_metadata = self.query.get("include").is_some_and(|i| i.split(',').any(|p| p == "metadata"));
        let endpoint = self.engine.config().file_endpoint(self.account);
        with_shares(self.engine, self.account, |shares, _| {
            let mut out = format!("<EnumerationResults ServiceEndpoint=\"{}\">", xml_escape(&endpoint));
            if !prefix.is_empty() {
                out.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(&prefix)));
            }
            out.push_str("<Shares>");
            for (name, share) in shares.iter().filter(|(n, _)| n.starts_with(&prefix)) {
                out.push_str(&format!(
                    "<Share><Name>{}</Name><Properties><Last-Modified>{}</Last-Modified><Etag>{}</Etag><Quota>{}</Quota><AccessTier>TransactionOptimized</AccessTier><LeaseStatus>unlocked</LeaseStatus><LeaseState>available</LeaseState></Properties>",
                    xml_escape(name),
                    http_date(share.last_modified),
                    xml_escape(&share.etag),
                    share.quota
                ));
                if with_metadata {
                    out.push_str("<Metadata>");
                    for (key, value) in &share.metadata {
                        out.push_str(&format!("<{key}>{}</{key}>", xml_escape(value)));
                    }
                    out.push_str("</Metadata>");
                }
                out.push_str("</Share>");
            }
            out.push_str("</Shares><NextMarker /></EnumerationResults>");
            Ok(xml_reply(out))
        })
    }

    fn create_share(&self) -> Result<Reply, Failure> {
        if !crate::blob::is_valid_container_name(self.share) {
            return Err(fail(StatusCode::BAD_REQUEST, "InvalidResourceName", "The specifed resource name contains invalid characters."));
        }
        let quota = match (self.header)("x-ms-share-quota") {
            Some(q) => q.parse::<u64>().ok().filter(|q| (1..=102_400).contains(q)).ok_or_else(bad_header)?,
            None => 5120,
        };
        let metadata = metadata_from(&self.parts.headers);
        let (etag, modified) = with_shares(self.engine, self.account, |shares, etag| {
            if shares.contains_key(self.share) {
                return Err(fail(StatusCode::CONFLICT, "ShareAlreadyExists", "The specified share already exists."));
            }
            let mut share = Share::new(quota, etag.clone());
            share.metadata = metadata;
            let modified = share.last_modified;
            shares.insert(self.share.to_string(), share);
            Ok((etag, modified))
        })?;
        arm_added(self.engine, self.account, self.share, quota);
        Ok(reply(StatusCode::CREATED, vec![("etag".into(), etag), ("last-modified".into(), http_date(modified))]))
    }

    fn share_properties(&self) -> Result<Reply, Failure> {
        self.with_share(|share, _| {
            let mut headers = vec![
                ("etag".into(), share.etag.clone()),
                ("last-modified".into(), http_date(share.last_modified)),
                ("x-ms-share-quota".into(), share.quota.to_string()),
                ("x-ms-access-tier".into(), "TransactionOptimized".into()),
                ("x-ms-lease-status".into(), "unlocked".into()),
                ("x-ms-lease-state".into(), "available".into()),
            ];
            metadata_headers(&mut headers, &share.metadata);
            Ok(reply(StatusCode::OK, headers))
        })
    }

    fn delete_share(&self) -> Result<Reply, Failure> {
        with_shares(self.engine, self.account, |shares, _| shares.remove(self.share).map(|_| ()).ok_or_else(share_not_found))?;
        arm_removed(self.engine, self.account, self.share);
        Ok(reply(StatusCode::ACCEPTED, Vec::new()))
    }

    fn set_share(&self, metadata: bool) -> Result<Reply, Failure> {
        let quota = (self.header)("x-ms-share-quota").map(|q| q.parse::<u64>().ok().filter(|q| (1..=102_400).contains(q)).ok_or_else(bad_header)).transpose()?;
        let given = metadata_from(&self.parts.headers);
        self.with_share(|share, etag| {
            if metadata {
                share.metadata = given;
            }
            if let Some(quota) = quota {
                share.quota = quota;
            }
            share.etag = etag.clone();
            share.last_modified = SystemTime::now();
            Ok(reply(StatusCode::OK, vec![("etag".into(), etag), ("last-modified".into(), http_date(share.last_modified))]))
        })
    }

    fn share_stats(&self) -> Result<Reply, Failure> {
        self.with_share(|share, _| Ok(xml_reply(format!("<ShareStats><ShareUsageBytes>{}</ShareUsageBytes></ShareStats>", share.usage()))))
    }

    fn share_acl(&self, set: Option<String>) -> Result<Reply, Failure> {
        self.with_share(|share, etag| match set {
            Some(xml) => {
                share.acl = (!xml.is_empty()).then_some(xml);
                share.etag = etag.clone();
                Ok(reply(StatusCode::OK, vec![("etag".into(), etag)]))
            }
            None => {
                let mut r = xml_reply(share.acl.as_deref().map(without_declaration).unwrap_or("<SignedIdentifiers />").to_string());
                r.headers.push(("etag".into(), share.etag.clone()));
                Ok(r)
            }
        })
    }

    /// The lowercased path of the item's parent directory.
    fn parent_key(&self) -> String {
        self.key.rsplit_once('/').map(|(parent, _)| parent.to_string()).unwrap_or_default()
    }

    /// Create Directory, or Create File: a file of `x-ms-content-length`
    /// zero bytes, which replaces any file at the path.
    fn create(&self, directory: bool, body: Bytes) -> Result<Reply, Failure> {
        let size = match directory {
            true => 0,
            false => {
                if (self.header)("x-ms-type") != Some("file") {
                    return Err(bad_header());
                }
                if !body.is_empty() {
                    return Err(bad_header());
                }
                (self.header)("x-ms-content-length").and_then(|v| v.parse::<u64>().ok()).filter(|s| *s <= MAX_FILE).ok_or_else(bad_header)?
            }
        };
        let metadata = metadata_from(&self.parts.headers);
        let headers = file_headers_from(&self.parts.headers);
        let name = self.path.rsplit('/').next().unwrap_or_default().to_string();
        self.with_share(|share, etag| {
            let parent = share.items.get(&self.parent_key()).filter(|p| p.directory).ok_or_else(parent_not_found)?;
            let parent_id = parent.id;
            match share.items.get(self.key) {
                Some(existing) if directory || existing.directory => {
                    let (code, message) = match (directory, existing.directory) {
                        (true, true) => ("ResourceAlreadyExists", "The specified resource already exists."),
                        _ => ("ResourceTypeMismatch", "The specified resource type does not match the type of the existing resource."),
                    };
                    return Err(fail(StatusCode::CONFLICT, code, message));
                }
                _ => {}
            }
            let id = share.items.get(self.key).map(|i| i.id).unwrap_or_else(|| {
                share.next_id += 1;
                share.next_id
            });
            let mut item = Item::directory(&name, id, parent_id, etag, metadata);
            if !directory {
                item.directory = false;
                item.data = Bytes::from(vec![0u8; size as usize]);
                item.headers = headers;
            }
            let reply_headers = item_headers(&item);
            share.items.insert(self.key.to_string(), item);
            Ok(reply(StatusCode::CREATED, reply_headers))
        })
    }

    fn item<'s>(&self, share: &'s Share, directory: bool) -> Result<&'s Item, Failure> {
        share.items.get(self.key).filter(|i| i.directory == directory).ok_or_else(resource_not_found)
    }

    fn properties(&self, directory: bool) -> Result<Reply, Failure> {
        self.with_share(|share, _| {
            let item = self.item(share, directory)?;
            let mut headers = item_headers(item);
            metadata_headers(&mut headers, &item.metadata);
            if !directory {
                headers.push(("x-ms-type".into(), "File".into()));
                headers.push(("content-length".into(), item.data.len().to_string()));
                for (name, value) in &item.headers {
                    headers.push((name.clone(), value.clone()));
                }
            }
            Ok(reply(StatusCode::OK, headers))
        })
    }

    fn delete(&self, directory: bool) -> Result<Reply, Failure> {
        self.with_share(|share, _| {
            self.item(share, directory)?;
            if directory {
                let prefix = format!("{}/", self.key);
                if share.items.keys().any(|k| k.starts_with(&prefix)) {
                    return Err(fail(StatusCode::CONFLICT, "DirectoryNotEmpty", "The specified directory is not empty."));
                }
            }
            share.items.remove(self.key);
            Ok(reply(StatusCode::ACCEPTED, Vec::new()))
        })
    }

    fn set_metadata(&self, directory: bool) -> Result<Reply, Failure> {
        let metadata = metadata_from(&self.parts.headers);
        self.with_share(|share, etag| {
            self.item(share, directory)?;
            let item = share.items.get_mut(self.key).expect("checked above");
            item.metadata = metadata;
            item.etag = etag;
            item.changed = SystemTime::now();
            Ok(reply(StatusCode::OK, item_headers(item)))
        })
    }

    /// Set Directory Properties: the emulator keeps no SMB properties, so
    /// this only changes the entity tag.
    fn touch(&self, directory: bool) -> Result<Reply, Failure> {
        self.with_share(|share, etag| {
            self.item(share, directory)?;
            let item = share.items.get_mut(self.key).expect("checked above");
            item.etag = etag;
            item.changed = SystemTime::now();
            Ok(reply(StatusCode::OK, item_headers(item)))
        })
    }

    /// Set File Properties: a new size (`x-ms-content-length`) and the HTTP
    /// headers. Headers left out are cleared, unless the request only resizes.
    fn set_file_properties(&self) -> Result<Reply, Failure> {
        let size = (self.header)("x-ms-content-length").map(|v| v.parse::<u64>().ok().filter(|s| *s <= MAX_FILE).ok_or_else(bad_header)).transpose()?;
        let headers = file_headers_from(&self.parts.headers);
        let sets_headers = !headers.is_empty() || size.is_none();
        self.with_share(|share, etag| {
            self.item(share, false)?;
            let item = share.items.get_mut(self.key).expect("checked above");
            if let Some(size) = size {
                let mut data = item.data.to_vec();
                data.resize(size as usize, 0);
                item.data = Bytes::from(data);
                if let Some(last) = size.checked_sub(1) {
                    mark(&mut item.ranges, size, u64::MAX - 1, false);
                    item.ranges.retain(|(a, _)| *a <= last);
                } else {
                    item.ranges.clear();
                }
            }
            if sets_headers {
                item.headers = headers;
            }
            item.etag = etag;
            item.last_write = SystemTime::now();
            item.changed = item.last_write;
            Ok(reply(StatusCode::OK, item_headers(item)))
        })
    }

    /// The `x-ms-range` (or `Range`) of a range operation: inclusive bytes.
    fn range(&self, size: usize) -> Result<Option<(usize, usize)>, Failure> {
        let Some(raw) = (self.header)("x-ms-range").or((self.header)("range")) else { return Ok(None) };
        let invalid = || fail(StatusCode::RANGE_NOT_SATISFIABLE, "InvalidRange", "The range specified is invalid for the current size of the resource.");
        let (start, end) = raw.trim().strip_prefix("bytes=").and_then(|r| r.split_once('-')).ok_or_else(invalid)?;
        let start: usize = start.parse().map_err(|_| invalid())?;
        if start >= size {
            return Err(invalid());
        }
        let end = match end.trim() {
            "" => size - 1,
            given => given.parse::<usize>().map_err(|_| invalid())?.min(size - 1),
        };
        if end < start {
            return Err(invalid());
        }
        Ok(Some((start, end)))
    }

    fn download(&self) -> Result<Reply, Failure> {
        self.with_share(|share, _| {
            let item = self.item(share, false)?;
            let mut headers = item_headers(item);
            metadata_headers(&mut headers, &item.metadata);
            headers.push(("x-ms-type".into(), "File".into()));
            headers.push(("accept-ranges".into(), "bytes".into()));
            for (name, value) in &item.headers {
                if name != "content-md5" {
                    headers.push((name.clone(), value.clone()));
                }
            }
            if !item.headers.contains_key("content-type") {
                headers.push(("content-type".into(), "application/octet-stream".into()));
            }
            let len = item.data.len();
            // A range on an empty file is unsatisfiable (416), as in Azure;
            // the SDKs take that answer to mean the file is empty.
            let (status, data) = match self.range(len)? {
                Some((start, end)) => {
                    headers.push(("content-range".into(), format!("bytes {start}-{end}/{len}")));
                    if let Some(md5) = item.headers.get("content-md5") {
                        headers.push(("x-ms-content-md5".into(), md5.clone()));
                    }
                    (StatusCode::PARTIAL_CONTENT, item.data.slice(start..=end))
                }
                None => {
                    if let Some(md5) = item.headers.get("content-md5") {
                        headers.push(("content-md5".into(), md5.clone()));
                    }
                    (StatusCode::OK, item.data.clone())
                }
            };
            headers.push(("content-length".into(), data.len().to_string()));
            let body = (self.parts.method != Method::HEAD).then(|| Body::from(data));
            Ok(Reply { status, headers, body })
        })
    }

    /// Put Range: writes the body (`x-ms-write: update`) or zeroes the range (`clear`).
    fn put_range(&self, body: Bytes) -> Result<Reply, Failure> {
        let clear = match (self.header)("x-ms-write") {
            Some("update") => false,
            Some("clear") => true,
            _ => return Err(bad_header()),
        };
        let md5 = STANDARD.encode(Md5::digest(&body));
        if !clear && (self.header)("content-md5").is_some_and(|sent| sent != md5) {
            return Err(fail(StatusCode::BAD_REQUEST, "Md5Mismatch", "The MD5 value specified in the request did not match with the MD5 value calculated by the server."));
        }
        self.with_share(|share, etag| {
            self.item(share, false)?;
            let item = share.items.get_mut(self.key).expect("checked above");
            let (start, end) = self.range(item.data.len())?.ok_or_else(bad_header)?;
            if !clear && body.len() != end - start + 1 {
                return Err(fail(StatusCode::BAD_REQUEST, "InvalidHeaderValue", "The length of the body does not match the range."));
            }
            let mut data = item.data.to_vec();
            match clear {
                true => data[start..=end].fill(0),
                false => data[start..=end].copy_from_slice(&body),
            }
            item.data = Bytes::from(data);
            mark(&mut item.ranges, start as u64, end as u64, !clear);
            item.etag = etag;
            item.last_write = SystemTime::now();
            item.changed = item.last_write;
            let mut headers = item_headers(item);
            if !clear {
                headers.push(("content-md5".into(), md5));
            }
            Ok(reply(StatusCode::CREATED, headers))
        })
    }

    fn list_ranges(&self) -> Result<Reply, Failure> {
        self.with_share(|share, _| {
            let item = self.item(share, false)?;
            let window = if item.data.is_empty() { None } else { self.range(item.data.len())? };
            let mut out = String::from("<Ranges>");
            for &(a, b) in &item.ranges {
                let (a, b) = match window {
                    Some((start, end)) if b < start as u64 || a > end as u64 => continue,
                    Some((start, end)) => (a.max(start as u64), b.min(end as u64)),
                    None => (a, b),
                };
                out.push_str(&format!("<Range><Start>{a}</Start><End>{b}</End></Range>"));
            }
            out.push_str("</Ranges>");
            let mut r = xml_reply(out);
            r.headers.push(("etag".into(), item.etag.clone()));
            r.headers.push(("last-modified".into(), http_date(item.last_write)));
            r.headers.push(("x-ms-content-length".into(), item.data.len().to_string()));
            Ok(r)
        })
    }

    /// List Directories and Files: the directory's own entries, in name order.
    fn list(&self) -> Result<Reply, Failure> {
        let prefix = self.query.get("prefix").cloned().unwrap_or_default().to_ascii_lowercase();
        let endpoint = self.engine.config().file_endpoint(self.account);
        let share_name = self.share.to_string();
        let path = self.path.to_string();
        self.with_share(|share, _| {
            let directory = self.item(share, true)?;
            let base = if self.key.is_empty() { String::new() } else { format!("{}/", self.key) };
            let mut out = format!(
                "<EnumerationResults ServiceEndpoint=\"{}\" ShareName=\"{}\" DirectoryPath=\"{}\">",
                xml_escape(&endpoint),
                xml_escape(&share_name),
                xml_escape(&path)
            );
            if !prefix.is_empty() {
                out.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(&prefix)));
            }
            out.push_str("<Entries>");
            for (key, item) in share.items.range(base.clone()..) {
                let Some(rest) = key.strip_prefix(&base) else { break };
                if rest.is_empty() || rest.contains('/') || !rest.starts_with(&prefix) {
                    continue;
                }
                let tag = if item.directory { "Directory" } else { "File" };
                let length = if item.directory { String::new() } else { format!("<Content-Length>{}</Content-Length>", item.data.len()) };
                out.push_str(&format!(
                    "<{tag}><FileId>{}</FileId><Name>{}</Name><Properties>{length}<CreationTime>{}</CreationTime><LastWriteTime>{}</LastWriteTime><ChangeTime>{}</ChangeTime><Last-Modified>{}</Last-Modified><Etag>{}</Etag></Properties><Attributes>{}</Attributes><PermissionKey>{PERMISSION_KEY}</PermissionKey></{tag}>",
                    item.file_id(),
                    xml_escape(&item.name),
                    file_time(item.created),
                    file_time(item.last_write),
                    file_time(item.changed),
                    http_date(item.last_write),
                    xml_escape(&item.etag),
                    if item.directory { "Directory" } else { "Archive" },
                ));
            }
            out.push_str(&format!("</Entries><NextMarker /><DirectoryId>{}</DirectoryId></EnumerationResults>", directory.file_id()));
            Ok(xml_reply(out))
        })
    }
}

/// A share created on the data plane shows up on Resource Manager.
fn arm_added(engine: &Engine, account: &str, share: &str, quota: u64) {
    let Some(account_id) = account_resource_id(engine, account) else { return };
    let id = format!("{account_id}/fileServices/default/shares/{share}");
    let state = engine.state::<ArmState>();
    let mut state = state.lock();
    if state.resource(&id).is_none() {
        state.insert(Resource {
            id,
            name: share.to_string(),
            resource_type: SHARE_TYPE.into(),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: json!({
                "shareQuota": quota,
                "enabledProtocols": "SMB",
                "accessTier": "TransactionOptimized",
                "leaseStatus": "unlocked",
                "leaseState": "available",
                "lastModifiedTime": emulator_core::http_util::iso_timestamp(),
            }),
        });
    }
}

fn arm_removed(engine: &Engine, account: &str, share: &str) {
    let Some(account_id) = account_resource_id(engine, account) else { return };
    engine.state::<ArmState>().lock().remove(&format!("{account_id}/fileServices/default/shares/{share}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_dropped_whatever_their_quotes() {
        assert_eq!(without_declaration("<?xml version='1.0' encoding='utf-8'?>\n<A/>"), "<A/>");
        assert_eq!(without_declaration("<?xml version=\"1.0\"?><A/>"), "<A/>");
        assert_eq!(without_declaration("<A/>"), "<A/>");
    }

    #[test]
    fn ranges_merge_and_split() {
        let mut ranges = Vec::new();
        mark(&mut ranges, 0, 9, true);
        mark(&mut ranges, 20, 29, true);
        mark(&mut ranges, 10, 19, true);
        assert_eq!(ranges, vec![(0, 29)]);
        mark(&mut ranges, 5, 14, false);
        assert_eq!(ranges, vec![(0, 4), (15, 29)]);
        mark(&mut ranges, 0, 100, false);
        assert!(ranges.is_empty());
    }

    #[test]
    fn routes() {
        let q = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        let seg = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(route(&Method::PUT, &seg(&["s"]), &q(&[("restype", "share")])), Some(Op::CreateShare));
        assert_eq!(route(&Method::GET, &seg(&["s"]), &q(&[("restype", "directory"), ("comp", "list")])), Some(Op::List));
        assert_eq!(route(&Method::PUT, &seg(&["s", "d", "f"]), &q(&[("comp", "range")])), Some(Op::PutRange));
        assert_eq!(route(&Method::PUT, &seg(&["s"]), &q(&[])), None);
    }
}
