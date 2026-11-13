//! The Blob service data plane.
//!
//! Implemented: container create / delete / get properties / list, and block blob
//! put (single request) / get (with ranges) / get properties / delete / list.
//! Everything else answers `501 NotImplemented` rather than pretending.

use std::sync::Arc;
use std::time::SystemTime;

use axum::body::{Body, Bytes};
use axum::response::Response;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use emulator_core::auth::{self, AuthFailure};
use emulator_core::http_util::{parse_query, percent_decode, read_body, request_host, request_id};
use emulator_core::Engine;
use http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use md5::{Digest, Md5};

use crate::sas::{self, Level, Needed, SasFailure};
use crate::shared_key;
use crate::state::{lease_state, Blob, Container, CopyInfo, Lease, LeaseState, Protection, StorageState};

/// Service version reported when the request names none.
const DEFAULT_VERSION: &str = "2021-08-06";

struct BlobError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    headers: Vec<(HeaderName, String)>,
}

fn error(status: StatusCode, code: &'static str, message: &'static str) -> BlobError {
    BlobError {
        status,
        code,
        message,
        headers: Vec::new(),
    }
}

fn not_implemented() -> BlobError {
    error(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        "The emulator does not implement this Blob service operation yet.",
    )
}

fn container_not_found() -> BlobError {
    error(
        StatusCode::NOT_FOUND,
        "ContainerNotFound",
        "The specified container does not exist.",
    )
}

fn blob_not_found() -> BlobError {
    error(StatusCode::NOT_FOUND, "BlobNotFound", "The specified blob does not exist.")
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn http_date(time: SystemTime) -> String {
    httpdate::fmt_http_date(time)
}

type Headers = Vec<(HeaderName, String)>;

fn name(header: &'static str) -> HeaderName {
    HeaderName::from_static(header)
}

fn respond(status: StatusCode, headers: Headers, body: impl Into<Body>) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    for (key, value) in headers {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert(key, value);
        }
    }
    response
}

fn xml(status: StatusCode, body: String) -> Response {
    respond(status, vec![(header::CONTENT_TYPE, "application/xml".into())], body)
}

/// Serves one Blob service request addressed to `account`.
pub async fn handle(engine: Arc<Engine>, account: String, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let id = request_id();
    let is_head = parts.method == Method::HEAD;
    let mut response = match serve(&engine, &account, &parts, body).await {
        Ok(response) => response,
        Err(failure) => {
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{}</Code><Message>{}\nRequestId:{}\nTime:{}</Message></Error>",
                failure.code,
                xml_escape(failure.message),
                id,
                emulator_core::http_util::iso_timestamp(),
            );
            let mut headers = failure.headers;
            headers.push((name("x-ms-error-code"), failure.code.into()));
            let empty = is_head || failure.status == StatusCode::NOT_MODIFIED;
            let mut response = xml(failure.status, if empty { String::new() } else { body });
            for (key, value) in headers {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    response.headers_mut().insert(key, value);
                }
            }
            response
        }
    };
    let version = parts
        .headers
        .get("x-ms-version")
        .cloned()
        .unwrap_or(HeaderValue::from_static(DEFAULT_VERSION));
    let headers = response.headers_mut();
    headers.insert("x-ms-version", version);
    if let Ok(value) = HeaderValue::from_str(&id) {
        headers.insert("x-ms-request-id", value);
    }
    if let Some(client_id) = parts.headers.get("x-ms-client-request-id") {
        headers.insert("x-ms-client-request-id", client_id.clone());
    }
    headers.insert(header::SERVER, HeaderValue::from_static("Azure-Emulator-Blob/0.1"));
    response
}

/// The operations this data plane implements.
#[derive(Clone, Copy)]
enum Route {
    ListContainers,
    CreateContainer,
    ContainerProperties,
    DeleteContainer,
    ListBlobs,
    PutBlob,
    GetBlob,
    HeadBlob,
    DeleteBlob,
    PutBlock,
    PutBlockList,
    GetBlockList,
    GetMetadata,
    SetMetadata,
    SetProperties,
    LeaseBlob,
    LeaseContainer,
    Snapshot,
    CopyBlob,
    AbortCopy,
}

impl Route {
    fn of(method: &str, container: &str, blob: &str, restype: Option<&str>, comp: Option<&str>, copy_source: bool) -> Option<Self> {
        if container.is_empty() {
            return matches!((method, comp), ("GET", Some("list"))).then_some(Self::ListContainers);
        }
        if blob.is_empty() {
            if restype != Some("container") {
                return None;
            }
            return match (method, comp) {
                ("PUT", None) => Some(Self::CreateContainer),
                ("GET" | "HEAD", None) => Some(Self::ContainerProperties),
                ("DELETE", None) => Some(Self::DeleteContainer),
                ("GET", Some("list")) => Some(Self::ListBlobs),
                ("PUT", Some("lease")) => Some(Self::LeaseContainer),
                _ => None,
            };
        }
        match (method, comp) {
            ("PUT", None) if copy_source => Some(Self::CopyBlob),
            ("PUT", None) => Some(Self::PutBlob),
            ("GET", None) => Some(Self::GetBlob),
            ("HEAD", None) => Some(Self::HeadBlob),
            ("DELETE", None) => Some(Self::DeleteBlob),
            ("PUT", Some("block")) => Some(Self::PutBlock),
            ("PUT", Some("blocklist")) => Some(Self::PutBlockList),
            ("GET", Some("blocklist")) => Some(Self::GetBlockList),
            ("GET" | "HEAD", Some("metadata")) => Some(Self::GetMetadata),
            ("PUT", Some("metadata")) => Some(Self::SetMetadata),
            ("PUT", Some("properties")) => Some(Self::SetProperties),
            ("PUT", Some("lease")) => Some(Self::LeaseBlob),
            ("PUT", Some("snapshot")) => Some(Self::Snapshot),
            ("PUT", Some("copy")) => Some(Self::AbortCopy),
            _ => None,
        }
    }

    /// What a shared access signature must grant for this operation.
    fn needed(self) -> Needed {
        let (level, permissions, service_sas_on_container) = match self {
            Self::ListContainers => (Level::Service, "l", false),
            Self::CreateContainer => (Level::Container, "cw", false),
            Self::ContainerProperties => (Level::Container, "r", false),
            Self::DeleteContainer => (Level::Container, "d", false),
            Self::ListBlobs => (Level::Container, "l", true),
            Self::PutBlob | Self::PutBlock | Self::PutBlockList => (Level::Object, "wc", false),
            Self::SetMetadata | Self::SetProperties => (Level::Object, "w", false),
            Self::GetBlob | Self::HeadBlob | Self::GetBlockList | Self::GetMetadata => (Level::Object, "r", false),
            Self::DeleteBlob => (Level::Object, "d", false),
            Self::LeaseBlob => (Level::Object, "wd", false),
            Self::LeaseContainer => (Level::Container, "wd", false),
            Self::Snapshot | Self::CopyBlob => (Level::Object, "cw", false),
            Self::AbortCopy => (Level::Object, "w", false),
        };
        Needed {
            level,
            permissions,
            service_sas_on_container,
        }
    }
}

