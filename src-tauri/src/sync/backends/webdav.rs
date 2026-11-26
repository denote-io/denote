//! WebDAV transport: `PROPFIND`, `MKCOL`, `PUT`, `GET`, and `DELETE` under
//! `<base URL>/denote-sync/v1/`, authenticated with HTTP Basic auth.
//!
//! Atomic replacement is the server's responsibility; mainstream servers
//! (Apache `mod_dav`, Nextcloud, nginx `dav_ext`, rclone) write a `PUT` body
//! to a temporary file before replacing the target.

// Until the backend factory constructs the network transports, only tests
// reach them.
#![cfg_attr(not(test), allow(dead_code))]

use std::{
    collections::HashSet,
    sync::{Mutex, MutexGuard},
};

use quick_xml::{Reader, events::Event};
use reqwest::{
    Method, StatusCode, Url,
    blocking::RequestBuilder,
    header::{CONTENT_TYPE, IF_NONE_MATCH},
};
use zeroize::Zeroizing;

use crate::{
    error::{AppError, AppResult},
    sync::backend::{Backend, PutMode, PutOutcome, REMOTE_PREFIX, validate_key, validate_prefix},
};

use super::http::{
    HttpClient, HttpResponse, MAX_LISTING_BYTES, MAX_OBJECT_BYTES, parse_service_url,
    percent_decode,
};

const SERVICE: &str = "WebDAV";
const MAX_CREDENTIAL_BYTES: usize = 1024;
const MAX_LIST_REQUESTS: usize = 10_000;
const MAX_LISTED_KEYS: usize = 2_000_000;
const MAX_LIST_DEPTH: usize = 16;
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;

pub(crate) struct WebDavBackend {
    http: HttpClient,
    /// `<base>/denote-sync/v1/`, always ending in `/`.
    root: Url,
    /// Percent-decoded path of `root`, used to relativize listing hrefs.
    root_path: Vec<u8>,
    username: String,
    password: Zeroizing<String>,
    /// Collections (relative to the base URL, ending in `/`) known to exist.
    collections: Mutex<HashSet<String>>,
}

impl WebDavBackend {
    pub(crate) fn new(url: &str, username: &str, password: &str) -> AppResult<Self> {
        Self::with_client(url, username, password, HttpClient::new(SERVICE)?)
    }

    fn with_client(url: &str, username: &str, password: &str, http: HttpClient) -> AppResult<Self> {
        let mut base = parse_service_url(url, SERVICE)?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        if username.len() > MAX_CREDENTIAL_BYTES
            || password.len() > MAX_CREDENTIAL_BYTES
            || username.contains(':')
            || username
                .chars()
                .chain(password.chars())
                .any(char::is_control)
        {
            return Err(AppError::InvalidData(
                "WebDAV username or password is invalid".to_string(),
            ));
        }
        let root = base
            .join(REMOTE_PREFIX)
            .map_err(|_| AppError::InvalidData("WebDAV URL is not a valid base URL".to_string()))?;
        let root_path = percent_decode(root.path());
        Ok(Self {
            http,
            root,
            root_path,
            username: username.to_string(),
            password: Zeroizing::new(password.to_string()),
            collections: Mutex::new(HashSet::new()),
        })
    }

