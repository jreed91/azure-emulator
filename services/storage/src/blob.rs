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
use emulator_core::http_util::{parse_query, percent_decode, read_body, request_id};
use emulator_core::Engine;
use http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use md5::{Digest, Md5};

use crate::sas::{self, Level, Needed, SasFailure};
use crate::shared_key;
use crate::state::{Blob, Container, Protection, StorageState};

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
            let mut response = xml(failure.status, if is_head { String::new() } else { body });
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
}

impl Route {
    fn of(method: &str, container: &str, blob: &str, restype: Option<&str>, comp: Option<&str>) -> Option<Self> {
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
                _ => None,
            };
        }
        if comp.is_some() {
            return None;
        }
        match method {
            "PUT" => Some(Self::PutBlob),
            "GET" => Some(Self::GetBlob),
            "HEAD" => Some(Self::HeadBlob),
            "DELETE" => Some(Self::DeleteBlob),
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
            Self::PutBlob => (Level::Object, "wc", false),
            Self::GetBlob | Self::HeadBlob => (Level::Object, "r", false),
            Self::DeleteBlob => (Level::Object, "d", false),
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
        Route::CreateContainer => vec![Permission::Action(&write)],
        Route::DeleteContainer => vec![Permission::Action(&delete)],
        Route::ListBlobs | Route::GetBlob | Route::HeadBlob => vec![Permission::Data(&blob_read)],
        Route::PutBlob if is_new() => vec![Permission::Data(&blob_write), Permission::Data(&blob_add)],
        Route::PutBlob => vec![Permission::Data(&blob_write)],
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
    let route = Route::of(parts.method.as_str(), &container, &blob, param("restype"), param("comp"));
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
        Route::PutBlob => {
            let data = read_body(body).await.map_err(|_| {
                error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "RequestBodyTooLarge",
                    "The request body is too large and exceeds the maximum permissible limit.",
                )
            })?;
            request.put_blob(&container, &blob, data)
        }
        Route::GetBlob => request.get_blob(&container, &blob, true),
        Route::HeadBlob => request.get_blob(&container, &blob, false),
        Route::DeleteBlob => request.delete_blob(&container, &blob),
    }
}

struct BlobRequest<'a> {
    engine: &'a Engine,
    account: &'a str,
    headers: &'a HeaderMap,
    query: &'a std::collections::BTreeMap<String, String>,
}

pub(crate) fn is_valid_container_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && !name.contains("--")
}

fn lease_headers(headers: &mut Headers) {
    headers.push((name("x-ms-lease-status"), "unlocked".into()));
    headers.push((name("x-ms-lease-state"), "available".into()));
}

fn container_lease_headers(headers: &mut Headers, container: &Container) {
    let state = container.lease_state(SystemTime::now());
    let status = if state.blocks() { "locked" } else { "unlocked" };
    headers.push((name("x-ms-lease-status"), status.into()));
    headers.push((name("x-ms-lease-state"), state.as_str().to_ascii_lowercase()));
    if state.blocks() {
        let infinite = container.lease.as_ref().is_some_and(|l| l.duration.is_none());
        headers.push((name("x-ms-lease-duration"), if infinite { "infinite" } else { "fixed" }.into()));
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
            replicated_by: Vec::new(),
            replica_of: None,
        };
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

    fn find_blob(&self, container: &str, blob: &str) -> Result<Blob, BlobError> {
        let handle = self.engine.state::<StorageState>();
        let state = handle.lock();
        state.accounts[self.account]
            .containers
            .get(container)
            .ok_or_else(container_not_found)?
            .blobs
            .get(blob)
            .cloned()
            .ok_or_else(blob_not_found)
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
        lease_headers(&mut headers);
        for (key, value) in &found.metadata {
            if let Ok(key) = HeaderName::try_from(format!("x-ms-meta-{key}")) {
                headers.push((key, value.clone()));
            }
        }
        let (status, data) = match range {
            Some((start, end)) => {
                headers.push((header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}")));
                // A ranged read reports the whole blob's MD5 under its own header.
                headers.push((name("x-ms-blob-content-md5"), found.content_md5.clone()));
                (StatusCode::PARTIAL_CONTENT, found.data.slice(start..=end))
            }
            None => {
                headers.push((name("content-md5"), found.content_md5.clone()));
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
        let existing = found.blobs.get(blob).ok_or_else(blob_not_found)?;
        if let Some(protection) = found.protection(existing, SystemTime::now()) {
            return Err(immutable(protection));
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
        let with_metadata = param("include").is_some_and(|i| i.split(',').any(|p| p == "metadata"));
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
        for (entry, blob) in page {
            let Some(blob) = blob else {
                out.push_str(&format!("<BlobPrefix><Name>{}</Name></BlobPrefix>", xml_escape(entry)));
                continue;
            };
            out.push_str(&format!(
                "<Blob><Name>{}</Name><Properties><Creation-Time>{}</Creation-Time><Last-Modified>{}</Last-Modified><Etag>{}</Etag><Content-Length>{}</Content-Length><Content-Type>{}</Content-Type><Content-MD5>{}</Content-MD5><BlobType>BlockBlob</BlobType><LeaseStatus>unlocked</LeaseStatus><LeaseState>available</LeaseState><ServerEncrypted>true</ServerEncrypted></Properties>",
                xml_escape(entry),
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
