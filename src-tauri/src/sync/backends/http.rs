//! Shared blocking HTTP client for the network sync transports.
//!
//! Every request goes through [`HttpClient::send`], which enforces the URL
//! policy, bounds the response size, and retries throttling and transient
//! server errors with backoff. Errors never carry credentials: callers pass
//! secrets only in headers, URLs with embedded credentials are refused, and
//! transport errors are stripped of their URL.

// Until the backend factory constructs the network transports, only tests
// reach them.
#![cfg_attr(not(test), allow(dead_code))]

use std::{io::Read, net::IpAddr, thread, time::Duration};

use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, RequestBuilder},
    header::{HeaderMap, RETRY_AFTER},
    redirect::Policy,
    tls,
};

use crate::error::{AppError, AppResult};

/// Largest single object a transport downloads. Content objects are at most
/// 256 MiB of plaintext, so this leaves room for the envelope.
pub(crate) const MAX_OBJECT_BYTES: u64 = 300 * 1024 * 1024;
/// Largest listing, metadata, or token response.
pub(crate) const MAX_LISTING_BYTES: u64 = 16 * 1024 * 1024;
/// Largest error body read for a diagnostic.
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;
const MAX_URL_BYTES: usize = 2048;
const MAX_ATTEMPTS: u32 = 4;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Whole-request ceiling, sized for a 300 MiB object on a slow link.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub(crate) struct HttpResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl HttpResponse {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

#[derive(Clone)]
pub(crate) struct HttpClient {
    client: Client,
    service: &'static str,
    retry_delay: Duration,
}

impl HttpClient {
    /// `service` names the storage in user-facing errors, e.g. "WebDAV".
    pub(crate) fn new(service: &'static str) -> AppResult<Self> {
        let client = Client::builder()
            .user_agent(concat!("Denote/", env!("CARGO_PKG_VERSION"), " sync"))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // Redirects would carry signed or authenticated requests to a URL
            // the user never configured, so they are reported instead.
            .redirect(Policy::none())
            .min_tls_version(tls::Version::TLS_1_2)
            .referer(false)
            .build()
            .map_err(|error| {
                AppError::State(format!(
                    "Unable to create the {service} HTTP client: {}",
                    error.without_url()
                ))
            })?;
        Ok(Self {
            client,
            service,
            retry_delay: DEFAULT_RETRY_DELAY,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        self
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    /// Sends `request`, retrying 408, 429, and 5xx responses and connection
    /// failures, and reads at most `limit` bytes of a successful body. Any
    /// final status is returned to the caller, which owns its meaning. A
    /// request that times out after connecting is not retried, so one slow
    /// transfer cannot hold a sync cycle for several request timeouts.
    pub(crate) fn send(&self, request: RequestBuilder, limit: u64) -> AppResult<HttpResponse> {
        let probe = request.try_clone().ok_or_else(|| {
            AppError::State(format!("{} request body cannot be retried", self.service))
        })?;
        let probe = probe.build().map_err(|error| self.transport_error(error))?;
        ensure_url_allowed(probe.url(), self.service)?;
        // A HEAD response advertises the size of a body it never sends.
        let is_head = probe.method() == Method::HEAD;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let current = request.try_clone().ok_or_else(|| {
                AppError::State(format!("{} request body cannot be retried", self.service))
            })?;
            match current.send() {
                Ok(response) => {
                    let status = response.status();
                    if is_retryable(status) && attempt < MAX_ATTEMPTS {
                        let delay = retry_after(response.headers())
                            .unwrap_or_else(|| self.backoff(attempt));
                        drop(response);
                        thread::sleep(delay);
                        continue;
                    }
                    let headers = response.headers().clone();
                    let limit = if status.is_success() {
                        limit
                    } else {
                        limit.min(MAX_ERROR_BODY_BYTES)
                    };
                    if status.is_success()
                        && !is_head
                        && response
                            .content_length()
                            .is_some_and(|length| length > limit)
                    {
                        return Err(self.too_large(limit));
                    }
                    let mut body = Vec::new();
                    response
                        .take(limit + 1)
                        .read_to_end(&mut body)
                        .map_err(|error| {
                            AppError::State(format!(
                                "Unable to read the {} response: {error}",
                                self.service
                            ))
                        })?;
                    if body.len() as u64 > limit {
                        if status.is_success() {
                            return Err(self.too_large(limit));
                        }
                        body.truncate(limit as usize);
                    }
                    return Ok(HttpResponse {
                        status,
                        headers,
                        body,
                    });
                }
                Err(error) if error.is_connect() && attempt < MAX_ATTEMPTS => {
                    thread::sleep(self.backoff(attempt));
                }
                Err(error) => return Err(self.transport_error(error)),
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        self.retry_delay
            .saturating_mul(1 << attempt.saturating_sub(1).min(6))
            .min(MAX_RETRY_AFTER)
    }

    fn transport_error(&self, error: reqwest::Error) -> AppError {
        let error = error.without_url();
        let reason = if error.is_timeout() {
            "the request timed out".to_string()
        } else if error.is_connect() {
            "the server could not be reached".to_string()
        } else {
            error.to_string()
        };
        AppError::State(format!("{} request failed: {reason}", self.service))
    }

    fn too_large(&self, limit: u64) -> AppError {
        AppError::State(format!(
            "{} response exceeded the {} MiB limit",
            self.service,
            limit / (1024 * 1024)
        ))
    }

    /// The error for a final status a caller did not expect.
    pub(crate) fn status_error(&self, action: &str, response: &HttpResponse) -> AppError {
        status_error(self.service, action, response.status, None)
    }
}

/// Builds a user-facing error for an HTTP status. Authentication failures get
/// an actionable message; `detail` is a short server-provided code that never
/// contains credentials (for example an S3 error code).
pub(crate) fn status_error(
    service: &str,
    action: &str,
    status: StatusCode,
    detail: Option<&str>,
) -> AppError {
    let detail = detail
        .filter(|value| !value.is_empty())
        .map(|value| format!(" ({})", sanitize_detail(value)))
        .unwrap_or_default();
    match status {
        StatusCode::UNAUTHORIZED => AppError::State(format!(
            "{service} rejected the sign-in while trying to {action} (HTTP 401){detail}. Check the credentials in Sync settings."
        )),
        StatusCode::FORBIDDEN => AppError::State(format!(
            "{service} denied access while trying to {action} (HTTP 403){detail}. Check the credentials and their permissions in Sync settings."
        )),
        StatusCode::TOO_MANY_REQUESTS => AppError::State(format!(
            "{service} is throttling requests (HTTP 429){detail}. Try again later."
        )),
        status if status.is_redirection() => AppError::State(format!(
            "{service} redirected the request to {action} (HTTP {}). Configure the final URL instead.",
            status.as_u16()
        )),
        status => AppError::State(format!(
            "{service} could not {action} (HTTP {}){detail}",
            status.as_u16()
        )),
    }
}

fn sanitize_detail(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect()
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

/// Parses a user-configured service base URL. HTTPS is required; plain HTTP
/// is accepted only for loopback hosts such as a local MinIO or SeaweedFS.
pub(crate) fn parse_service_url(value: &str, service: &str) -> AppResult<Url> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_URL_BYTES {
        return Err(AppError::InvalidData(format!(
            "{service} URL is empty or longer than {MAX_URL_BYTES} characters"
        )));
    }
    let url = Url::parse(value)
        .map_err(|_| AppError::InvalidData(format!("{service} URL is not a valid URL")))?;
    ensure_url_allowed(&url, service)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(AppError::InvalidData(format!(
            "{service} URL cannot contain a query or fragment"
        )));
    }
    Ok(url)
}

/// The URL policy applied to every request.
pub(crate) fn ensure_url_allowed(url: &Url, service: &str) -> AppResult<()> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::InvalidData(format!(
            "{service} URL cannot embed credentials; enter them in the credential fields instead"
        )));
    }
    let Some(host) = url.host_str().filter(|host| !host.is_empty()) else {
        return Err(AppError::InvalidData(format!(
            "{service} URL is missing a host"
        )));
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(host) => Ok(()),
        "http" => Err(AppError::InvalidData(format!(
            "{service} URL must use https://. Plain http:// is allowed only for localhost."
        ))),
        _ => Err(AppError::InvalidData(format!(
            "{service} URL must use https://"
        ))),
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Decodes `%XX` escapes. Invalid escapes are kept literally.
pub(crate) fn percent_decode(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push(high << 4 | low);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    output
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Encodes every byte except RFC 3986 unreserved characters, and `/` when
/// `keep_slash` is set.
pub(crate) fn percent_encode(value: &[u8], keep_slash: bool) -> String {
    let mut output = String::with_capacity(value.len());
    for &byte in value {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && byte == b'/')
        {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::sync::backends::test_support::{MockResponse, MockServer};

    #[test]
    fn url_policy_requires_https_except_loopback() {
        for url in [
            "https://dav.example.com/remote.php/dav",
            "http://localhost:8333",
            "http://127.0.0.1:9000/bucket",
            "http://[::1]:9000",
        ] {
            parse_service_url(url, "Test").unwrap();
        }
        for url in [
            "http://dav.example.com",
            "ftp://example.com",
            "https://user:secret@example.com",
            "https://example.com/?a=b",
            "file:///tmp",
            "",
        ] {
            let error = parse_service_url(url, "Test").unwrap_err().to_string();
            assert!(!error.contains("secret"), "{error}");
        }
    }

    #[test]
    fn retries_throttling_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let server = MockServer::start(move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                MockResponse::status(503)
            } else {
                MockResponse::body(200, "ok")
            }
        });
        let client = HttpClient::new("Test")
            .unwrap()
            .with_retry_delay(Duration::from_millis(1));
        let response = client
            .send(client.client().get(server.url()), 1024)
            .unwrap();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body, b"ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn gives_up_after_bounded_retries_and_bounds_bodies() {
        let server = MockServer::start(|request| {
            if request.path() == "/big" {
                MockResponse::body(200, vec![b'x'; 2048])
            } else {
                MockResponse::status(429).with_header("Retry-After", "0")
            }
        });
        let client = HttpClient::new("Test")
            .unwrap()
            .with_retry_delay(Duration::from_millis(1));
        let response = client
            .send(client.client().get(server.url()), 1024)
            .unwrap();
        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
        let error = client
            .send(client.client().get(format!("{}/big", server.url())), 1024)
            .err()
            .unwrap();
        assert!(error.to_string().contains("limit"));
    }

    #[test]
    fn percent_coding_round_trips() {
        assert_eq!(percent_encode(b"a b/c$", true), "a%20b/c%24");
        assert_eq!(percent_encode(b"a/b", false), "a%2Fb");
        assert_eq!(percent_decode("a%20b%2fc%zz"), b"a b/c%zz");
    }
}