    fn url(&self, relative: &str) -> AppResult<Url> {
        self.root
            .join(relative)
            .map_err(|_| AppError::InvalidData(format!("Invalid sync storage key: {relative}")))
    }

    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        let request = self.http.client().request(method, url);
        if self.username.is_empty() && self.password.is_empty() {
            request
        } else {
            request.basic_auth(&self.username, Some(self.password.as_str()))
        }
    }

    fn send(&self, request: RequestBuilder, limit: u64) -> AppResult<HttpResponse> {
        self.http.send(request, limit)
    }

    fn exists(&self, key: &str) -> AppResult<bool> {
        let response = self.send(self.request(Method::HEAD, self.url(key)?), 0)?;
        match response.status {
            status if status.is_success() => Ok(true),
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(false),
            _ => Err(self.http.status_error("check a stored item", &response)),
        }
    }

    fn put_once(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<HttpResponse> {
        let mut request = self
            .request(Method::PUT, self.url(key)?)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(bytes.to_vec());
        if mode == PutMode::CreateOnly {
            request = request.header(IF_NONE_MATCH, "*");
        }
        self.send(request, MAX_LISTING_BYTES)
    }

    fn collections(&self) -> AppResult<MutexGuard<'_, HashSet<String>>> {
        self.collections
            .lock()
            .map_err(|_| AppError::State("WebDAV collection cache lock is poisoned".to_string()))
    }

    /// Creates every missing collection from `denote-sync/` down to the
    /// parent of `key`. An existing collection answers `MKCOL` with 405.
    fn ensure_collections(&self, key: &str) -> AppResult<()> {
        let mut relative = String::new();
        let mut paths = Vec::new();
        for segment in REMOTE_PREFIX.trim_end_matches('/').split('/') {
            relative.push_str(segment);
            relative.push('/');
            paths.push(relative.clone());
        }
        let mut parents: Vec<&str> = key.split('/').collect();
        parents.pop();
        for segment in parents {
            relative.push_str(segment);
            relative.push('/');
            paths.push(relative.clone());
        }
        let base = self
            .root
            .join("../../")
            .map_err(|_| AppError::InvalidData("WebDAV URL is not a valid base URL".to_string()))?;
        for path in paths {
            if self.collections()?.contains(&path) {
                continue;
            }
            let url = base
                .join(&path)
                .map_err(|_| AppError::InvalidData(format!("Invalid sync storage key: {key}")))?;
            let response = self.send(self.request(mkcol(), url), MAX_LISTING_BYTES)?;
            match response.status {
                status if status.is_success() => {}
                StatusCode::METHOD_NOT_ALLOWED => {}
                _ => return Err(self.http.status_error("create a folder", &response)),
            }
            self.collections()?.insert(path);
        }
        Ok(())
    }

    fn propfind(&self, directory: &str) -> AppResult<Option<Vec<DavEntry>>> {
        let url = self.url(directory)?;
        let request = self
            .request(propfind(), url.clone())
            .header("Depth", "1")
            .header(CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(PROPFIND_BODY);
        let response = self.send(request, MAX_LISTING_BYTES)?;
        match response.status.as_u16() {
            207 => {}
            404 | 410 => return Ok(None),
            _ => return Err(self.http.status_error("list stored items", &response)),
        }
        let entries = parse_multistatus(&response.body)?;
        let mut resolved = Vec::with_capacity(entries.len());
        for (href, collection) in entries {
            let Ok(target) = url.join(&href) else {
                continue;
            };
            let path = percent_decode(target.path());
            let Some(relative) = path.strip_prefix(self.root_path.as_slice()) else {
                continue;
            };
            let Ok(relative) = String::from_utf8(relative.to_vec()) else {
                continue;
            };
            let collection = collection || relative.ends_with('/');
            let name = relative.trim_end_matches('/').to_string();
            if name == directory.trim_end_matches('/') {
                continue;
            }
            resolved.push(DavEntry { name, collection });
        }
        Ok(Some(resolved))
    }
}

struct DavEntry {
    /// Path relative to the remote prefix, without a trailing slash.
    name: String,
    collection: bool,
}

fn mkcol() -> Method {
    Method::from_bytes(b"MKCOL").unwrap_or(Method::PUT)
}

fn propfind() -> Method {
    Method::from_bytes(b"PROPFIND").unwrap_or(Method::GET)
}