fn authorize(
    engine: &Engine,
    account: &str,
    parts: &http::request::Parts,
    query: &std::collections::BTreeMap<String, String>,
    container: &str,
    blob: &str,
    route: Option<Route>,
) -> Result<(), BlobError> {
    let config = engine.config();
    let failed = || {
        error(
            StatusCode::FORBIDDEN,
            "AuthenticationFailed",
            "Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature.",
        )
    };
    let authorization = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(credential) = authorization.strip_prefix("SharedKey ") {
        let (signed_account, signature) = credential.split_once(':').ok_or_else(failed)?;
        if signed_account != account {
            return Err(failed());
        }
        let keys = {
            let state = engine.state::<StorageState>();
            let state = state.lock();
            state.accounts.get(account).map(|a| a.keys.clone()).ok_or_else(failed)?
        };
        let to_sign = shared_key::string_to_sign(&parts.method, &parts.uri, &parts.headers, account);
        return match keys.iter().any(|key| shared_key::verify(&to_sign, key, signature)) {
            true => Ok(()),
            false => Err(failed()),
        };
    }

    if query.contains_key("sig") {
        let route = route.ok_or_else(not_implemented)?;
        let keys = {
            let state = engine.state::<StorageState>();
            let state = state.lock();
            state.accounts.get(account).map(|a| a.keys.clone()).ok_or_else(failed)?
        };
        return sas::authorize(query, account, &keys, container, blob, route.needed()).map_err(|failure| match failure {
            SasFailure::Authentication => failed(),
            SasFailure::Permission => error(
                StatusCode::FORBIDDEN,
                "AuthorizationPermissionMismatch",
                "This request is not authorized to perform this operation using this permission.",
            ),
            SasFailure::ResourceType => error(
                StatusCode::FORBIDDEN,
                "AuthorizationResourceTypeMismatch",
                "This request is not authorized to perform this operation using this resource type.",
            ),
            SasFailure::Service => error(
                StatusCode::FORBIDDEN,
                "AuthorizationServiceMismatch",
                "This request is not authorized to perform this operation using this service.",
            ),
            SasFailure::Unsupported => error(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "The emulator verifies account and service shared access signatures signed with an account key only.",
            ),
        });
    }

    let audiences = [
        "https://storage.azure.com".to_string(),
        format!("https://{account}.blob.core.windows.net"),
        config.blob_endpoint(account),
    ];
    match auth::authorize(&parts.headers, &audiences) {
        Ok(claims) => match route {
            Some(route) => check_roles(engine, account, &claims.oid, container, blob, route),
            None => Ok(()),
        },
        Err(AuthFailure::Missing) => {
            let mut failure = error(
                StatusCode::UNAUTHORIZED,
                "NoAuthenticationInformation",
                "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.",
            );
            failure.headers.push((
                header::WWW_AUTHENTICATE,
                format!(
                    "Bearer authorization_uri={}/{}/oauth2/authorize resource_id=https://storage.azure.com",
                    config.management_endpoint(),
                    config.tenant_id
                ),
            ));
            Err(failure)
        }
        Err(AuthFailure::Invalid) => Err(error(
            StatusCode::UNAUTHORIZED,
            "InvalidAuthenticationInfo",
            "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.",
        )),
        Err(AuthFailure::WrongAudience) => Err(error(
            StatusCode::UNAUTHORIZED,
            "InvalidAuthenticationInfo",
            "Authentication information is not given in the correct format. Check the value of Authorization header.",
        )),
    }
}

/// With role assignments enforced, refuses an Entra-authorized request the
/// caller's roles do not allow. Shared Key and SAS requests are not subject to
/// role assignments, as in Azure.
fn check_roles(engine: &Engine, account: &str, oid: &str, container: &str, blob: &str, route: Route) -> Result<(), BlobError> {
    use emulator_core::arm::{data_access, ArmState, Permission};
    if !engine.config().enforce_rbac {
        return Ok(());
    }
    const CONTAINERS: &str = "Microsoft.Storage/storageAccounts/blobServices/containers";
    let (read, write, delete) = (format!("{CONTAINERS}/read"), format!("{CONTAINERS}/write"), format!("{CONTAINERS}/delete"));
    let blobs = |verb: &str| format!("{CONTAINERS}/blobs/{verb}");
    let (blob_read, blob_write, blob_add, blob_delete) = (blobs("read"), blobs("write"), blobs("add/action"), blobs("delete"));
    // Writing a blob that does not exist yet is also allowed by `add`.
    let is_new = || {
        let state = engine.state::<StorageState>();
        let state = state.lock();
        !state
            .accounts
            .get(account)
            .and_then(|a| a.containers.get(container))
            .is_some_and(|c| c.blobs.contains_key(blob))
    };
    let needed: Vec<Permission<'_>> = match route {
        Route::ListContainers | Route::ContainerProperties => vec![Permission::Action(&read)],
        Route::CreateContainer | Route::LeaseContainer => vec![Permission::Action(&write)],
        Route::DeleteContainer => vec![Permission::Action(&delete)],
        Route::ListBlobs | Route::GetBlob | Route::HeadBlob | Route::GetBlockList | Route::GetMetadata => {
            vec![Permission::Data(&blob_read)]
        }
        Route::PutBlob | Route::PutBlock | Route::PutBlockList | Route::CopyBlob if is_new() => {
            vec![Permission::Data(&blob_write), Permission::Data(&blob_add)]
        }
        Route::PutBlob
        | Route::PutBlock
        | Route::PutBlockList
        | Route::SetMetadata
        | Route::SetProperties
        | Route::CopyBlob
        | Route::AbortCopy
        | Route::LeaseBlob
        | Route::Snapshot => {
            vec![Permission::Data(&blob_write)]
        }
        Route::DeleteBlob => vec![Permission::Data(&blob_delete)],
    };
    let account_id = {
        let state = engine.state::<ArmState>();
        let state = state.lock();
        let id = state
            .resources()
            .find(|r| r.resource_type.eq_ignore_ascii_case("Microsoft.Storage/storageAccounts") && r.name.eq_ignore_ascii_case(account))
            .map(|r| r.id.clone())
            .unwrap_or_default();
        id
    };
    // Role assignments reach down to a container; a blob is checked there.
    let scope = match container {
        "" => account_id,
        container => format!("{account_id}/blobServices/default/containers/{container}"),
    };
    if data_access(engine, oid, &needed, &scope) {
        return Ok(());
    }
    Err(error(
        StatusCode::FORBIDDEN,
        "AuthorizationPermissionMismatch",
        "This request is not authorized to perform this operation using this permission.",
    ))
}

async fn serve(
    engine: &Engine,
    account: &str,
    parts: &http::request::Parts,
    body: Body,
) -> Result<Response, BlobError> {
    let exists = engine.state::<StorageState>().lock().accounts.contains_key(account);
    if !exists {
        // A real request for a missing account fails in DNS; this is the nearest answer.
        return Err(error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            "The specified resource does not exist.",
        ));
    }
    let query: std::collections::BTreeMap<String, String> = parse_query(parts.uri.query())
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect();
    let param = |key: &str| query.get(key).map(String::as_str);
    let path = parts.uri.path().trim_start_matches('/');
    let (container, blob) = match path.split_once('/') {
        Some((container, blob)) => (percent_decode(container, false), percent_decode(blob, false)),
        None => (percent_decode(path, false), String::new()),
    };
    let copy_source = parts.headers.contains_key("x-ms-copy-source");
    let route = Route::of(parts.method.as_str(), &container, &blob, param("restype"), param("comp"), copy_source);
    authorize(engine, account, parts, &query, &container, &blob, route)?;
    let route = route.ok_or_else(not_implemented)?;

    let request = BlobRequest {
        engine,
        account,
        headers: &parts.headers,
        query: &query,
    };
    match route {
        Route::ListContainers => request.list_containers(),
        Route::CreateContainer => request.create_container(&container),
        Route::ContainerProperties => request.container_properties(&container),
        Route::DeleteContainer => request.delete_container(&container),
        Route::ListBlobs => request.list_blobs(&container),
        Route::PutBlob | Route::PutBlock | Route::PutBlockList => {
            let data = read_body(body).await.map_err(|_| {
                error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "RequestBodyTooLarge",
                    "The request body is too large and exceeds the maximum permissible limit.",
                )
            })?;
            match route {
                Route::PutBlob => request.put_blob(&container, &blob, data),
                Route::PutBlock => request.put_block(&container, &blob, data),
                _ => request.put_block_list(&container, &blob, &data),
            }
        }
        Route::GetBlob => request.get_blob(&container, &blob, true),
        Route::HeadBlob => request.get_blob(&container, &blob, false),
        Route::DeleteBlob => request.delete_blob(&container, &blob),
        Route::GetBlockList => request.get_block_list(&container, &blob),
        Route::GetMetadata => request.get_metadata(&container, &blob),
        Route::SetMetadata => request.set_metadata(&container, &blob),
        Route::SetProperties => request.set_properties(&container, &blob),
        Route::LeaseBlob => request.lease_blob(&container, &blob),
        Route::LeaseContainer => request.lease_container(&container),
        Route::Snapshot => request.snapshot_blob(&container, &blob),
        Route::CopyBlob => request.copy_blob(&container, &blob),
        Route::AbortCopy => Err(error(
            StatusCode::CONFLICT,
            "NoPendingCopyOperation",
            "There is currently no pending copy operation.",
        )),
    }
}

struct BlobRequest<'a> {
    engine: &'a Engine,
    account: &'a str,
    headers: &'a HeaderMap,
    query: &'a std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum BlockKind {
    Committed,
    Uncommitted,
    Latest,
}

