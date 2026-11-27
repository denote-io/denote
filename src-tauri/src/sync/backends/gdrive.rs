//! Google Drive transport over the Drive v3 REST API.
//!
//! Denote signs in with OAuth 2.0 for installed apps (PKCE S256, `state`, and
//! a one-shot loopback redirect on `127.0.0.1`) using the `drive.file` scope,
//! so it can only see files it created. Everything lives in one app folder
//! marked with `appProperties.denoteSyncFolder=v1`; each key is one file in
//! that folder whose `appProperties.denoteKey` is the key.
//!
//! Drive allows several files with the same name or properties. When a key
//! (or the app folder) has duplicates, every device picks the same one: the
//! oldest `createdTime`, then the lowest file ID. A `CreateOnly` upload that
//! loses that race deletes its own copy and reports `AlreadyExists`.

// Until the backend factory constructs the network transports, only tests
// reach them.

use std::{
    collections::HashMap,
    fmt,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{
    Method, StatusCode, Url,
    blocking::RequestBuilder,
    header::{CONTENT_TYPE, LOCATION},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{
    error::{AppError, AppResult},
    sync::backend::{Backend, PutMode, PutOutcome, validate_key, validate_prefix},
};

use super::http::{
    HttpClient, HttpResponse, MAX_LISTING_BYTES, MAX_OBJECT_BYTES, ensure_url_allowed,
    parse_service_url, percent_encode, status_error,
};

const SERVICE: &str = "Google Drive";
pub(crate) const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FOLDER_PROPERTY: &str = "denoteSyncFolder";
const KEY_PROPERTY: &str = "denoteKey";
/// Drive limits one property's key plus value to 124 bytes.
const MAX_PROPERTY_BYTES: usize = 124;
const RESUMABLE_THRESHOLD: usize = 5 * 1024 * 1024;
const MAX_LIST_PAGES: usize = 10_000;
const MAX_LISTED_FILES: usize = 2_000_000;
const MAX_FOLDER_NAME_CHARS: usize = 100;
const MAX_SECRET_BYTES: usize = 4096;
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_REDIRECT_REQUEST_BYTES: usize = 16 * 1024;
/// Unrelated loopback requests (favicons, stray local clients) tolerated
/// before the sign-in gives up.
const MAX_IGNORED_REDIRECTS: usize = 32;

/// OAuth and API endpoints, injectable for tests.
#[derive(Clone, Debug)]
pub(crate) struct GoogleEndpoints {
    pub(crate) authorize: String,
    pub(crate) token: String,
    /// Base for `/drive/v3/...`.
    pub(crate) api: String,
    /// Base for `/upload/drive/v3/...`.
    pub(crate) upload: String,
}

impl GoogleEndpoints {
    pub(crate) fn google() -> Self {
        Self {
            authorize: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            token: "https://oauth2.googleapis.com/token".to_string(),
            api: "https://www.googleapis.com".to_string(),
            upload: "https://www.googleapis.com".to_string(),
        }
    }
}

/// An OAuth client. Google treats installed-app client secrets as public, but
/// still requires the one issued with a desktop client in token requests.
#[derive(Clone)]
pub(crate) struct GoogleClient {
    pub(crate) id: String,
    pub(crate) secret: Option<String>,
}

impl fmt::Debug for GoogleClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleClient")
            .field("id", &self.id)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl GoogleClient {
    /// The user's client ID when given, otherwise the build-time default.
    /// The build-time secret is used only with the build-time client ID.
    pub(crate) fn resolve(client_id: Option<String>) -> AppResult<Self> {
        let default_id = option_env!("DENOTE_GOOGLE_OAUTH_CLIENT_ID").filter(|id| !id.is_empty());
        let default_secret =
            option_env!("DENOTE_GOOGLE_OAUTH_CLIENT_SECRET").filter(|secret| !secret.is_empty());
        let id = client_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .or_else(|| default_id.map(str::to_string))
            .ok_or_else(|| {
                AppError::InvalidData(
                    "This build of Denote has no Google OAuth client configured. Enter the client ID of your own Google Cloud desktop OAuth client in Sync settings."
                        .to_string(),
                )
            })?;
        let secret = default_secret
            .filter(|_| Some(id.as_str()) == default_id)
            .map(str::to_string);
        let client = Self { id, secret };
        client.validate()?;
        Ok(client)
    }

    fn validate(&self) -> AppResult<()> {
        let valid_id = !self.id.is_empty()
            && self.id.len() <= 256
            && self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        let valid_secret = self.secret.as_ref().is_none_or(|secret| {
            !secret.is_empty()
                && secret.len() <= 256
                && secret.bytes().all(|byte| byte.is_ascii_graphic())
        });
        if valid_id && valid_secret {
            Ok(())
        } else {
            Err(AppError::InvalidData(
                "Google OAuth client ID or secret is invalid".to_string(),
            ))
        }
    }

    fn form_fields(&self) -> Vec<(&'static str, &str)> {
        let mut fields = vec![("client_id", self.id.as_str())];
        if let Some(secret) = &self.secret {
            fields.push(("client_secret", secret.as_str()));
        }
        fields
    }
}

/// The result of a successful sign-in. The refresh token belongs in the
/// operating-system credential store only. `client_id` is the user's own
/// OAuth client, or `None` for the build-time default; pass it back to
/// [`GoogleClient::resolve`] when opening the backend.
pub(crate) struct GoogleConnection {
    pub(crate) account_label: String,
    pub(crate) refresh_token: String,
    pub(crate) client_id: Option<String>,
}

impl fmt::Debug for GoogleConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleConnection")
            .field("account_label", &self.account_label)
            .field("refresh_token", &"<redacted>")
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl Drop for GoogleConnection {
    fn drop(&mut self) {
        self.refresh_token.zeroize();
    }
}

/// Runs the installed-app OAuth flow: opens the consent page with
/// `open_url`, waits up to five minutes for the loopback redirect, and
/// exchanges the code for a refresh token. `client_id` overrides the
/// build-time OAuth client.
pub(crate) fn connect(
    client_id: Option<String>,
    open_url: &dyn Fn(&str) -> AppResult<()>,
) -> AppResult<GoogleConnection> {
    let override_id = client_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    let client = GoogleClient::resolve(override_id.clone())?;
    let (account_label, refresh_token) = connect_with(
        &GoogleEndpoints::google(),
        &HttpClient::new(SERVICE)?,
        &client,
        open_url,
        SIGN_IN_TIMEOUT,
    )?;
    Ok(GoogleConnection {
        account_label,
        refresh_token: refresh_token.as_str().to_string(),
        client_id: override_id,
    })
}

/// Returns the account label and the refresh token.
fn connect_with(
    endpoints: &GoogleEndpoints,
    http: &HttpClient,
    client: &GoogleClient,
    open_url: &dyn Fn(&str) -> AppResult<()>,
    timeout: Duration,
) -> AppResult<(String, Zeroizing<String>)> {
    client.validate()?;
    let verifier = Zeroizing::new(random_token(64)?);
    let state = random_token(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
        AppError::State(format!(
            "Unable to start the Google sign-in listener: {error}"
        ))
    })?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let mut authorize = parse_service_url(&endpoints.authorize, SERVICE)?;
    authorize
        .query_pairs_mut()
        .append_pair("client_id", &client.id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", DRIVE_SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    open_url(authorize.as_str())?;
    let code = Zeroizing::new(await_redirect(&listener, &state, Instant::now() + timeout)?);
    drop(listener);

    let mut fields = client.form_fields();
    fields.extend([
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
    ]);
    let token = token_request(http, endpoints, &fields)?;
    let refresh_token = Zeroizing::new(token.refresh_token.clone().ok_or_else(|| {
        AppError::State(
            "Google did not return a refresh token. Remove Denote's access in your Google account settings and connect again."
                .to_string(),
        )
    })?);
    let access_token = Zeroizing::new(token.access_token.clone());
    let about_url = api_url(
        &endpoints.api,
        "/drive/v3/about",
        &[("fields", "user(displayName,emailAddress)")],
    )?;
    let response = http.send(
        http.client()
            .get(about_url)
            .bearer_auth(access_token.as_str()),
        MAX_LISTING_BYTES,
    )?;
    if !response.status.is_success() {
        return Err(drive_error("read the signed-in account", &response));
    }
    let about: About = parse_json(&response)?;
    let account_label = about
        .user
        .and_then(|user| user.email_address.or(user.display_name))
        .filter(|label| !label.trim().is_empty())
        .map(|label| {
            label
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect()
        })
        .unwrap_or_else(|| "Google account".to_string());
    Ok((account_label, refresh_token))
}

fn random_token(bytes: usize) -> AppResult<String> {
    let mut buffer = Zeroizing::new(vec![0_u8; bytes]);
    getrandom::fill(&mut buffer)
        .map_err(|error| AppError::Crypto(format!("Unable to generate random data: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer.as_slice()))
}

/// Serves the loopback redirect until a request carries the expected
/// `state`. Requests for other paths or with a wrong `state` are answered and
/// ignored, so a stray local request cannot end the sign-in.
fn await_redirect(listener: &TcpListener, state: &str, deadline: Instant) -> AppResult<String> {
    listener.set_nonblocking(true)?;
    let mut ignored = 0;
    loop {
        match listener.accept() {
            Ok((stream, address)) => {
                if address.ip().is_loopback()
                    && let Some(result) = handle_redirect(stream, state)
                {
                    return result;
                }
                ignored += 1;
                if ignored > MAX_IGNORED_REDIRECTS {
                    return Err(AppError::State(
                        "Google sign-in received too many unexpected requests. Try connecting again."
                            .to_string(),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(AppError::State(
                        "Google sign-in timed out after five minutes. Try connecting again."
                            .to_string(),
                    ));
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(AppError::State(format!(
                    "Google sign-in listener failed: {error}"
                )));
            }
        }
    }
}

fn handle_redirect(stream: TcpStream, state: &str) -> Option<AppResult<String>> {
    stream.set_nonblocking(false).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    let mut total = 0;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).ok()?;
        total += read;
        if read == 0 || total > MAX_REDIRECT_REQUEST_BYTES {
            return None;
        }
        if request_line.is_empty() {
            request_line = line;
        } else if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        respond(stream, 405, "Unsupported request.");
        return None;
    };
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != "/" {
        respond(stream, 404, "Not found.");
        return None;
    }
    let parameter = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    let state_matches = parameter("state")
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(state.as_bytes())));
    if !state_matches {
        respond(
            stream,
            400,
            "This sign-in link is not the one Denote is waiting for.",
        );
        return None;
    }
    if let Some(error) = parameter("error") {
        respond(
            stream,
            200,
            "Google sign-in was cancelled. You can close this window.",
        );
        let error: String = error
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
            .take(64)
            .collect();
        return Some(Err(AppError::State(format!(
            "Google sign-in did not complete ({error})"
        ))));
    }
    match parameter("code").filter(|code| !code.is_empty() && code.len() <= 2048) {
        Some(code) => {
            respond(
                stream,
                200,
                "Google Drive is connected to Denote. You can close this window.",
            );
            Some(Ok(code))
        }
        None => {
            respond(stream, 400, "The sign-in response is missing its code.");
            None
        }
    }
}

fn respond(mut stream: TcpStream, status: u16, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Denote</title></head><body><p>{message}</p></body></html>"
    );
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct About {
    user: Option<AboutUser>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AboutUser {
    display_name: Option<String>,
    email_address: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct DriveFile {
    id: String,
    #[serde(default)]
    created_time: String,
    #[serde(default)]
    app_properties: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct DriveErrorBody {
    error: Option<DriveErrorDetail>,
}

#[derive(Deserialize)]
struct DriveErrorDetail {
    #[serde(default)]
    errors: Vec<DriveErrorReason>,
}

#[derive(Deserialize)]
struct DriveErrorReason {
    reason: Option<String>,
}

fn token_request(
    http: &HttpClient,
    endpoints: &GoogleEndpoints,
    fields: &[(&str, &str)],
) -> AppResult<TokenResponse> {
    let url = parse_service_url(&endpoints.token, SERVICE)?;
    let body: String = fields
        .iter()
        .map(|(name, value)| format!("{name}={}", percent_encode(value.as_bytes(), false)))
        .collect::<Vec<_>>()
        .join("&");
    let response = http.send(
        http.client()
            .post(url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body),
        MAX_LISTING_BYTES,
    )?;
    if response.status.is_success() {
        return parse_json(&response);
    }
    let (code, description) = serde_json::from_slice::<OAuthError>(&response.body)
        .map(|error| (error.error, error.error_description))
        .unwrap_or_default();
    if description
        .as_deref()
        .is_some_and(|text| text.contains("client_secret"))
    {
        return Err(AppError::State(
            "This Google OAuth client requires a client secret. Use a client ID that does not need one, or Denote's built-in client."
                .to_string(),
        ));
    }
    Err(match code.as_deref() {
        Some("invalid_grant") => AppError::State(
            "Google Drive access was revoked or has expired. Reconnect Google Drive in Sync settings."
                .to_string(),
        ),
        Some("invalid_client" | "unauthorized_client") => AppError::State(
            "Google rejected Denote's OAuth client. Check the client ID in Sync settings.".to_string(),
        ),
        code => status_error(SERVICE, "sign in", response.status, code),
    })
}

fn parse_json<T: for<'de> Deserialize<'de>>(response: &HttpResponse) -> AppResult<T> {
    serde_json::from_slice(&response.body)
        .map_err(|_| AppError::InvalidData("Google Drive returned an invalid response".to_string()))
}

fn drive_error(action: &str, response: &HttpResponse) -> AppError {
    let reason = serde_json::from_slice::<DriveErrorBody>(&response.body)
        .ok()
        .and_then(|body| body.error)
        .and_then(|error| error.errors.into_iter().find_map(|item| item.reason));
    match reason.as_deref() {
        Some("storageQuotaExceeded") => AppError::State(
            "Google Drive storage is full. Free up space and sync again.".to_string(),
        ),
        Some("rateLimitExceeded" | "userRateLimitExceeded") => AppError::State(
            "Google Drive is throttling requests. Sync will try again later.".to_string(),
        ),
        reason => status_error(SERVICE, action, response.status, reason),
    }
}

fn api_url(base: &str, path: &str, query: &[(&str, &str)]) -> AppResult<Url> {
    let mut url = parse_service_url(base, SERVICE)?;
    let joined = format!("{}{path}", url.path().trim_end_matches('/'));
    url.set_path(&joined);
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in query {
            pairs.append_pair(name, value);
        }
    }
    Ok(url)
}

/// Escapes a value for a single-quoted Drive query string.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Sorts candidates so the deterministic winner comes first.
fn order_candidates(files: &mut [DriveFile]) {
    files.sort_by(|left, right| {
        (left.created_time.as_str(), left.id.as_str())
            .cmp(&(right.created_time.as_str(), right.id.as_str()))
    });
}

struct AccessToken {
    token: Zeroizing<String>,
    expires_at: Instant,
}

pub(crate) struct GoogleDriveBackend {
    http: HttpClient,
    endpoints: GoogleEndpoints,
    client: GoogleClient,
    refresh_token: Zeroizing<String>,
    folder_name: String,
    resumable_threshold: usize,
    access: Mutex<Option<AccessToken>>,
    folder_id: Mutex<Option<String>>,
    /// Winning file ID per key, from the last lookup or listing.
    file_ids: Mutex<HashMap<String, String>>,
}

impl GoogleDriveBackend {
    /// Resolve `client_id` and `client_secret` with [`GoogleClient::resolve`]
    /// from the connection's stored client override.
    pub(crate) fn new(
        refresh_token: &str,
        client_id: &str,
        client_secret: Option<&str>,
        folder_name: &str,
    ) -> AppResult<Self> {
        Self::with_endpoints(
            refresh_token,
            GoogleClient {
                id: client_id.to_string(),
                secret: client_secret.map(str::to_string),
            },
            folder_name,
            GoogleEndpoints::google(),
            HttpClient::new(SERVICE)?,
        )
    }

    pub(crate) fn with_endpoints(
        refresh_token: &str,
        client: GoogleClient,
        folder_name: &str,
        endpoints: GoogleEndpoints,
        http: HttpClient,
    ) -> AppResult<Self> {
        client.validate()?;
        if refresh_token.is_empty()
            || refresh_token.len() > MAX_SECRET_BYTES
            || !refresh_token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(AppError::InvalidData(
                "Google Drive connection is missing or invalid. Reconnect Google Drive."
                    .to_string(),
            ));
        }
        let folder_name = folder_name.trim();
        if folder_name.is_empty()
            || folder_name.chars().count() > MAX_FOLDER_NAME_CHARS
            || folder_name.chars().any(char::is_control)
        {
            return Err(AppError::InvalidData(format!(
                "Google Drive folder name must be 1 to {MAX_FOLDER_NAME_CHARS} characters"
            )));
        }
        for base in [
            &endpoints.authorize,
            &endpoints.token,
            &endpoints.api,
            &endpoints.upload,
        ] {
            parse_service_url(base, SERVICE)?;
        }
        Ok(Self {
            http,
            endpoints,
            client,
            refresh_token: Zeroizing::new(refresh_token.to_string()),
            folder_name: folder_name.to_string(),
            resumable_threshold: RESUMABLE_THRESHOLD,
            access: Mutex::new(None),
            folder_id: Mutex::new(None),
            file_ids: Mutex::new(HashMap::new()),
        })
    }

    fn lock<'a, T>(mutex: &'a Mutex<T>) -> AppResult<MutexGuard<'a, T>> {
        mutex
            .lock()
            .map_err(|_| AppError::State("Google Drive state lock is poisoned".to_string()))
    }

    fn access_token(&self, force: bool) -> AppResult<Zeroizing<String>> {
        let mut access = Self::lock(&self.access)?;
        if !force
            && let Some(current) = access.as_ref()
            && current.expires_at > Instant::now() + TOKEN_EXPIRY_MARGIN
        {
            return Ok(current.token.clone());
        }
        let mut fields = self.client.form_fields();
        fields.extend([
            ("grant_type", "refresh_token"),
            ("refresh_token", self.refresh_token.as_str()),
        ]);
        let response = token_request(&self.http, &self.endpoints, &fields)?;
        let token = Zeroizing::new(response.access_token.clone());
        let lifetime = Duration::from_secs(response.expires_in.unwrap_or(3600).clamp(60, 86_400));
        *access = Some(AccessToken {
            token: token.clone(),
            expires_at: Instant::now() + lifetime,
        });
        Ok(token)
    }

    /// Sends an authorized request, refreshing the access token once if
    /// Google rejects it.
    fn api(
        &self,
        build: &dyn Fn(&str) -> AppResult<RequestBuilder>,
        limit: u64,
    ) -> AppResult<HttpResponse> {
        let token = self.access_token(false)?;
        let response = self.http.send(build(token.as_str())?, limit)?;
        if response.status != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let token = self.access_token(true)?;
        self.http.send(build(token.as_str())?, limit)
    }

    fn query_files(&self, query: &str, fields: &str) -> AppResult<Vec<DriveFile>> {
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut parameters = vec![
                ("q", query),
                ("fields", fields),
                ("pageSize", "1000"),
                ("spaces", "drive"),
            ];
            if let Some(token) = page_token.as_deref() {
                parameters.push(("pageToken", token));
            }
            let url = api_url(&self.endpoints.api, "/drive/v3/files", &parameters)?;
            let response = self.api(
                &|token| Ok(self.http.client().get(url.clone()).bearer_auth(token)),
                MAX_LISTING_BYTES,
            )?;
            if !response.status.is_success() {
                return Err(drive_error("list stored items", &response));
            }
            let page: FileList = parse_json(&response)?;
            files.extend(page.files);
            if files.len() > MAX_LISTED_FILES {
                return Err(AppError::State(
                    "Google Drive listing exceeded the item limit".to_string(),
                ));
            }
            match page.next_page_token.filter(|token| !token.is_empty()) {
                Some(next) if Some(&next) != page_token.as_ref() => page_token = Some(next),
                Some(_) => {
                    return Err(AppError::State(
                        "Google Drive returned a repeating page token".to_string(),
                    ));
                }
                None => return Ok(files),
            }
        }
        Err(AppError::State(
            "Google Drive listing exceeded the page limit".to_string(),
        ))
    }

    fn folder(&self) -> AppResult<String> {
        let mut cached = Self::lock(&self.folder_id)?;
        if let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        let query = format!(
            "mimeType = '{FOLDER_MIME}' and name = {} and appProperties has {{ key='{FOLDER_PROPERTY}' and value='v1' }} and trashed = false",
            quote(&self.folder_name)
        );
        let fields = "nextPageToken,files(id,createdTime)";
        let mut folders = self.query_files(&query, fields)?;
        if folders.is_empty() {
            let url = api_url(
                &self.endpoints.api,
                "/drive/v3/files",
                &[("fields", "id,createdTime")],
            )?;
            let metadata = json!({
                "name": self.folder_name,
                "mimeType": FOLDER_MIME,
                "appProperties": { FOLDER_PROPERTY: "v1" },
            })
            .to_string();
            let response = self.api(
                &|token| {
                    Ok(self
                        .http
                        .client()
                        .post(url.clone())
                        .bearer_auth(token)
                        .header(CONTENT_TYPE, "application/json; charset=UTF-8")
                        .body(metadata.clone()))
                },
                MAX_LISTING_BYTES,
            )?;
            if !response.status.is_success() {
                return Err(drive_error("create the sync folder", &response));
            }
            let created: DriveFile = parse_json(&response)?;
            validate_file_id(&created.id)?;
            // Another device may have created the folder at the same time.
            folders = self.query_files(&query, fields)?;
            if !folders.iter().any(|folder| folder.id == created.id) {
                folders.push(created.clone());
            }
            order_candidates(&mut folders);
            if folders
                .first()
                .is_some_and(|winner| winner.id != created.id)
            {
                // Best effort: an empty duplicate folder is harmless.
                let _ = self.delete_file(&created.id);
            }
        }
        order_candidates(&mut folders);
        let Some(id) = folders.first().map(|folder| folder.id.clone()) else {
            return Err(AppError::State(
                "Google Drive did not return the sync folder".to_string(),
            ));
        };
        validate_file_id(&id)?;
        *cached = Some(id.clone());
        Ok(id)
    }

    /// Every file for `key`, winner first.
    fn candidates(&self, key: &str) -> AppResult<Vec<DriveFile>> {
        let query = format!(
            "{} in parents and appProperties has {{ key='{KEY_PROPERTY}' and value={} }} and trashed = false",
            quote(&self.folder()?),
            quote(key)
        );
        let mut files = self.query_files(&query, "nextPageToken,files(id,createdTime)")?;
        files.retain(|file| validate_file_id(&file.id).is_ok());
        order_candidates(&mut files);
        let mut ids = Self::lock(&self.file_ids)?;
        match files.first() {
            Some(winner) => ids.insert(key.to_string(), winner.id.clone()),
            None => ids.remove(key),
        };
        Ok(files)
    }

    fn cached_id(&self, key: &str) -> AppResult<Option<String>> {
        Ok(Self::lock(&self.file_ids)?.get(key).cloned())
    }

    fn forget(&self, key: &str) -> AppResult<()> {
        Self::lock(&self.file_ids)?.remove(key);
        Ok(())
    }

    fn download(&self, id: &str) -> AppResult<Option<Vec<u8>>> {
        let url = api_url(
            &self.endpoints.api,
            &format!("/drive/v3/files/{id}"),
            &[("alt", "media")],
        )?;
        let response = self.api(
            &|token| Ok(self.http.client().get(url.clone()).bearer_auth(token)),
            MAX_OBJECT_BYTES,
        )?;
        match response.status {
            StatusCode::OK => Ok(Some(response.body)),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(drive_error("download a stored item", &response)),
        }
    }

    fn delete_file(&self, id: &str) -> AppResult<()> {
        let url = api_url(&self.endpoints.api, &format!("/drive/v3/files/{id}"), &[])?;
        let response = self.api(
            &|token| Ok(self.http.client().delete(url.clone()).bearer_auth(token)),
            MAX_LISTING_BYTES,
        )?;
        match response.status {
            status if status.is_success() => Ok(()),
            StatusCode::NOT_FOUND => Ok(()),
            _ => Err(drive_error("delete a stored item", &response)),
        }
    }

    /// Uploads content to a new file (`existing` is `None`) or replaces the
    /// content of an existing one. Drive commits an upload atomically.
    fn upload(&self, key: &str, bytes: &[u8], existing: Option<&str>) -> AppResult<DriveFile> {
        let (method, path) = match existing {
            Some(id) => (Method::PATCH, format!("/upload/drive/v3/files/{id}")),
            None => (Method::POST, "/upload/drive/v3/files".to_string()),
        };
        let metadata = match existing {
            Some(_) => json!({}),
            None => json!({
                "name": key,
                "parents": [self.folder()?],
                "mimeType": "application/octet-stream",
                "appProperties": { KEY_PROPERTY: key },
            }),
        }
        .to_string();
        let response = if bytes.len() > self.resumable_threshold {
            self.resumable_upload(method, &path, &metadata, bytes)?
        } else {
            let url = api_url(
                &self.endpoints.upload,
                &path,
                &[("uploadType", "multipart"), ("fields", "id,createdTime")],
            )?;
            let boundary = format!("denote-{}", uuid::Uuid::new_v4().simple());
            let mut body = Vec::with_capacity(bytes.len() + metadata.len() + 256);
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            self.api(
                &|token| {
                    Ok(self
                        .http
                        .client()
                        .request(method.clone(), url.clone())
                        .bearer_auth(token)
                        .header(
                            CONTENT_TYPE,
                            format!("multipart/related; boundary={boundary}"),
                        )
                        .body(body.clone()))
                },
                MAX_LISTING_BYTES,
            )?
        };
        if !response.status.is_success() {
            return Err(drive_error("upload a stored item", &response));
        }
        let file: DriveFile = parse_json(&response)?;
        validate_file_id(&file.id)?;
        Ok(file)
    }

    fn resumable_upload(
        &self,
        method: Method,
        path: &str,
        metadata: &str,
        bytes: &[u8],
    ) -> AppResult<HttpResponse> {
        let url = api_url(
            &self.endpoints.upload,
            path,
            &[("uploadType", "resumable"), ("fields", "id,createdTime")],
        )?;
        let length = bytes.len().to_string();
        let response = self.api(
            &|token| {
                Ok(self
                    .http
                    .client()
                    .request(method.clone(), url.clone())
                    .bearer_auth(token)
                    .header(CONTENT_TYPE, "application/json; charset=UTF-8")
                    .header("X-Upload-Content-Type", "application/octet-stream")
                    .header("X-Upload-Content-Length", length.as_str())
                    .body(metadata.to_string()))
            },
            MAX_LISTING_BYTES,
        )?;
        if !response.status.is_success() {
            return Err(drive_error("start an upload", &response));
        }
        let session = response
            .header(LOCATION.as_str())
            .and_then(|location| url.join(location).ok())
            .ok_or_else(|| {
                AppError::InvalidData("Google Drive did not return an upload session".to_string())
            })?;
        ensure_url_allowed(&session, SERVICE)?;
        if session.host_str() != url.host_str()
            || session.port_or_known_default() != url.port_or_known_default()
        {
            return Err(AppError::InvalidData(
                "Google Drive returned an upload session on an unexpected host".to_string(),
            ));
        }
        let body = bytes.to_vec();
        self.api(
            &|token| {
                Ok(self
                    .http
                    .client()
                    .put(session.clone())
                    .bearer_auth(token)
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .body(body.clone()))
            },
            MAX_LISTING_BYTES,
        )
    }
}

fn validate_file_id(id: &str) -> AppResult<()> {
    if !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(AppError::InvalidData(
            "Google Drive returned an invalid file ID".to_string(),
        ))
    }
}

fn validate_drive_key(key: &str) -> AppResult<()> {
    validate_key(key)?;
    if KEY_PROPERTY.len() + key.len() > MAX_PROPERTY_BYTES {
        return Err(AppError::InvalidData(format!(
            "Sync storage key is too long for Google Drive: {key}"
        )));
    }
    Ok(())
}

impl Backend for GoogleDriveBackend {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        validate_drive_key(key)?;
        if let Some(id) = self.cached_id(key)? {
            if let Some(bytes) = self.download(&id)? {
                return Ok(Some(bytes));
            }
            self.forget(key)?;
        }
        match self.candidates(key)?.first() {
            Some(winner) => self.download(&winner.id),
            None => Ok(None),
        }
    }

    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome> {
        validate_drive_key(key)?;
        let existing = self.candidates(key)?;
        match (mode, existing.first()) {
            (PutMode::CreateOnly, Some(_)) => Ok(PutOutcome::AlreadyExists),
            (PutMode::Overwrite, Some(winner)) => {
                self.upload(key, bytes, Some(&winner.id))?;
                Ok(PutOutcome::Written)
            }
            (mode, None) => {
                let created = self.upload(key, bytes, None)?;
                if mode == PutMode::Overwrite {
                    Self::lock(&self.file_ids)?.insert(key.to_string(), created.id);
                    return Ok(PutOutcome::Written);
                }
                let winner = self.candidates(key)?.into_iter().next();
                match winner {
                    Some(winner) if winner.id != created.id => {
                        self.delete_file(&created.id)?;
                        Ok(PutOutcome::AlreadyExists)
                    }
                    _ => Ok(PutOutcome::Written),
                }
            }
        }
    }

    fn list(&self, prefix: &str) -> AppResult<Vec<String>> {
        validate_prefix(prefix)?;
        let query = format!("{} in parents and trashed = false", quote(&self.folder()?));
        let mut files =
            self.query_files(&query, "nextPageToken,files(id,createdTime,appProperties)")?;
        order_candidates(&mut files);
        let mut winners: HashMap<String, String> = HashMap::new();
        for file in files {
            let Some(key) = file.app_properties.get(KEY_PROPERTY) else {
                continue;
            };
            if !key.starts_with(prefix)
                || validate_key(key).is_err()
                || validate_file_id(&file.id).is_err()
            {
                continue;
            }
            winners.entry(key.clone()).or_insert(file.id);
        }
        let mut ids = Self::lock(&self.file_ids)?;
        ids.retain(|key, _| !key.starts_with(prefix));
        ids.extend(winners.iter().map(|(key, id)| (key.clone(), id.clone())));
        Ok(winners.into_keys().collect())
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        validate_drive_key(key)?;
        for file in self.candidates(key)? {
            self.delete_file(&file.id)?;
        }
        self.forget(key)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Read,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::sync::backends::test_support::{MockRequest, MockResponse, MockServer, conformance};

    #[derive(Default)]
    struct DriveState {
        files: Vec<StoredFile>,
        clock: u64,
        next_id: u64,
        access_generation: u64,
        reject_next_access: bool,
        sessions: HashMap<String, (String, Option<String>)>,
        challenge: Option<String>,
        resumable_uploads: usize,
        list_pages: usize,
        base: String,
        /// Another device creates the app folder just before this one.
        race_folder: bool,
    }

    #[derive(Clone)]
    struct StoredFile {
        id: String,
        created: String,
        name: String,
        parents: Vec<String>,
        folder: bool,
        properties: HashMap<String, String>,
        content: Vec<u8>,
    }

    impl DriveState {
        fn create(&mut self, metadata: &serde_json::Value, content: Vec<u8>) -> StoredFile {
            self.clock += 1;
            self.next_id += 1;
            let file = StoredFile {
                id: format!("id{:04}", 1000 - self.next_id),
                created: format!("2026-01-01T00:00:{:02}.000Z", self.clock),
                name: metadata["name"].as_str().unwrap_or_default().to_string(),
                parents: metadata["parents"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().unwrap().to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                folder: metadata["mimeType"] == FOLDER_MIME,
                properties: metadata["appProperties"]
                    .as_object()
                    .map(|items| {
                        items
                            .iter()
                            .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                content,
            };
            self.files.push(file.clone());
            file
        }
    }

    fn file_json(file: &StoredFile) -> serde_json::Value {
        json!({ "id": file.id, "createdTime": file.created, "appProperties": file.properties })
    }

    fn quoted_after<'a>(query: &'a str, marker: &str) -> Option<&'a str> {
        let rest = &query[query.find(marker)? + marker.len()..];
        let rest = rest.strip_prefix('\'')?;
        Some(&rest[..rest.find('\'')?])
    }

    fn split_multipart(request: &MockRequest) -> (serde_json::Value, Vec<u8>) {
        let content_type = request.header("content-type").unwrap();
        let boundary = content_type.split("boundary=").nth(1).unwrap();
        let body = &request.body;
        let separator = format!("--{boundary}");
        let text_end = body
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let json_end = text_end
            + body[text_end..]
                .windows(separator.len())
                .position(|window| window == separator.as_bytes())
                .unwrap();
        let metadata: serde_json::Value =
            serde_json::from_slice(&body[text_end..json_end - 2]).unwrap();
        let content_start = json_end
            + body[json_end..]
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
            + 4;
        let closing = format!("\r\n--{boundary}--\r\n");
        let content = body[content_start..body.len() - closing.len()].to_vec();
        (metadata, content)
    }

    fn handle(state: &mut DriveState, request: &MockRequest) -> MockResponse {
        let path = request.path().to_string();
        if path == "/token" {
            let form = String::from_utf8(request.body.clone()).unwrap();
            let field = |name: &str| {
                form.split('&').find_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    (key == name).then(|| {
                        String::from_utf8(super::super::http::percent_decode(value)).unwrap()
                    })
                })
            };
            assert_eq!(
                field("client_id").as_deref(),
                Some("client-id.apps.example")
            );
            assert_eq!(
                field("client_secret").as_deref(),
                Some("public-desktop-secret")
            );
            return match field("grant_type").as_deref() {
                Some("authorization_code") => {
                    let verifier = field("code_verifier").unwrap();
                    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                    if field("code").as_deref() != Some("auth-code")
                        || Some(challenge) != state.challenge
                    {
                        return MockResponse::body(400, r#"{"error":"invalid_grant"}"#);
                    }
                    MockResponse::body(
                        200,
                        r#"{"access_token":"access-0","expires_in":3600,"refresh_token":"refresh-synthetic"}"#,
                    )
                }
                Some("refresh_token")
                    if field("refresh_token").as_deref() == Some("refresh-synthetic") =>
                {
                    state.access_generation += 1;
                    MockResponse::body(
                        200,
                        format!(
                            r#"{{"access_token":"access-{}","expires_in":3600}}"#,
                            state.access_generation
                        ),
                    )
                }
                _ => MockResponse::body(400, r#"{"error":"invalid_grant"}"#),
            };
        }
        let expected = format!("Bearer access-{}", state.access_generation);
        if request.header("authorization") != Some(expected.as_str()) || state.reject_next_access {
            state.reject_next_access = false;
            return MockResponse::body(401, r#"{"error":{"errors":[{"reason":"authError"}]}}"#);
        }
        match (request.method.as_str(), path.as_str()) {
            ("GET", "/drive/v3/about") => MockResponse::body(
                200,
                r#"{"user":{"displayName":"Synthetic User","emailAddress":"user@example.test"}}"#,
            ),
            ("GET", "/drive/v3/files") => {
                state.list_pages += 1;
                let query = request.query("q").unwrap();
                let mut matching: Vec<&StoredFile> = state
                    .files
                    .iter()
                    .filter(|file| {
                        if query.contains(FOLDER_MIME) {
                            return file.folder
                                && Some(file.name.as_str()) == quoted_after(&query, "name = ")
                                && file.properties.get(FOLDER_PROPERTY).map(String::as_str)
                                    == Some("v1");
                        }
                        let parent = quoted_after(&query, "").unwrap();
                        if !file.parents.iter().any(|item| item == parent) {
                            return false;
                        }
                        match quoted_after(&query, "and value=") {
                            Some(key) => {
                                file.properties.get(KEY_PROPERTY).map(String::as_str) == Some(key)
                            }
                            None => true,
                        }
                    })
                    .collect();
                matching.sort_by(|a, b| a.id.cmp(&b.id));
                let start: usize = request
                    .query("pageToken")
                    .map(|token| token.parse().unwrap())
                    .unwrap_or(0);
                let end = (start + 2).min(matching.len());
                let files: Vec<serde_json::Value> = matching[start..end]
                    .iter()
                    .map(|file| file_json(file))
                    .collect();
                let mut body = json!({ "files": files });
                if end < matching.len() {
                    body["nextPageToken"] = json!(end.to_string());
                }
                MockResponse::body(200, body.to_string())
            }
            ("POST", "/drive/v3/files") => {
                let metadata: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if std::mem::take(&mut state.race_folder) {
                    state.create(&metadata, Vec::new());
                }
                let file = state.create(&metadata, Vec::new());
                MockResponse::body(200, file_json(&file).to_string())
            }
            ("POST", "/upload/drive/v3/files") | ("PATCH", _)
                if request.query("uploadType").as_deref() == Some("resumable") =>
            {
                state.next_id += 1;
                let session = format!("s{}", state.next_id);
                let existing = path
                    .strip_prefix("/upload/drive/v3/files/")
                    .map(str::to_string);
                let metadata = String::from_utf8(request.body.clone()).unwrap();
                state.sessions.insert(session.clone(), (metadata, existing));
                MockResponse::status(200).with_header(
                    "Location",
                    &format!("{}/upload/session/{session}?upload_id=x", state.base),
                )
            }
            ("PUT", session) if session.starts_with("/upload/session/") => {
                state.resumable_uploads += 1;
                let (metadata, existing) = state
                    .sessions
                    .remove(session.trim_start_matches("/upload/session/"))
                    .unwrap();
                let file = match existing {
                    Some(id) => {
                        let file = state.files.iter_mut().find(|file| file.id == id).unwrap();
                        file.content = request.body.clone();
                        file.clone()
                    }
                    None => state.create(
                        &serde_json::from_str(&metadata).unwrap(),
                        request.body.clone(),
                    ),
                };
                MockResponse::body(200, file_json(&file).to_string())
            }
            ("POST", "/upload/drive/v3/files") => {
                assert_eq!(request.query("uploadType").as_deref(), Some("multipart"));
                let (metadata, content) = split_multipart(request);
                let file = state.create(&metadata, content);
                MockResponse::body(200, file_json(&file).to_string())
            }
            ("PATCH", upload) if upload.starts_with("/upload/drive/v3/files/") => {
                let id = upload.trim_start_matches("/upload/drive/v3/files/");
                let (_, content) = split_multipart(request);
                match state.files.iter_mut().find(|file| file.id == id) {
                    Some(file) => {
                        file.content = content;
                        MockResponse::body(200, file_json(file).to_string())
                    }
                    None => MockResponse::status(404),
                }
            }
            (method, file) if file.starts_with("/drive/v3/files/") => {
                let id = file.trim_start_matches("/drive/v3/files/");
                let index = state.files.iter().position(|file| file.id == id);
                match (method, index) {
                    ("GET", Some(index)) => {
                        assert_eq!(request.query("alt").as_deref(), Some("media"));
                        MockResponse::body(200, state.files[index].content.clone())
                    }
                    ("DELETE", Some(index)) => {
                        state.files.remove(index);
                        MockResponse::status(204)
                    }
                    _ => MockResponse::body(404, r#"{"error":{"errors":[{"reason":"notFound"}]}}"#),
                }
            }
            _ => MockResponse::status(400),
        }
    }

    fn server() -> (MockServer, Arc<Mutex<DriveState>>, GoogleEndpoints) {
        let state = Arc::new(Mutex::new(DriveState::default()));
        let shared = Arc::clone(&state);
        let server = MockServer::start(move |request| handle(&mut shared.lock().unwrap(), request));
        state.lock().unwrap().base = server.url();
        let endpoints = GoogleEndpoints {
            authorize: format!("{}/authorize", server.url()),
            token: format!("{}/token", server.url()),
            api: server.url(),
            upload: server.url(),
        };
        (server, state, endpoints)
    }

    fn client() -> GoogleClient {
        GoogleClient {
            id: "client-id.apps.example".to_string(),
            secret: Some("public-desktop-secret".to_string()),
        }
    }

    fn test_http() -> HttpClient {
        HttpClient::new(SERVICE)
            .unwrap()
            .with_retry_delay(Duration::from_millis(1))
    }

    fn backend(endpoints: &GoogleEndpoints) -> GoogleDriveBackend {
        let mut backend = GoogleDriveBackend::with_endpoints(
            "refresh-synthetic",
            client(),
            "Denote Sync",
            endpoints.clone(),
            test_http(),
        )
        .unwrap();
        backend.resumable_threshold = 16;
        backend
    }

    #[test]
    fn conforms_with_pagination_and_resumable_uploads() {
        let (_server, state, endpoints) = server();
        conformance(&backend(&endpoints));
        let state = state.lock().unwrap();
        assert_eq!(state.files.iter().filter(|file| file.folder).count(), 1);
        assert!(state.resumable_uploads > 0);
    }

    #[test]
    fn duplicates_resolve_to_the_oldest_file() {
        let (_server, state, endpoints) = server();
        let backend = backend(&endpoints);
        backend
            .put("heads/aa/1", b"older", PutMode::CreateOnly)
            .unwrap();
        {
            let mut state = state.lock().unwrap();
            let original = state
                .files
                .iter()
                .find(|file| !file.folder)
                .unwrap()
                .clone();
            let mut newer = original.clone();
            newer.id = "id0000".to_string();
            newer.created = "2026-12-31T00:00:00.000Z".to_string();
            newer.content = b"newer".to_vec();
            state.files.push(newer);
        }
        let fresh = super::tests::backend(&endpoints);
        assert_eq!(
            fresh.get("heads/aa/1").unwrap().as_deref(),
            Some(&b"older"[..])
        );
        assert_eq!(
            fresh.list("heads/").unwrap(),
            vec!["heads/aa/1".to_string()]
        );
        assert_eq!(
            fresh.put("heads/aa/1", b"x", PutMode::CreateOnly).unwrap(),
            PutOutcome::AlreadyExists
        );
        fresh.delete("heads/aa/1").unwrap();
        assert!(state.lock().unwrap().files.iter().all(|file| file.folder));
    }

    #[test]
    fn concurrent_folder_creation_converges_on_the_oldest_folder() {
        let (_server, state, endpoints) = server();
        state.lock().unwrap().race_folder = true;
        let backend = backend(&endpoints);
        backend
            .put("repository.json", b"x", PutMode::CreateOnly)
            .unwrap();
        let state = state.lock().unwrap();
        let folders: Vec<&StoredFile> = state.files.iter().filter(|file| file.folder).collect();
        assert_eq!(folders.len(), 1);
        let stored = state.files.iter().find(|file| !file.folder).unwrap();
        assert_eq!(stored.parents, vec![folders[0].id.clone()]);
        assert_eq!(folders[0].created, "2026-01-01T00:00:01.000Z");
    }

    #[test]
    fn refreshes_a_rejected_access_token() {
        let (_server, state, endpoints) = server();
        let backend = backend(&endpoints);
        backend
            .put("repository.json", b"x", PutMode::Overwrite)
            .unwrap();
        state.lock().unwrap().reject_next_access = true;
        assert_eq!(
            backend.get("repository.json").unwrap().as_deref(),
            Some(&b"x"[..])
        );
        assert!(state.lock().unwrap().access_generation >= 2);
    }

    #[test]
    fn revoked_refresh_token_is_actionable() {
        let (_server, _state, endpoints) = server();
        let backend = GoogleDriveBackend::with_endpoints(
            "revoked-token",
            client(),
            "Denote Sync",
            endpoints,
            test_http(),
        )
        .unwrap();
        let message = backend.list("").unwrap_err().to_string();
        assert!(message.contains("Reconnect Google Drive"), "{message}");
        assert!(!message.contains("revoked-token"));
    }

    fn hit(url: &str) -> String {
        let url = Url::parse(url).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", url.port().unwrap())).unwrap();
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn connects_with_pkce_loopback_flow() {
        let (_server, state, endpoints) = server();
        let opened = |url: &str| -> AppResult<()> {
            let url = Url::parse(url).unwrap();
            let parameter = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .unwrap()
                    .1
                    .into_owned()
            };
            assert_eq!(parameter("scope"), DRIVE_SCOPE);
            assert_eq!(parameter("code_challenge_method"), "S256");
            assert_eq!(parameter("response_type"), "code");
            state.lock().unwrap().challenge = Some(parameter("code_challenge"));
            let redirect = parameter("redirect_uri");
            let state_value = parameter("state");
            thread::spawn(move || {
                assert!(hit(&format!("{redirect}/favicon.ico")).starts_with("HTTP/1.1 404"));
                assert!(
                    hit(&format!("{redirect}/?code=forged&state=wrong"))
                        .starts_with("HTTP/1.1 400")
                );
                assert!(
                    hit(&format!("{redirect}/?code=auth-code&state={state_value}"))
                        .starts_with("HTTP/1.1 200")
                );
            });
            Ok(())
        };
        let (account_label, refresh_token) = connect_with(
            &endpoints,
            &test_http(),
            &client(),
            &opened,
            Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(refresh_token.as_str(), "refresh-synthetic");
        assert_eq!(account_label, "user@example.test");
        let connection = GoogleConnection {
            account_label,
            refresh_token: refresh_token.to_string(),
            client_id: None,
        };
        assert!(!format!("{connection:?}").contains("refresh-synthetic"));
    }

    #[test]
    fn sign_in_reports_denial_and_timeout() {
        let (_server, _state, endpoints) = server();
        let denied = |url: &str| -> AppResult<()> {
            let url = Url::parse(url).unwrap();
            let parameter = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .unwrap()
                    .1
                    .into_owned()
            };
            let target = format!(
                "{}/?error=access_denied&state={}",
                parameter("redirect_uri"),
                parameter("state")
            );
            thread::spawn(move || hit(&target));
            Ok(())
        };
        let error = connect_with(
            &endpoints,
            &test_http(),
            &client(),
            &denied,
            Duration::from_secs(20),
        )
        .unwrap_err();
        assert!(error.to_string().contains("access_denied"));
        let ignored = |_: &str| -> AppResult<()> { Ok(()) };
        let error = connect_with(
            &endpoints,
            &test_http(),
            &client(),
            &ignored,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn resolves_client_configuration() {
        let explicit = GoogleClient::resolve(Some(" my-client.apps.example ".to_string())).unwrap();
        assert_eq!(explicit.id, "my-client.apps.example");
        if option_env!("DENOTE_GOOGLE_OAUTH_CLIENT_ID").is_none() {
            assert!(
                GoogleClient::resolve(None)
                    .unwrap_err()
                    .to_string()
                    .contains("client ID")
            );
            assert!(explicit.secret.is_none());
        }
        assert!(GoogleClient::resolve(Some("bad id'".to_string())).is_err());
        assert_eq!(quote("a'b\\c"), r"'a\'b\\c'");
    }
}