impl Backend for WebDavBackend {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        validate_key(key)?;
        let response = self.send(self.request(Method::GET, self.url(key)?), MAX_OBJECT_BYTES)?;
        match response.status {
            StatusCode::OK => Ok(Some(response.body)),
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(None),
            _ => Err(self.http.status_error("download a stored item", &response)),
        }
    }

    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome> {
        validate_key(key)?;
        // Some servers ignore `If-None-Match`, so existence is checked first.
        if mode == PutMode::CreateOnly && self.exists(key)? {
            return Ok(PutOutcome::AlreadyExists);
        }
        let mut response = self.put_once(key, bytes, mode)?;
        if matches!(response.status.as_u16(), 404 | 409) {
            self.ensure_collections(key)?;
            response = self.put_once(key, bytes, mode)?;
        }
        match response.status {
            status if status.is_success() => Ok(PutOutcome::Written),
            StatusCode::PRECONDITION_FAILED if mode == PutMode::CreateOnly => {
                Ok(PutOutcome::AlreadyExists)
            }
            _ => Err(self.http.status_error("upload a stored item", &response)),
        }
    }

    fn list(&self, prefix: &str) -> AppResult<Vec<String>> {
        validate_prefix(prefix)?;
        let start = match prefix.rfind('/') {
            Some(index) => &prefix[..=index],
            None => "",
        };
        let mut keys = Vec::new();
        let mut pending = vec![start.to_string()];
        let mut requests = 0;
        while let Some(directory) = pending.pop() {
            requests += 1;
            if requests > MAX_LIST_REQUESTS {
                return Err(AppError::State(
                    "WebDAV listing exceeded the folder limit".to_string(),
                ));
            }
            let Some(entries) = self.propfind(&directory)? else {
                if directory == start {
                    continue;
                }
                return Err(AppError::State(
                    "A WebDAV folder disappeared while it was being listed. Try again.".to_string(),
                ));
            };
            for entry in entries {
                if entry.collection {
                    let child = format!("{}/", entry.name);
                    if validate_prefix(&child).is_ok()
                        && child.split('/').count() <= MAX_LIST_DEPTH
                        && (prefix.starts_with(&child) || child.starts_with(prefix))
                    {
                        pending.push(child);
                    }
                } else if entry.name.starts_with(prefix) && validate_key(&entry.name).is_ok() {
                    keys.push(entry.name);
                    if keys.len() > MAX_LISTED_KEYS {
                        return Err(AppError::State(
                            "WebDAV listing exceeded the item limit".to_string(),
                        ));
                    }
                }
            }
        }
        Ok(keys)
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        validate_key(key)?;
        let response = self.send(
            self.request(Method::DELETE, self.url(key)?),
            MAX_LISTING_BYTES,
        )?;
        match response.status {
            status if status.is_success() => Ok(()),
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(()),
            _ => Err(self.http.status_error("delete a stored item", &response)),
        }
    }
}

const MAX_MULTISTATUS_RESPONSES: usize = 500_000;