/// The entries of a Put Block List body, in order:
/// `<BlockList><Latest>id</Latest><Committed>id</Committed>...</BlockList>`.
fn parse_block_list(text: &str) -> Option<Vec<(BlockKind, String)>> {
    let start = text.find("<BlockList>")? + "<BlockList>".len();
    let end = text.rfind("</BlockList>")?;
    let mut rest = text.get(start..end)?;
    let mut out = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Some(out);
        }
        let open_end = rest.find('>')?;
        let tag = rest.strip_prefix('<')?.get(..open_end - 1)?;
        let kind = match tag {
            "Committed" => BlockKind::Committed,
            "Uncommitted" => BlockKind::Uncommitted,
            "Latest" => BlockKind::Latest,
            _ => return None,
        };
        let close = format!("</{tag}>");
        let body_end = rest.find(&close)?;
        let id = rest.get(open_end + 1..body_end)?.trim();
        out.push((kind, xml_unescape(id)));
        rest = &rest[body_end + close.len()..];
    }
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

pub(crate) fn is_valid_container_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && !name.contains("--")
}

/// `x-ms-lease-status`, `x-ms-lease-state` and, while leased, `x-ms-lease-duration`.
fn lease_headers(headers: &mut Headers, lease: Option<&Lease>) {
    let (status, state, duration) = lease_properties(lease);
    headers.push((name("x-ms-lease-status"), status.into()));
    headers.push((name("x-ms-lease-state"), state));
    if let Some(duration) = duration {
        headers.push((name("x-ms-lease-duration"), duration.into()));
    }
}

/// A lease's status, state and, while leased, duration, as Azure spells them.
fn lease_properties(lease: Option<&Lease>) -> (&'static str, String, Option<&'static str>) {
    let state = lease_state(lease, SystemTime::now());
    let status = if state.blocks() { "locked" } else { "unlocked" };
    let duration = state.blocks().then(|| if lease.is_some_and(|l| l.duration.is_none()) { "infinite" } else { "fixed" });
    (status, state.as_str().to_ascii_lowercase(), duration)
}

fn container_lease_headers(headers: &mut Headers, container: &Container) {
    lease_headers(headers, container.lease.as_ref());
}