/// Returns `(href, is_collection)` for each `<response>`. Namespace prefixes
/// are ignored; only the local element names matter.
fn parse_multistatus(body: &[u8]) -> AppResult<Vec<(String, bool)>> {
    let invalid = || AppError::InvalidData("WebDAV returned an invalid listing".to_string());
    let text = std::str::from_utf8(body).map_err(|_| invalid())?;
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    let mut href = String::new();
    let mut response_collection = false;
    let mut propstat_collection = false;
    let mut propstat_status = String::new();
    let mut saw_multistatus = false;
    loop {
        let event = reader.read_event().map_err(|_| invalid())?;
        match event {
            Event::Start(element) => {
                let name = local_name(element.local_name().as_ref());
                if stack.is_empty() {
                    saw_multistatus |= name == "multistatus";
                }
                match name.as_str() {
                    "response" => {
                        href.clear();
                        response_collection = false;
                    }
                    "propstat" => {
                        propstat_collection = false;
                        propstat_status.clear();
                    }
                    "collection" if stack.iter().any(|item| item == "resourcetype") => {
                        propstat_collection = true;
                    }
                    _ => {}
                }
                stack.push(name);
                if stack.len() > 64 {
                    return Err(invalid());
                }
            }
            Event::Empty(element) => {
                if local_name(element.local_name().as_ref()) == "collection"
                    && stack.iter().any(|item| item == "resourcetype")
                {
                    propstat_collection = true;
                }
            }
            Event::End(_) => {
                let Some(name) = stack.pop() else {
                    return Err(invalid());
                };
                match name.as_str() {
                    "propstat" => {
                        let ok = propstat_status.is_empty()
                            || propstat_status.split_whitespace().nth(1) == Some("200");
                        if ok && propstat_collection {
                            response_collection = true;
                        }
                    }
                    "response" if !href.is_empty() => {
                        entries.push((std::mem::take(&mut href), response_collection));
                        if entries.len() > MAX_MULTISTATUS_RESPONSES {
                            return Err(invalid());
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(content) => append_text(
                &stack,
                &mut href,
                &mut propstat_status,
                &content.xml10_content(),
            ),
            Event::CData(content) => {
                append_text(&stack, &mut href, &mut propstat_status, content.as_ref());
            }
            Event::GeneralRef(reference) => {
                let resolved = match reference.resolve_char_ref() {
                    Ok(Some(character)) => character.to_string(),
                    Ok(None) => match reference.xml10_content().as_ref() {
                        "amp" => "&".to_string(),
                        "lt" => "<".to_string(),
                        "gt" => ">".to_string(),
                        "quot" => "\"".to_string(),
                        "apos" => "'".to_string(),
                        _ => return Err(invalid()),
                    },
                    Err(_) => return Err(invalid()),
                };
                append_text(&stack, &mut href, &mut propstat_status, &resolved);
            }
            Event::Eof if stack.is_empty() && saw_multistatus => break,
            Event::Eof => return Err(invalid()),
            _ => {}
        }
    }
    Ok(entries)
}

fn append_text(stack: &[String], href: &mut String, status: &mut String, text: &str) {
    match stack.last().map(String::as_str) {
        Some("href")
            if stack
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|parent| parent == "response") =>
        {
            href.push_str(text)
        }
        Some("status")
            if stack
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|parent| parent == "propstat") =>
        {
            status.push_str(text)
        }
        _ => {}
    }
}

fn local_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::*;
    use crate::sync::backends::test_support::{MockRequest, MockResponse, MockServer, conformance};

    const BASE: &str = "/remote.php/dav/files/user%20name/";

    #[derive(Default)]
    struct DavState {
        files: BTreeMap<String, Vec<u8>>,
        collections: std::collections::BTreeSet<String>,
        ignore_if_none_match: bool,
        requests: Vec<String>,
    }

    fn server(ignore_if_none_match: bool) -> (MockServer, Arc<Mutex<DavState>>) {
        let state = Arc::new(Mutex::new(DavState {
            ignore_if_none_match,
            ..DavState::default()
        }));
        state.lock().unwrap().collections.insert(String::new());
        let shared = Arc::clone(&state);
        let server = MockServer::start(move |request| handle(&mut shared.lock().unwrap(), request));
        (server, state)
    }

    fn handle(state: &mut DavState, request: &MockRequest) -> MockResponse {
        state
            .requests
            .push(format!("{} {}", request.method, request.path()));
        // "alice:s3cret" in Basic auth.
        if request.header("authorization") != Some("Basic YWxpY2U6czNjcmV0") {
            return MockResponse::status(401);
        }
        let Some(path) = request.path().strip_prefix(BASE) else {
            return MockResponse::status(404);
        };
        let path = String::from_utf8(percent_decode(path)).unwrap();
        let parent = |path: &str| -> String {
            let trimmed = path.trim_end_matches('/');
            match trimmed.rfind('/') {
                Some(index) => trimmed[..=index].to_string(),
                None => String::new(),
            }
        };
        match request.method.as_str() {
            "PUT" => {
                if !state.collections.contains(&parent(&path)) {
                    return MockResponse::status(409);
                }
                if request.header("if-none-match") == Some("*")
                    && !state.ignore_if_none_match
                    && state.files.contains_key(&path)
                {
                    return MockResponse::status(412);
                }
                let created = state.files.insert(path, request.body.clone()).is_none();
                MockResponse::status(if created { 201 } else { 204 })
            }
            "MKCOL" => {
                if state.collections.contains(&path) {
                    MockResponse::status(405)
                } else if !state.collections.contains(&parent(&path)) {
                    MockResponse::status(409)
                } else {
                    state.collections.insert(path);
                    MockResponse::status(201)
                }
            }
            "GET" | "HEAD" => match state.files.get(&path) {
                Some(bytes) if request.method == "GET" => MockResponse::body(200, bytes.clone()),
                Some(_) => MockResponse::status(200),
                None => MockResponse::status(404),
            },
            "DELETE" => match state.files.remove(&path) {
                Some(_) => MockResponse::status(204),
                None => MockResponse::status(404),
            },
            "PROPFIND" => {
                assert_eq!(request.header("depth"), Some("1"));
                if !state.collections.contains(&path) {
                    return MockResponse::status(404);
                }
                let mut body =
                    String::from(r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">"#);
                let entry = |href: String, collection: bool| {
                    let kind = if collection { "<D:collection/>" } else { "" };
                    format!(
                        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:resourcetype>{kind}</D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
                    )
                };
                body.push_str(&entry(format!("{BASE}{path}"), true));
                for collection in state
                    .collections
                    .iter()
                    .filter(|item| !item.is_empty() && parent(item) == path)
                {
                    // Absolute URLs and encoded characters must both be accepted.
                    body.push_str(&entry(
                        format!("http://127.0.0.1{BASE}{}", collection.replace('-', "%2D")),
                        true,
                    ));
                }
                for file in state.files.keys().filter(|item| parent(item) == path) {
                    body.push_str(&entry(
                        format!("{BASE}{}", file.replace('.', "&#46;")),
                        false,
                    ));
                }
                body.push_str("</D:multistatus>");
                MockResponse::body(207, body)
            }
            _ => MockResponse::status(405),
        }
    }

    fn backend(server: &MockServer, password: &str) -> WebDavBackend {
        WebDavBackend::with_client(
            &format!("{}{}", server.url(), BASE.trim_end_matches('/')),
            "alice",
            password,
            HttpClient::new(SERVICE)
                .unwrap()
                .with_retry_delay(Duration::from_millis(1)),
        )
        .unwrap()
    }

    #[test]
    fn conforms_with_native_conditional_put() {
        let (server, state) = server(false);
        conformance(&backend(&server, "s3cret"));
        let state = state.lock().unwrap();
        assert!(state.collections.contains("denote-sync/v1/objects/ab/"));
        assert!(state.files.contains_key("denote-sync/v1/repository.json"));
    }

    #[test]
    fn conforms_when_server_ignores_if_none_match() {
        let (server, _) = server(true);
        conformance(&backend(&server, "s3cret"));
    }

    #[test]
    fn creates_collections_only_once() {
        let (server, state) = server(false);
        let backend = backend(&server, "s3cret");
        backend.put("heads/aa/1", b"x", PutMode::Overwrite).unwrap();
        backend.put("heads/aa/2", b"x", PutMode::Overwrite).unwrap();
        let state = state.lock().unwrap();
        let mkcols = state
            .requests
            .iter()
            .filter(|line| line.starts_with("MKCOL"))
            .count();
        assert_eq!(mkcols, 4);
    }

    #[test]
    fn auth_failure_is_an_error_without_the_password() {
        let (server, _) = server(false);
        let backend = backend(&server, "wrong-password");
        for error in [
            backend.list("").unwrap_err(),
            backend.get("repository.json").unwrap_err(),
            backend
                .put("repository.json", b"x", PutMode::CreateOnly)
                .unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(message.contains("401"), "{message}");
            assert!(!message.contains("wrong-password"), "{message}");
        }
    }

    #[test]
    fn parses_multistatus_variants() {
        let body = br#"<?xml version="1.0"?>
            <multistatus xmlns="DAV:"><response><href>/a/b%20c/</href>
            <propstat><prop><resourcetype><collection/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat>
            </response><response><href><![CDATA[/a/x&y]]></href>
            <propstat><prop><resourcetype><collection/></resourcetype></prop><status>HTTP/1.1 404 Not Found</status></propstat>
            </response></multistatus>"#;
        assert_eq!(
            parse_multistatus(body).unwrap(),
            vec![
                ("/a/b%20c/".to_string(), true),
                ("/a/x&y".to_string(), false)
            ]
        );
        assert!(parse_multistatus(b"<multistatus><response>").is_err());
        assert!(parse_multistatus(b"<html><body>Login</body></html>").is_err());
        assert!(parse_multistatus(b"").is_err());
    }

    #[test]
    fn rejects_insecure_urls() {
        for url in [
            "http://dav.example.com/",
            "https://alice:pw@dav.example.com/",
        ] {
            assert!(WebDavBackend::new(url, "alice", "pw").is_err());
        }
        assert!(WebDavBackend::new("https://dav.example.com/", "a:b", "pw").is_err());
    }
}