/// An entity tag without quotes or a weak prefix, for comparison.
fn bare_etag(tag: &str) -> &str {
    tag.trim().trim_start_matches("W/").trim_matches('"')
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A snapshot time, `2024-01-02T03:04:05.1234567Z`, later than any in `taken`.
fn snapshot_time(taken: Option<&std::collections::BTreeMap<String, Blob>>) -> String {
    let mut time = chrono::Utc::now();
    loop {
        // Seven digits of fraction, as Azure gives: chrono formats three, six or nine.
        let nanos = time.format("%Y-%m-%dT%H:%M:%S%.9f").to_string();
        let stamp = format!("{}Z", &nanos[..nanos.len() - 2]);
        let last = taken.and_then(|t| t.keys().next_back());
        if last.is_none_or(|last| *last < stamp) {
            return stamp;
        }
        time += chrono::Duration::nanoseconds(100);
    }
}

/// The refusal for overwriting or deleting a protected blob.
fn immutable(protection: Protection) -> BlobError {
    match protection {
        Protection::LegalHold => error(
            StatusCode::CONFLICT,
            "BlobImmutableDueToLegalHold",
            "This operation is not permitted as the blob is immutable due to one or more legal holds.",
        ),
        Protection::Retention => error(
            StatusCode::CONFLICT,
            "BlobImmutableDueToPolicy",
            "This operation is not permitted as the blob is immutable due to a policy.",
        ),
    }
}

/// The range a request asks for, as inclusive byte offsets into a blob of `len`.
/// `Ok(None)` means the whole blob; `Err` means the range cannot be satisfied.
fn requested_range(headers: &HeaderMap, len: usize) -> Result<Option<(usize, usize)>, ()> {
    let Some(raw) = headers
        .get("x-ms-range")
        .or_else(|| headers.get(header::RANGE))
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    let Some((start, end)) = raw.strip_prefix("bytes=").and_then(|r| r.split_once('-')) else {
        return Ok(None);
    };
    let Ok(start) = start.parse::<usize>() else {
        return Ok(None);
    };
    if start >= len {
        return Err(());
    }
    let end = end.parse::<usize>().unwrap_or(usize::MAX).min(len - 1);
    if end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

impl BlobRequest<'_> {
    fn header(&self, key: &str) -> Option<&str> {
        self.headers.get(key).and_then(|v| v.to_str().ok())
    }

    fn metadata(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .filter_map(|(key, value)| {
                let key = key.as_str().strip_prefix("x-ms-meta-")?;
                Some((key.to_string(), value.to_str().ok()?.to_string()))
            })
            .collect()
    }

    /// The standard content headers a write sets (`x-ms-blob-cache-control`, ...),
    /// under the names a read returns them with.
    fn content_headers(&self) -> Vec<(&'static str, String)> {
        [
            ("cache-control", "x-ms-blob-cache-control"),
            ("content-encoding", "x-ms-blob-content-encoding"),
            ("content-language", "x-ms-blob-content-language"),
            ("content-disposition", "x-ms-blob-content-disposition"),
        ]
        .into_iter()
        .filter_map(|(returned, sent)| Some((returned, self.header(sent)?.to_string())))
        .collect()
    }

    fn create_container(&self, container: &str) -> Result<Response, BlobError> {
        if !is_valid_container_name(container) {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "InvalidResourceName",
                "The specifed resource name contains invalid characters.",
            ));
        }
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let etag = state.next_etag();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        if account.containers.contains_key(container) {
            return Err(error(
                StatusCode::CONFLICT,
                "ContainerAlreadyExists",
                "The specified container already exists.",
            ));
        }
        let now = SystemTime::now();
        account.containers.insert(
            container.to_string(),
            Container::new(etag.clone(), now, self.metadata()),
        );
        Ok(respond(
            StatusCode::CREATED,
            vec![(header::ETAG, etag), (header::LAST_MODIFIED, http_date(now))],
            Body::empty(),
        ))
    }

    fn container_properties(&self, container: &str) -> Result<Response, BlobError> {
        let handle = self.engine.state::<StorageState>();
        let state = handle.lock();
        let found = state.accounts[self.account]
            .containers
            .get(container)
            .ok_or_else(container_not_found)?;
        let mut headers = vec![
            (header::ETAG, found.etag.clone()),
            (header::LAST_MODIFIED, http_date(found.last_modified)),
            (name("x-ms-has-immutability-policy"), found.immutability.is_some().to_string()),
            (name("x-ms-has-legal-hold"), (!found.legal_hold.is_empty()).to_string()),
        ];
        container_lease_headers(&mut headers, found);
        for (key, value) in &found.metadata {
            if let Ok(key) = HeaderName::try_from(format!("x-ms-meta-{key}")) {
                headers.push((key, value.clone()));
            }
        }
        Ok(respond(StatusCode::OK, headers, Body::empty()))
    }

    fn delete_container(&self, container: &str) -> Result<Response, BlobError> {
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get(container).ok_or_else(container_not_found)?;
        let now = SystemTime::now();
        if let Some(protection) = found.deletion_blocker(now) {
            let (code, message) = crate::protection::protected(protection);
            return Err(error(StatusCode::CONFLICT, code, message));
        }
        if found.lease_state(now).blocks() {
            let current = found.lease.as_ref().map(|l| l.id.as_str());
            match self.header("x-ms-lease-id") {
                None => {
                    return Err(error(
                        StatusCode::PRECONDITION_FAILED,
                        "LeaseIdMissing",
                        "There is currently a lease on the container and no lease ID was specified in the request.",
                    ))
                }
                Some(sent) if Some(sent) != current => {
                    return Err(error(
                        StatusCode::PRECONDITION_FAILED,
                        "LeaseIdMismatchWithContainerOperation",
                        "The lease ID specified did not match the lease ID for the container.",
                    ))
                }
                Some(_) => {}
            }
        }
        account.containers.remove(container);
        Ok(respond(StatusCode::ACCEPTED, Vec::new(), Body::empty()))
    }

    fn list_containers(&self) -> Result<Response, BlobError> {
        let prefix = self.query.get("prefix").map(String::as_str).unwrap_or("");
        let handle = self.engine.state::<StorageState>();
        let state = handle.lock();
        let mut out = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ServiceEndpoint=\"{}\">",
            xml_escape(&self.engine.config().blob_endpoint(self.account))
        );
        if !prefix.is_empty() {
            out.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(prefix)));
        }
        out.push_str("<Containers>");
        for (container, found) in &state.accounts[self.account].containers {
            if !container.starts_with(prefix) {
                continue;
            }
            let lease = found.lease_state(SystemTime::now());
            out.push_str(&format!(
                "<Container><Name>{}</Name><Properties><Last-Modified>{}</Last-Modified><Etag>{}</Etag><LeaseStatus>{}</LeaseStatus><LeaseState>{}</LeaseState><HasImmutabilityPolicy>{}</HasImmutabilityPolicy><HasLegalHold>{}</HasLegalHold></Properties></Container>",
                xml_escape(container),
                http_date(found.last_modified),
                xml_escape(&found.etag),
                if lease.blocks() { "locked" } else { "unlocked" },
                lease.as_str().to_ascii_lowercase(),
                found.immutability.is_some(),
                !found.legal_hold.is_empty(),
            ));
        }
        out.push_str("</Containers><NextMarker /></EnumerationResults>");
        Ok(xml(StatusCode::OK, out))
    }

    fn put_blob(&self, container: &str, blob: &str, data: Bytes) -> Result<Response, BlobError> {
        match self.header("x-ms-blob-type") {
            Some("BlockBlob") => {}
            Some(_) => return Err(not_implemented()),
            None => {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "MissingRequiredHeader",
                    "An HTTP header that's mandatory for this request is not specified.",
                ))
            }
        }
        let content_md5 = STANDARD.encode(Md5::digest(&data));
        if self.header("content-md5").is_some_and(|sent| sent != content_md5) {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Md5Mismatch",
                "The MD5 value specified in the request did not match with the MD5 value calculated by the server.",
            ));
        }
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let etag = state.next_etag();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        let existing = found.blobs.get(blob);
        if let Some(protection) = existing.and_then(|b| found.protection(b, SystemTime::now())) {
            return Err(immutable(protection));
        }
        if existing.is_some() && self.header("if-none-match") == Some("*") {
            return Err(error(
                StatusCode::CONFLICT,
                "BlobAlreadyExists",
                "The specified blob already exists.",
            ));
        }
        self.preconditions(existing, true)?;
        let now = SystemTime::now();
        let stored = Blob {
            data,
            content_type: self
                .header("x-ms-blob-content-type")
                .or(self.header("content-type"))
                .unwrap_or("application/octet-stream")
                .to_string(),
            content_md5: content_md5.clone(),
            etag: etag.clone(),
            created: existing.map(|b| b.created).unwrap_or(now),
            last_modified: now,
            metadata: self.metadata(),
            content_headers: self.content_headers(),
            blocks: Vec::new(),
            replicated_by: Vec::new(),
            replica_of: None,
            lease: existing.and_then(|b| b.lease.clone()),
            copy: None,
        };
        // Put Blob discards any blocks staged for the blob.
        found.uncommitted.remove(blob);
        found.blobs.insert(blob.to_string(), stored);
        crate::replication::replicate(&mut state, self.account, container, blob);
        Ok(respond(
            StatusCode::CREATED,
            vec![
                (header::ETAG, etag),
                (header::LAST_MODIFIED, http_date(now)),
                (name("content-md5"), content_md5),
                (name("x-ms-request-server-encrypted"), "true".into()),
            ],
            Body::empty(),
        ))
    }

    /// Put Block: stages a block for a blob, to be committed by Put Block List.
    fn put_block(&self, container: &str, blob: &str, data: Bytes) -> Result<Response, BlobError> {
        let id = self.block_id()?;
        let content_md5 = STANDARD.encode(Md5::digest(&data));
        if self.header("content-md5").is_some_and(|sent| sent != content_md5) {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Md5Mismatch",
                "The MD5 value specified in the request did not match with the MD5 value calculated by the server.",
            ));
        }
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        self.check_lease(found.blobs.get(blob), true)?;
        // Every block of a blob has an id of the same length.
        let decoded_len = |id: &str| STANDARD.decode(id).map(|d| d.len()).unwrap_or(0);
        let length = decoded_len(&id);
        let staged = found.uncommitted.get(blob).into_iter().flatten().map(|(i, _)| i.as_str());
        let committed = found.blobs.get(blob).into_iter().flat_map(|b| b.blocks.iter()).map(|(i, _)| i.as_str());
        if staged.chain(committed).any(|other| decoded_len(other) != length) {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "InvalidBlobOrBlock",
                "The specified blob or block content is invalid.",
            ));
        }
        let blocks = found.uncommitted.entry(blob.to_string()).or_default();
        blocks.retain(|(existing, _)| *existing != id);
        blocks.push((id, data));
        Ok(respond(
            StatusCode::CREATED,
            vec![
                (name("content-md5"), content_md5),
                (name("x-ms-request-server-encrypted"), "true".into()),
            ],
            Body::empty(),
        ))
    }

    /// The `blockid` of a Put Block: base64 of at most 64 bytes.
    fn block_id(&self) -> Result<String, BlobError> {
        let id = self.query.get("blockid").ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "MissingRequiredQueryParameter",
                "A query parameter that's mandatory for this request is not specified.",
            )
        })?;
        match STANDARD.decode(id) {
            Ok(decoded) if !decoded.is_empty() && decoded.len() <= 64 => Ok(id.clone()),
            _ => Err(error(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
                "Value for one of the query parameters specified in the request URI is invalid.",
            )),
        }
    }

    /// Put Block List: commits the listed blocks, in order, as the blob's content.
    fn put_block_list(&self, container: &str, blob: &str, body: &[u8]) -> Result<Response, BlobError> {
        let invalid = || error(StatusCode::BAD_REQUEST, "InvalidBlockList", "The specified block list is invalid.");
        let text = std::str::from_utf8(body).map_err(|_| invalid())?;
        let wanted = parse_block_list(text).ok_or_else(|| {
            error(StatusCode::BAD_REQUEST, "InvalidXmlDocument", "XML specified is not syntactically valid.")
        })?;
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let etag = state.next_etag();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        let existing = found.blobs.get(blob);
        if let Some(protection) = existing.and_then(|b| found.protection(b, SystemTime::now())) {
            return Err(immutable(protection));
        }
        if existing.is_some() && self.header("if-none-match") == Some("*") {
            return Err(error(StatusCode::CONFLICT, "BlobAlreadyExists", "The specified blob already exists."));
        }
        self.preconditions(existing, true)?;
        // The committed blocks, as slices of the current content.
        let mut committed: Vec<(String, Bytes)> = Vec::new();
        if let Some(current) = existing {
            let mut offset = 0;
            for (id, size) in &current.blocks {
                committed.push((id.clone(), current.data.slice(offset..offset + size)));
                offset += size;
            }
        }
        let staged = found.uncommitted.get(blob).cloned().unwrap_or_default();
        let lookup = |list: &[(String, Bytes)], id: &str| list.iter().find(|(i, _)| i == id).map(|(_, d)| d.clone());
        let mut content = Vec::new();
        let mut blocks = Vec::new();
        for (kind, id) in &wanted {
            let data = match kind {
                BlockKind::Committed => lookup(&committed, id),
                BlockKind::Uncommitted => lookup(&staged, id),
                BlockKind::Latest => lookup(&staged, id).or_else(|| lookup(&committed, id)),
            }
            .ok_or_else(invalid)?;
            content.extend_from_slice(&data);
            blocks.push((id.clone(), data.len()));
        }
        let now = SystemTime::now();
        let stored = Blob {
            data: Bytes::from(content),
            content_type: self.header("x-ms-blob-content-type").unwrap_or("application/octet-stream").to_string(),
            // Only an MD5 the client states is stored for a blob made from blocks.
            content_md5: self.header("x-ms-blob-content-md5").unwrap_or("").to_string(),
            etag: etag.clone(),
            created: existing.map(|b| b.created).unwrap_or(now),
            last_modified: now,
            metadata: self.metadata(),
            content_headers: self.content_headers(),
            blocks,
            replicated_by: Vec::new(),
            replica_of: None,
            lease: existing.and_then(|b| b.lease.clone()),
            copy: None,
        };
        found.uncommitted.remove(blob);
        found.blobs.insert(blob.to_string(), stored);
        crate::replication::replicate(&mut state, self.account, container, blob);
        Ok(respond(
            StatusCode::CREATED,
            vec![
                (header::ETAG, etag),
                (header::LAST_MODIFIED, http_date(now)),
                // The MD5 of the request: the block list, not the blob.
                (name("content-md5"), STANDARD.encode(Md5::digest(body))),
                (name("x-ms-request-server-encrypted"), "true".into()),
            ],
            Body::empty(),
        ))
    }

    /// Get Block List: the committed blocks, the staged ones, or both.
    fn get_block_list(&self, container: &str, blob: &str) -> Result<Response, BlobError> {
        let kind = self.query.get("blocklisttype").map(|v| v.to_ascii_lowercase());
        let (with_committed, with_uncommitted) = match kind.as_deref() {
            None | Some("committed") => (true, false),
            Some("uncommitted") => (false, true),
            Some("all") => (true, true),
            Some(_) => {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "InvalidQueryParameterValue",
                    "Value for one of the query parameters specified in the request URI is invalid.",
                ))
            }
        };
        let handle = self.engine.state::<StorageState>();
        let state = handle.lock();
        let found = state.accounts[self.account].containers.get(container).ok_or_else(container_not_found)?;
        let current = found.blobs.get(blob);
        let staged = found.uncommitted.get(blob);
        if current.is_none() && staged.is_none() {
            return Err(blob_not_found());
        }
        let entries = |blocks: Vec<(&str, usize)>| -> String {
            blocks
                .iter()
                .map(|(id, size)| format!("<Block><Name>{}</Name><Size>{size}</Size></Block>", xml_escape(id)))
                .collect()
        };
        let mut xml_body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        if with_committed {
            let blocks = current.map(|b| b.blocks.iter().map(|(i, s)| (i.as_str(), *s)).collect()).unwrap_or_default();
            xml_body.push_str(&format!("<CommittedBlocks>{}</CommittedBlocks>", entries(blocks)));
        }
        if with_uncommitted {
            let blocks = staged.map(|b| b.iter().map(|(i, d)| (i.as_str(), d.len())).collect()).unwrap_or_default();
            xml_body.push_str(&format!("<UncommittedBlocks>{}</UncommittedBlocks>", entries(blocks)));
        }
        xml_body.push_str("</BlockList>");
        let mut headers = vec![(header::CONTENT_TYPE, "application/xml".to_string())];
        if let Some(current) = current {
            headers.push((header::ETAG, current.etag.clone()));
            headers.push((header::LAST_MODIFIED, http_date(current.last_modified)));
            headers.push((name("x-ms-blob-content-length"), current.data.len().to_string()));
        }
        Ok(respond(StatusCode::OK, headers, xml_body))
    }

    fn get_metadata(&self, container: &str, blob: &str) -> Result<Response, BlobError> {
        let found = self.find_blob(container, blob)?;
        let mut headers = vec![
            (header::ETAG, found.etag.clone()),
            (header::LAST_MODIFIED, http_date(found.last_modified)),
        ];
        for (key, value) in &found.metadata {
            if let Ok(key) = HeaderName::try_from(format!("x-ms-meta-{key}")) {
                headers.push((key, value.clone()));
            }
        }
        Ok(respond(StatusCode::OK, headers, Body::empty()))
    }

    /// Set Blob Metadata and Set Blob Properties: change a blob without
    /// rewriting it. Properties not given are cleared, as in Azure.
    fn update_blob(&self, container: &str, blob: &str, change: impl FnOnce(&mut Blob)) -> Result<Response, BlobError> {
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let etag = state.next_etag();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        let existing = found.blobs.get(blob).ok_or_else(blob_not_found)?;
        if let Some(protection) = found.protection(existing, SystemTime::now()) {
            return Err(immutable(protection));
        }
        self.preconditions(Some(existing), true)?;
        let target = found.blobs.get_mut(blob).expect("checked above");
        change(target);
        let now = SystemTime::now();
        target.etag = etag.clone();
        target.last_modified = now;
        Ok(respond(
            StatusCode::OK,
            vec![
                (header::ETAG, etag),
                (header::LAST_MODIFIED, http_date(now)),
                (name("x-ms-request-server-encrypted"), "true".into()),
            ],
            Body::empty(),
        ))
    }

    fn set_metadata(&self, container: &str, blob: &str) -> Result<Response, BlobError> {
        let metadata = self.metadata();
        self.update_blob(container, blob, |b| b.metadata = metadata)
    }

    fn set_properties(&self, container: &str, blob: &str) -> Result<Response, BlobError> {
        let content_type = self.header("x-ms-blob-content-type").unwrap_or("application/octet-stream").to_string();
        let content_md5 = self.header("x-ms-blob-content-md5").unwrap_or("").to_string();
        let content_headers = self.content_headers();
        self.update_blob(container, blob, |b| {
            b.content_type = content_type;
            b.content_md5 = content_md5;
            b.content_headers = content_headers;
        })
    }

    /// The blob, or the snapshot of it `?snapshot=` names, checked against
    /// the request's lease and conditional headers as a read.
    fn find_blob(&self, container: &str, blob: &str) -> Result<Blob, BlobError> {
        let handle = self.engine.state::<StorageState>();
        let state = handle.lock();
        let found = state.accounts[self.account].containers.get(container).ok_or_else(container_not_found)?;
        let target = match self.query.get("snapshot") {
            Some(stamp) => found.snapshots.get(blob).and_then(|s| s.get(stamp)),
            None => found.blobs.get(blob),
        }
        .ok_or_else(blob_not_found)?;
        self.preconditions(Some(target), false)?;
        Ok(target.clone())
    }

    fn get_blob(&self, container: &str, blob: &str, with_body: bool) -> Result<Response, BlobError> {
        let found = self.find_blob(container, blob)?;
        let len = found.data.len();
        let range = if with_body {
            requested_range(self.headers, len).map_err(|()| {
                let mut failure = error(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "InvalidRange",
                    "The range specified is invalid for the current size of the resource.",
                );
                failure.headers.push((header::CONTENT_RANGE, format!("bytes */{len}")));
                failure
            })?
        } else {
            None
        };
        let mut headers = vec![
            (header::CONTENT_TYPE, found.content_type.clone()),
            (header::ETAG, found.etag.clone()),
            (header::LAST_MODIFIED, http_date(found.last_modified)),
            (header::ACCEPT_RANGES, "bytes".into()),
            (name("x-ms-creation-time"), http_date(found.created)),
            (name("x-ms-blob-type"), "BlockBlob".into()),
            (name("x-ms-server-encrypted"), "true".into()),
        ];
        for pair in &found.replicated_by {
            if let Ok(key) = HeaderName::try_from(format!("x-ms-or-{pair}")) {
                headers.push((key, "complete".into()));
            }
        }
        if let Some(policy) = &found.replica_of {
            headers.push((name("x-ms-or-policy-id"), policy.clone()));
        }
        lease_headers(&mut headers, found.lease.as_ref());
        if let Some(copy) = &found.copy {
            headers.push((name("x-ms-copy-id"), copy.id.clone()));
            headers.push((name("x-ms-copy-source"), copy.source.clone()));
            headers.push((name("x-ms-copy-status"), "success".into()));
            headers.push((name("x-ms-copy-progress"), format!("{0}/{0}", copy.length)));
            headers.push((name("x-ms-copy-completion-time"), http_date(copy.completed)));
        }
        for (key, value) in &found.content_headers {
            headers.push((name(key), value.clone()));
        }
        for (key, value) in &found.metadata {
            if let Ok(key) = HeaderName::try_from(format!("x-ms-meta-{key}")) {
                headers.push((key, value.clone()));
            }
        }
        let (status, data) = match range {
            Some((start, end)) => {
                headers.push((header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}")));
                // A ranged read reports the whole blob's MD5 under its own header.
                if !found.content_md5.is_empty() {
                    headers.push((name("x-ms-blob-content-md5"), found.content_md5.clone()));
                }
                (StatusCode::PARTIAL_CONTENT, found.data.slice(start..=end))
            }
            None => {
                // A blob committed from blocks has an MD5 only if the client gave one.
                if !found.content_md5.is_empty() {
                    headers.push((name("content-md5"), found.content_md5.clone()));
                }
                (StatusCode::OK, found.data.clone())
            }
        };
        headers.push((header::CONTENT_LENGTH, data.len().to_string()));
        let body = if with_body { Body::from(data) } else { Body::empty() };
        Ok(respond(status, headers, body))
    }

    fn delete_blob(&self, container: &str, blob: &str) -> Result<Response, BlobError> {
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        if let Some(stamp) = self.query.get("snapshot") {
            let snapshots = found.snapshots.get_mut(blob).ok_or_else(blob_not_found)?;
            snapshots.remove(stamp).ok_or_else(blob_not_found)?;
            if snapshots.is_empty() {
                found.snapshots.remove(blob);
            }
            return Ok(respond(StatusCode::ACCEPTED, Vec::new(), Body::empty()));
        }
        let existing = found.blobs.get(blob).ok_or_else(blob_not_found)?;
        if let Some(protection) = found.protection(existing, SystemTime::now()) {
            return Err(immutable(protection));
        }
        self.preconditions(Some(existing), true)?;
        let has_snapshots = found.snapshots.get(blob).is_some_and(|s| !s.is_empty());
        match self.header("x-ms-delete-snapshots") {
            Some("only") => {
                found.snapshots.remove(blob);
                return Ok(respond(StatusCode::ACCEPTED, Vec::new(), Body::empty()));
            }
            Some("include") => {
                found.snapshots.remove(blob);
            }
            None if has_snapshots => {
                return Err(error(
                    StatusCode::CONFLICT,
                    "SnapshotsPresent",
                    "This operation is not permitted because the blob has snapshots.",
                ))
            }
            None => {}
            Some(_) => {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "The value for one of the HTTP headers is not in the correct format.",
                ))
            }
        }
        found.blobs.remove(blob);
        Ok(respond(
            StatusCode::ACCEPTED,
            vec![(name("x-ms-delete-type-permanent"), "true".into())],
            Body::empty(),
        ))
    }

    fn list_blobs(&self, container: &str) -> Result<Response, BlobError> {
        let param = |key: &str| self.query.get(key).map(String::as_str).filter(|v| !v.is_empty());
        let prefix = param("prefix").unwrap_or("");
        let delimiter = param("delimiter");
        let marker = param("marker");
        let included = |what: &str| param("include").is_some_and(|i| i.split(',').any(|p| p == what));
        let (with_metadata, with_snapshots) = (included("metadata"), included("snapshots"));
        let max_results = match param("maxresults").map(str::parse::<usize>) {
            None => 5000,
            Some(Ok(n)) if (1..=5000).contains(&n) => n,
            Some(_) => {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "OutOfRangeQueryParameterValue",
                    "One of the query parameters specified in the request URI is outside the permissible range.",
                ))
            }
        };

        let handle = self.engine.state::<StorageState>();
        let state = handle.lock();
        let found = state.accounts[self.account]
            .containers
            .get(container)
            .ok_or_else(container_not_found)?;

        // One entry per blob, or per distinct prefix up to the delimiter, in name order.
        let mut entries: Vec<(String, Option<&Blob>)> = Vec::new();
        for (blob_name, blob) in found.blobs.range(prefix.to_string()..) {
            if !blob_name.starts_with(prefix) {
                break;
            }
            let rest = &blob_name[prefix.len()..];
            let entry = match delimiter.and_then(|d| rest.find(d).map(|at| at + d.len())) {
                Some(end) => (format!("{prefix}{}", &rest[..end]), None),
                None => (blob_name.clone(), Some(blob)),
            };
            if entries.last().map(|(last, _)| last) != Some(&entry.0) {
                entries.push(entry);
            }
        }
        let start = marker
            .map(|m| entries.partition_point(|(entry, _)| entry.as_str() <= m))
            .unwrap_or(0);
        let page = &entries[start..entries.len().min(start + max_results)];
        let next_marker = (start + max_results < entries.len())
            .then(|| page.last().map(|(entry, _)| entry.clone()))
            .flatten();

        let mut out = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ServiceEndpoint=\"{}\" ContainerName=\"{}\">",
            xml_escape(&self.engine.config().blob_endpoint(self.account)),
            xml_escape(container)
        );
        for (tag, value) in [("Prefix", param("prefix")), ("Marker", marker), ("Delimiter", delimiter)] {
            if let Some(value) = value {
                out.push_str(&format!("<{tag}>{}</{tag}>", xml_escape(value)));
            }
        }
        if let Some(requested) = param("maxresults") {
            out.push_str(&format!("<MaxResults>{requested}</MaxResults>"));
        }
        out.push_str("<Blobs>");
        let write_blob = |out: &mut String, entry: &str, blob: &Blob, snapshot: Option<&str>| {
            let (status, state, duration) = lease_properties(blob.lease.as_ref());
            let duration = duration.map(|d| format!("<LeaseDuration>{d}</LeaseDuration>")).unwrap_or_default();
            out.push_str(&format!("<Blob><Name>{}</Name>", xml_escape(entry)));
            if let Some(snapshot) = snapshot {
                out.push_str(&format!("<Snapshot>{snapshot}</Snapshot>"));
            }
            out.push_str(&format!(
                "<Properties><Creation-Time>{}</Creation-Time><Last-Modified>{}</Last-Modified><Etag>{}</Etag><Content-Length>{}</Content-Length><Content-Type>{}</Content-Type><Content-MD5>{}</Content-MD5><BlobType>BlockBlob</BlobType><LeaseStatus>{status}</LeaseStatus><LeaseState>{state}</LeaseState>{duration}<ServerEncrypted>true</ServerEncrypted></Properties>",
                http_date(blob.created),
                http_date(blob.last_modified),
                xml_escape(blob.etag.trim_matches('"')),
                blob.data.len(),
                xml_escape(&blob.content_type),
                blob.content_md5,
            ));
            if with_metadata {
                out.push_str("<Metadata>");
                for (key, value) in &blob.metadata {
                    out.push_str(&format!("<{key}>{}</{key}>", xml_escape(value)));
                }
                out.push_str("</Metadata>");
            }
            out.push_str("</Blob>");
        };
        for (entry, blob) in page {
            let Some(blob) = blob else {
                out.push_str(&format!("<BlobPrefix><Name>{}</Name></BlobPrefix>", xml_escape(entry)));
                continue;
            };
            // Snapshots come before the blob they were taken of, oldest first.
            if with_snapshots {
                for (stamp, snapshot) in found.snapshots.get(entry).into_iter().flatten() {
                    write_blob(&mut out, entry, snapshot, Some(stamp));
                }
            }
            write_blob(&mut out, entry, blob, None);
        }
        out.push_str("</Blobs>");
        match next_marker {
            Some(next) => out.push_str(&format!("<NextMarker>{}</NextMarker>", xml_escape(&next))),
            None => out.push_str("<NextMarker />"),
        }
        out.push_str("</EnumerationResults>");
        Ok(xml(StatusCode::OK, out))
    }
}

/// Leases, snapshots, copies, and the lease and conditional headers every
/// operation on a blob honours.
impl BlobRequest<'_> {
    /// The lease and conditional headers, checked against `blob` (`None`:
    /// it does not exist). `write` is whether the request changes it.
    fn preconditions(&self, blob: Option<&Blob>, write: bool) -> Result<(), BlobError> {
        self.check_lease(blob, write)?;
        self.check_conditions(blob.map(|b| (b.etag.as_str(), b.last_modified)), write, "")
    }

    /// An active lease lets a write through only with its id; any request
    /// that names a lease must name the active one.
    fn check_lease(&self, blob: Option<&Blob>, write: bool) -> Result<(), BlobError> {
        let now = SystemTime::now();
        let active = blob.and_then(|b| b.lease.as_ref()).filter(|l| lease_state(Some(l), now).blocks());
        let failed = |code, message| Err(error(StatusCode::PRECONDITION_FAILED, code, message));
        match (self.header("x-ms-lease-id"), active) {
            (None, Some(_)) if write => failed(
                "LeaseIdMissing",
                "There is currently a lease on the blob and no lease ID was specified in the request.",
            ),
            (None, _) => Ok(()),
            (Some(_), None) => failed("LeaseNotPresentWithBlobOperation", "There is currently no lease on the blob."),
            (Some(sent), Some(lease)) if !sent.eq_ignore_ascii_case(&lease.id) => failed(
                "LeaseIdMismatchWithBlobOperation",
                "The lease ID specified did not match the lease ID for the blob.",
            ),
            _ => Ok(()),
        }
    }

    /// `If-Match`, `If-None-Match`, `If-Modified-Since` and
    /// `If-Unmodified-Since`, or with `prefix` `x-ms-source-` the same for a
    /// copy's source. A read that fails `If-None-Match` or
    /// `If-Modified-Since` is answered 304; everything else that fails, 412.
    fn check_conditions(&self, target: Option<(&str, SystemTime)>, write: bool, prefix: &str) -> Result<(), BlobError> {
        let get = |key: &str| self.header(&format!("{prefix}{key}"));
        let failed = || match prefix {
            "" => error(
                StatusCode::PRECONDITION_FAILED,
                "ConditionNotMet",
                "The condition specified using HTTP conditional header(s) is not met.",
            ),
            _ => error(
                StatusCode::PRECONDITION_FAILED,
                "SourceConditionNotMet",
                "The source condition specified using HTTP conditional header(s) is not met.",
            ),
        };
        let unchanged = || match write || !prefix.is_empty() {
            true => failed(),
            false => error(StatusCode::NOT_MODIFIED, "ConditionNotMet", "The condition specified using HTTP conditional header(s) is not met."),
        };
        let listed = |list: &str, etag: &str| list.split(',').any(|tag| tag.trim() == "*" || bare_etag(tag) == bare_etag(etag));
        if let Some(list) = get("if-match") {
            if !target.is_some_and(|(etag, _)| listed(list, etag)) {
                return Err(failed());
            }
        }
        if let (Some(list), Some((etag, _))) = (get("if-none-match"), target) {
            if listed(list, etag) {
                return Err(unchanged());
            }
        }
        let date = |key: &str| get(key).and_then(|v| httpdate::parse_http_date(v).ok()).map(unix_seconds);
        if let (Some(since), Some((_, modified))) = (date("if-modified-since"), target) {
            if unix_seconds(modified) <= since {
                return Err(unchanged());
            }
        }
        if let (Some(since), Some((_, modified))) = (date("if-unmodified-since"), target) {
            if unix_seconds(modified) > since {
                return Err(failed());
            }
        }
        Ok(())
    }

    /// Lease Blob and Lease Container: acquire, renew, change, release and
    /// break, on whichever lease `lease` is.
    fn lease_action(&self, lease: &mut Option<Lease>, what: &str) -> Result<(StatusCode, Headers), BlobError> {
        let now = SystemTime::now();
        let state = lease_state(lease.as_ref(), now);
        let conflict = |code: &'static str, message: &'static str| Err(error(StatusCode::CONFLICT, code, message));
        let invalid = || {
            error(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The value for one of the HTTP headers is not in the correct format.",
            )
        };
        let required = |key: &str| {
            self.header(key).ok_or_else(|| {
                error(
                    StatusCode::BAD_REQUEST,
                    "MissingRequiredHeader",
                    "An HTTP header that's mandatory for this request is not specified.",
                )
            })
        };
        let not_present = if what == "blob" { "There is currently no lease on the blob." } else { "There is currently no lease on the container." };
        let current = lease.as_ref().map(|l| l.id.clone());
        let is_current = |id: &str| current.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(id));
        let lease_id = |id: String| vec![(name("x-ms-lease-id"), id)];
        match required("x-ms-lease-action")?.to_ascii_lowercase().as_str() {
            "acquire" => {
                let duration = match self.header("x-ms-lease-duration").map(str::parse::<i64>) {
                    Some(Ok(-1)) => None,
                    Some(Ok(seconds)) if (15..=60).contains(&seconds) => Some(seconds as u64),
                    _ => return Err(invalid()),
                };
                let proposed = self.header("x-ms-proposed-lease-id");
                if proposed.is_some_and(|id| uuid::Uuid::parse_str(id).is_err()) {
                    return Err(invalid());
                }
                match state {
                    LeaseState::Breaking => {
                        return conflict("LeaseIsBreakingAndCannotBeAcquired", "There is already a lease present.")
                    }
                    LeaseState::Leased if !proposed.is_some_and(is_current) => {
                        return conflict("LeaseAlreadyPresent", "There is already a lease present.")
                    }
                    _ => {}
                }
                let id = proposed.map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                *lease = Some(Lease { id: id.clone(), duration, acquired: now, broken_at: None });
                Ok((StatusCode::CREATED, lease_id(id)))
            }
            "renew" => {
                let id = required("x-ms-lease-id")?;
                match (state, lease.as_mut()) {
                    (_, None) => conflict("LeaseNotPresentWithLeaseOperation", not_present),
                    _ if !is_current(id) => conflict(
                        "LeaseIdMismatchWithLeaseOperation",
                        "The lease ID specified did not match the lease ID for the lease.",
                    ),
                    (LeaseState::Breaking | LeaseState::Broken, _) => conflict(
                        "LeaseIsBrokenAndCannotBeRenewed",
                        "The lease ID matched, but the lease has been broken explicitly and cannot be renewed.",
                    ),
                    (_, Some(held)) => {
                        held.acquired = now;
                        Ok((StatusCode::OK, lease_id(held.id.clone())))
                    }
                }
            }
            "change" => {
                let id = required("x-ms-lease-id")?;
                let proposed = required("x-ms-proposed-lease-id")?;
                if uuid::Uuid::parse_str(proposed).is_err() {
                    return Err(invalid());
                }
                match (state, lease.as_mut()) {
                    (LeaseState::Leased, Some(held)) if is_current(id) || is_current(proposed) => {
                        held.id = proposed.to_string();
                        Ok((StatusCode::OK, lease_id(proposed.to_string())))
                    }
                    (LeaseState::Leased, _) => conflict(
                        "LeaseIdMismatchWithLeaseOperation",
                        "The lease ID specified did not match the lease ID for the lease.",
                    ),
                    (LeaseState::Breaking, _) => conflict("LeaseIsBreakingAndCannotBeChanged", "The lease is breaking and cannot be changed."),
                    (LeaseState::Broken, _) => conflict("LeaseIsBrokenAndCannotBeRenewed", "The lease has been broken and cannot be changed."),
                    _ => conflict("LeaseNotPresentWithLeaseOperation", not_present),
                }
            }
            "release" => {
                let id = required("x-ms-lease-id")?;
                if lease.is_none() {
                    return conflict("LeaseNotPresentWithLeaseOperation", not_present);
                }
                if !is_current(id) {
                    return conflict(
                        "LeaseIdMismatchWithLeaseOperation",
                        "The lease ID specified did not match the lease ID for the lease.",
                    );
                }
                *lease = None;
                Ok((StatusCode::OK, Vec::new()))
            }
            "break" => {
                let period = match self.header("x-ms-lease-break-period").map(str::parse::<u64>) {
                    None => None,
                    Some(Ok(seconds)) if seconds <= 60 => Some(seconds),
                    Some(_) => return Err(invalid()),
                };
                let Some(held) = lease.as_mut() else {
                    return conflict("LeaseNotPresentWithLeaseOperation", not_present);
                };
                let broken_at = match state {
                    LeaseState::Leased => {
                        let remaining = held.duration.map(|d| (held.acquired + std::time::Duration::from_secs(d)).duration_since(now).unwrap_or_default().as_secs());
                        let wait = match (period, remaining) {
                            (Some(p), Some(r)) => p.min(r),
                            (Some(p), None) => p,
                            (None, Some(r)) => r,
                            (None, None) => 0,
                        };
                        now + std::time::Duration::from_secs(wait)
                    }
                    LeaseState::Breaking => {
                        let at = held.broken_at.unwrap_or(now);
                        period.map(|p| at.min(now + std::time::Duration::from_secs(p))).unwrap_or(at)
                    }
                    _ => held.broken_at.unwrap_or(now),
                };
                held.broken_at = Some(broken_at);
                let left = broken_at.duration_since(now).map(|d| d.as_secs()).unwrap_or(0);
                Ok((StatusCode::ACCEPTED, vec![(name("x-ms-lease-time"), left.to_string())]))
            }
            _ => Err(invalid()),
        }
    }

    fn lease_blob(&self, container: &str, blob: &str) -> Result<Response, BlobError> {
        if self.query.contains_key("snapshot") {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
                "Value for one of the query parameters specified in the request URI is invalid.",
            ));
        }
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        let target = found.blobs.get_mut(blob).ok_or_else(blob_not_found)?;
        self.check_conditions(Some((&target.etag, target.last_modified)), true, "")?;
        let (status, mut headers) = self.lease_action(&mut target.lease, "blob")?;
        headers.push((header::ETAG, target.etag.clone()));
        headers.push((header::LAST_MODIFIED, http_date(target.last_modified)));
        Ok(respond(status, headers, Body::empty()))
    }

    fn lease_container(&self, container: &str) -> Result<Response, BlobError> {
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        self.check_conditions(Some((&found.etag, found.last_modified)), true, "")?;
        let (status, mut headers) = self.lease_action(&mut found.lease, "container")?;
        headers.push((header::ETAG, found.etag.clone()));
        headers.push((header::LAST_MODIFIED, http_date(found.last_modified)));
        Ok(respond(status, headers, Body::empty()))
    }

    /// Snapshot Blob: a read-only copy of the blob as it is now, with new
    /// metadata if the request gives any.
    fn snapshot_blob(&self, container: &str, blob: &str) -> Result<Response, BlobError> {
        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        let base = found.blobs.get(blob).ok_or_else(blob_not_found)?;
        self.check_lease(Some(base), false)?;
        self.check_conditions(Some((&base.etag, base.last_modified)), true, "")?;
        let mut snapshot = base.clone();
        snapshot.lease = None;
        let metadata = self.metadata();
        if !metadata.is_empty() {
            snapshot.metadata = metadata;
        }
        let stamp = snapshot_time(found.snapshots.get(blob));
        let headers = vec![
            (name("x-ms-snapshot"), stamp.clone()),
            (header::ETAG, snapshot.etag.clone()),
            (header::LAST_MODIFIED, http_date(snapshot.last_modified)),
            (name("x-ms-request-server-encrypted"), "false".into()),
        ];
        found.snapshots.entry(blob.to_string()).or_default().insert(stamp, snapshot);
        Ok(respond(StatusCode::CREATED, headers, Body::empty()))
    }

    /// Copy Blob, Copy Blob From URL and Put Blob From URL from a blob (or
    /// snapshot) this emulator holds. Every copy completes before the
    /// response, so a copy is never pending.
    fn copy_blob(&self, container: &str, blob: &str) -> Result<Response, BlobError> {
        let put_from_url = self.header("x-ms-blob-type").is_some();
        if put_from_url && self.header("x-ms-blob-type") != Some("BlockBlob") {
            return Err(not_implemented());
        }
        let source_url = self.header("x-ms-copy-source").unwrap_or_default().to_string();
        let cannot_verify = |status, message: &'static str| error(status, "CannotVerifyCopySource", message);
        let uri: http::Uri = source_url
            .parse()
            .map_err(|_| cannot_verify(StatusCode::BAD_REQUEST, "The copy source is not a valid URL."))?;
        let host = request_host(&HeaderMap::new(), &uri);
        let config = self.engine.config();
        let source_account = host
            .strip_suffix(&config.blob_host_suffix())
            .or_else(|| host.strip_suffix(".blob.core.windows.net"))
            .ok_or_else(|| cannot_verify(StatusCode::BAD_REQUEST, "The emulator copies only from blobs it holds."))?
            .to_string();
        let path = uri.path().trim_start_matches('/');
        let (source_container, source_blob) = path
            .split_once('/')
            .map(|(c, b)| (percent_decode(c, false), percent_decode(b, false)))
            .ok_or_else(|| cannot_verify(StatusCode::BAD_REQUEST, "The copy source is not a blob."))?;
        let source_query: std::collections::BTreeMap<String, String> =
            parse_query(uri.query()).into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();

        let handle = self.engine.state::<StorageState>();
        let mut state = handle.lock();
        let source = {
            let missing = || cannot_verify(StatusCode::NOT_FOUND, "The specified blob does not exist.");
            let account = state.accounts.get(&source_account).ok_or_else(missing)?;
            let holder = account.containers.get(&source_container).ok_or_else(missing)?;
            let source = match source_query.get("snapshot") {
                Some(stamp) => holder.snapshots.get(&source_blob).and_then(|s| s.get(stamp)),
                None => holder.blobs.get(&source_blob),
            }
            .ok_or_else(missing)?;
            // Another account's blob must be public or come with a SAS.
            if source_account != self.account && holder.public_access == "None" {
                let allowed = source_query.contains_key("sig")
                    && sas::authorize(&source_query, &source_account, &account.keys, &source_container, &source_blob, Route::GetBlob.needed()).is_ok();
                if !allowed {
                    return Err(cannot_verify(StatusCode::FORBIDDEN, "Server failed to authenticate the request."));
                }
            }
            self.check_conditions(Some((&source.etag, source.last_modified)), true, "x-ms-source-")?;
            source.clone()
        };

        let etag = state.next_etag();
        let account = state.accounts.get_mut(self.account).expect("account checked");
        let found = account.containers.get_mut(container).ok_or_else(container_not_found)?;
        let existing = found.blobs.get(blob);
        if let Some(protection) = existing.and_then(|b| found.protection(b, SystemTime::now())) {
            return Err(immutable(protection));
        }
        if existing.is_some() && self.header("if-none-match") == Some("*") {
            return Err(error(StatusCode::CONFLICT, "BlobAlreadyExists", "The specified blob already exists."));
        }
        self.preconditions(existing, true)?;
        let now = SystemTime::now();
        let metadata = self.metadata();
        // Put Blob From URL takes its properties from the request unless told
        // to copy the source's; Copy Blob takes the source's.
        let copy_properties = !put_from_url || self.header("x-ms-copy-source-blob-properties") != Some("false");
        let content_headers = self.content_headers();
        let copy_id = uuid::Uuid::new_v4().to_string();
        let stored = Blob {
            data: source.data.clone(),
            content_type: match self.header("x-ms-blob-content-type") {
                Some(sent) if put_from_url => sent.to_string(),
                _ if copy_properties => source.content_type.clone(),
                _ => "application/octet-stream".into(),
            },
            content_md5: source.content_md5.clone(),
            etag: etag.clone(),
            created: existing.map(|b| b.created).unwrap_or(now),
            last_modified: now,
            metadata: if metadata.is_empty() && !put_from_url { source.metadata.clone() } else { metadata },
            content_headers: if (put_from_url && !content_headers.is_empty()) || !copy_properties {
                content_headers
            } else {
                source.content_headers.clone()
            },
            blocks: source.blocks.clone(),
            replicated_by: Vec::new(),
            replica_of: None,
            lease: existing.and_then(|b| b.lease.clone()),
            copy: (!put_from_url).then(|| CopyInfo {
                id: copy_id.clone(),
                source: source_url.clone(),
                length: source.data.len(),
                completed: now,
            }),
        };
        found.uncommitted.remove(blob);
        found.blobs.insert(blob.to_string(), stored);
        crate::replication::replicate(&mut state, self.account, container, blob);
        let mut headers = vec![(header::ETAG, etag), (header::LAST_MODIFIED, http_date(now))];
        if put_from_url {
            if !source.content_md5.is_empty() {
                headers.push((name("content-md5"), source.content_md5.clone()));
            }
            headers.push((name("x-ms-request-server-encrypted"), "true".into()));
            return Ok(respond(StatusCode::CREATED, headers, Body::empty()));
        }
        headers.push((name("x-ms-copy-id"), copy_id));
        headers.push((name("x-ms-copy-status"), "success".into()));
        Ok(respond(StatusCode::ACCEPTED, headers, Body::empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_times_have_seven_digits_and_increase() {
        let first = snapshot_time(None);
        assert_eq!(first.len(), "2024-01-02T03:04:05.1234567Z".len());
        assert!(first.ends_with('Z'));
        let now = SystemTime::now();
        let blob = Blob {
            data: Bytes::new(),
            content_type: String::new(),
            content_md5: String::new(),
            etag: String::new(),
            created: now,
            last_modified: now,
            metadata: Vec::new(),
            content_headers: Vec::new(),
            blocks: Vec::new(),
            replicated_by: Vec::new(),
            replica_of: None,
            lease: None,
            copy: None,
        };
        // A snapshot taken in the same instant as the last one still sorts after it.
        let latest = snapshot_time(None);
        let mut taken = std::collections::BTreeMap::new();
        taken.insert(latest.clone(), blob);
        assert!(snapshot_time(Some(&taken)) > latest);
    }

    #[test]
    fn etags_compare_without_quotes() {
        assert_eq!(bare_etag("W/\"0x1\""), "0x1");
        assert_eq!(bare_etag(" \"0x1\" "), "0x1");
    }
}
