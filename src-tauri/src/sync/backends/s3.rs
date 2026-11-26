//! S3-compatible transport (AWS S3, MinIO, SeaweedFS, Cloudflare R2, ...)
//! signed with AWS Signature Version 4.
//!
//! Objects live under `<prefix>/denote-sync/v1/` in one bucket. `PutObject` is
//! atomic on every S3-compatible store. `CreateOnly` uses
//! `If-None-Match: *`; a store that does not implement conditional writes is
//! detected once and then checked with `HeadObject` before writing.

// Until the backend factory constructs the network transports, only tests
// reach them.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::atomic::{AtomicBool, Ordering};

use hmac::{Hmac, KeyInit, Mac};
use quick_xml::{Reader, events::Event};
use reqwest::{
    Method, StatusCode, Url,
    blocking::RequestBuilder,
    header::{AUTHORIZATION, CONTENT_TYPE, IF_NONE_MATCH},
};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{
    error::{AppError, AppResult},
    sync::backend::{Backend, PutMode, PutOutcome, REMOTE_PREFIX, validate_key, validate_prefix},
};

use super::http::{
    HttpClient, HttpResponse, MAX_LISTING_BYTES, MAX_OBJECT_BYTES, parse_service_url,
    percent_decode, percent_encode, status_error,
};

const SERVICE: &str = "S3 storage";
const MAX_PREFIX_BYTES: usize = 256;
const MAX_CREDENTIAL_BYTES: usize = 256;
const MAX_LIST_PAGES: usize = 10_000;
const MAX_LISTED_KEYS: usize = 2_000_000;
const MAX_CONTINUATION_TOKEN_BYTES: usize = 4096;

pub(crate) struct S3Backend {
    http: HttpClient,
    /// Bucket root URL, ending in `/`.
    bucket_url: Url,
    region: String,
    /// `<prefix>/denote-sync/v1/` (or just the remote prefix).
    root: String,
    access_key_id: String,
    secret_access_key: Zeroizing<String>,
    conditional_put_unsupported: AtomicBool,
}

impl S3Backend {
    pub(crate) fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        prefix: &str,
        path_style: bool,
        access_key_id: &str,
        secret_access_key: &str,
    ) -> AppResult<Self> {
        Self::with_client(
            endpoint,
            region,
            bucket,
            prefix,
            path_style,
            access_key_id,
            secret_access_key,
            HttpClient::new(SERVICE)?,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_client(
        endpoint: &str,
        region: &str,
        bucket: &str,
        prefix: &str,
        path_style: bool,
        access_key_id: &str,
        secret_access_key: &str,
        http: HttpClient,
    ) -> AppResult<Self> {
        let mut base = parse_service_url(endpoint, SERVICE)?;
        validate_region(region)?;
        validate_bucket(bucket)?;
        let prefix = normalize_prefix(prefix)?;
        if access_key_id.is_empty()
            || secret_access_key.is_empty()
            || access_key_id.len() > MAX_CREDENTIAL_BYTES
            || secret_access_key.len() > MAX_CREDENTIAL_BYTES
            || !access_key_id.bytes().all(|byte| byte.is_ascii_graphic())
            || !secret_access_key
                .bytes()
                .all(|byte| byte.is_ascii_graphic())
        {
            return Err(AppError::InvalidData(
                "S3 access key ID or secret access key is missing or invalid".to_string(),
            ));
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let bucket_url = if path_style {
            base.join(&format!("{bucket}/"))
                .map_err(|_| AppError::InvalidData("S3 bucket name is invalid".to_string()))?
        } else {
            let host = base
                .host_str()
                .ok_or_else(|| AppError::InvalidData("S3 endpoint is missing a host".to_string()))?
                .to_string();
            if host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok() {
                return Err(AppError::InvalidData(
                    "S3 endpoints addressed by IP need path-style addressing".to_string(),
                ));
            }
            base.set_host(Some(&format!("{bucket}.{host}")))
                .map_err(|_| AppError::InvalidData("S3 bucket name is invalid".to_string()))?;
            base
        };
        Ok(Self {
            http,
            bucket_url,
            region: region.to_string(),
            root: format!("{prefix}{REMOTE_PREFIX}"),
            access_key_id: access_key_id.to_string(),
            secret_access_key: Zeroizing::new(secret_access_key.to_string()),
            conditional_put_unsupported: AtomicBool::new(false),
        })
    }

    fn object_url(&self, key: &str) -> AppResult<Url> {
        self.bucket_url
            .join(&format!("{}{key}", self.root))
            .map_err(|_| AppError::InvalidData(format!("Invalid sync storage key: {key}")))
    }

    fn signed(&self, method: Method, url: Url, body: Vec<u8>) -> AppResult<RequestBuilder> {
        let payload_hash = hex::encode(Sha256::digest(&body));
        let amz_date = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string();
        let headers = vec![
            ("host".to_string(), host_header(&url)?),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        let authorization = authorization(&SigningInput {
            method: method.as_str(),
            url: &url,
            headers: &headers,
            payload_sha256: &payload_hash,
            amz_date: &amz_date,
            region: &self.region,
            service: "s3",
            access_key_id: &self.access_key_id,
            secret_access_key: &self.secret_access_key,
        });
        let mut request = self
            .http
            .client()
            .request(method, url)
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", amz_date)
            .header(AUTHORIZATION, authorization);
        if !body.is_empty() {
            request = request
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(body);
        } else {
            request = request.body(Vec::new());
        }
        Ok(request)
    }

    fn send(&self, method: Method, url: Url, body: Vec<u8>, limit: u64) -> AppResult<HttpResponse> {
        self.http.send(self.signed(method, url, body)?, limit)
    }

    fn error(&self, action: &str, response: &HttpResponse) -> AppError {
        let code = error_code(&response.body);
        match (response.status, code.as_deref()) {
            (_, Some("NoSuchBucket")) => AppError::State(
                "The S3 bucket does not exist. Check the bucket name and endpoint.".to_string(),
            ),
            (StatusCode::MOVED_PERMANENTLY, _) | (_, Some("PermanentRedirect" | "AuthorizationHeaderMalformed")) => {
                AppError::State(
                    "The S3 bucket is in a different region or needs another endpoint. Check the region and endpoint."
                        .to_string(),
                )
            }
            (_, Some("SignatureDoesNotMatch" | "InvalidAccessKeyId")) => AppError::State(
                "S3 storage rejected the access key. Check the access key ID and secret access key."
                    .to_string(),
            ),
            (_, Some("RequestTimeTooSkewed")) => AppError::State(
                "S3 storage rejected the request because this device's clock is wrong. Correct the system time."
                    .to_string(),
            ),
            (status, code) => status_error(SERVICE, action, status, code),
        }
    }

    fn exists(&self, key: &str) -> AppResult<bool> {
        let response = self.send(Method::HEAD, self.object_url(key)?, Vec::new(), 0)?;
        match response.status {
            status if status.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(self.error("check a stored item", &response)),
        }
    }

    fn put_object(&self, key: &str, bytes: &[u8], conditional: bool) -> AppResult<HttpResponse> {
        let mut request = self.signed(Method::PUT, self.object_url(key)?, bytes.to_vec())?;
        if conditional {
            request = request.header(IF_NONE_MATCH, "*");
        }
        self.http.send(request, MAX_LISTING_BYTES)
    }

    fn list_page(&self, prefix: &str, token: Option<&str>) -> AppResult<ListPage> {
        let mut url = self.bucket_url.clone();
        let mut query = format!(
            "list-type=2&prefix={}",
            percent_encode(prefix.as_bytes(), false)
        );
        if let Some(token) = token {
            query.push_str("&continuation-token=");
            query.push_str(&percent_encode(token.as_bytes(), false));
        }
        url.set_query(Some(&query));
        let response = self.send(Method::GET, url, Vec::new(), MAX_LISTING_BYTES)?;
        if response.status != StatusCode::OK {
            return Err(self.error("list stored items", &response));
        }
        parse_list_page(&response.body)
    }
}

impl Backend for S3Backend {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        validate_key(key)?;
        let response = self.send(
            Method::GET,
            self.object_url(key)?,
            Vec::new(),
            MAX_OBJECT_BYTES,
        )?;
        match response.status {
            StatusCode::OK => Ok(Some(response.body)),
            StatusCode::NOT_FOUND
                if error_code(&response.body).as_deref() != Some("NoSuchBucket") =>
            {
                Ok(None)
            }
            _ => Err(self.error("download a stored item", &response)),
        }
    }

    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome> {
        validate_key(key)?;
        let mut conditional = mode == PutMode::CreateOnly;
        if conditional && self.conditional_put_unsupported.load(Ordering::Relaxed) {
            if self.exists(key)? {
                return Ok(PutOutcome::AlreadyExists);
            }
            conditional = false;
        }
        let response = self.put_object(key, bytes, conditional)?;
        let unsupported = conditional
            && (response.status == StatusCode::NOT_IMPLEMENTED
                || (response.status == StatusCode::BAD_REQUEST
                    && error_code(&response.body).as_deref() == Some("NotImplemented")));
        if unsupported {
            self.conditional_put_unsupported
                .store(true, Ordering::Relaxed);
            if self.exists(key)? {
                return Ok(PutOutcome::AlreadyExists);
            }
            let response = self.put_object(key, bytes, false)?;
            if !response.status.is_success() {
                return Err(self.error("upload a stored item", &response));
            }
            return Ok(PutOutcome::Written);
        }
        match response.status {
            status if status.is_success() => Ok(PutOutcome::Written),
            StatusCode::PRECONDITION_FAILED if conditional => Ok(PutOutcome::AlreadyExists),
            // A concurrent conditional write to the same key won the race.
            StatusCode::CONFLICT if conditional && self.exists(key)? => {
                Ok(PutOutcome::AlreadyExists)
            }
            _ => Err(self.error("upload a stored item", &response)),
        }
    }

    fn list(&self, prefix: &str) -> AppResult<Vec<String>> {
        validate_prefix(prefix)?;
        let full_prefix = format!("{}{prefix}", self.root);
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let page = self.list_page(&full_prefix, token.as_deref())?;
            for key in page.keys {
                if let Some(relative) = key.strip_prefix(&self.root)
                    && relative.starts_with(prefix)
                    && validate_key(relative).is_ok()
                {
                    keys.push(relative.to_string());
                }
            }
            if keys.len() > MAX_LISTED_KEYS {
                return Err(AppError::State(
                    "S3 listing exceeded the item limit".to_string(),
                ));
            }
            if !page.truncated {
                return Ok(keys);
            }
            match page.next_token {
                Some(next) if !next.is_empty() && Some(&next) != token.as_ref() => {
                    token = Some(next)
                }
                _ => {
                    return Err(AppError::State(
                        "S3 storage returned a truncated listing without a continuation token"
                            .to_string(),
                    ));
                }
            }
        }
        Err(AppError::State(
            "S3 listing exceeded the page limit".to_string(),
        ))
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        validate_key(key)?;
        let response = self.send(
            Method::DELETE,
            self.object_url(key)?,
            Vec::new(),
            MAX_LISTING_BYTES,
        )?;
        match response.status {
            status if status.is_success() => Ok(()),
            StatusCode::NOT_FOUND
                if error_code(&response.body).as_deref() != Some("NoSuchBucket") =>
            {
                Ok(())
            }
            _ => Err(self.error("delete a stored item", &response)),
        }
    }
}

fn validate_region(region: &str) -> AppResult<()> {
    if region.is_empty()
        || region.len() > 64
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(AppError::InvalidData(
            "S3 region must contain only lowercase letters, digits, and hyphens (for example us-east-1 or auto)"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_bucket(bucket: &str) -> AppResult<()> {
    let valid = (3..=63).contains(&bucket.len())
        && bucket.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && bucket
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && bucket
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && !bucket.contains("..");
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidData(
            "S3 bucket name must be 3 to 63 lowercase letters, digits, dots, or hyphens"
                .to_string(),
        ))
    }
}

/// Returns `""` or a prefix ending in `/`. Segments use `[A-Za-z0-9._-]`.
fn normalize_prefix(prefix: &str) -> AppResult<String> {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let valid = trimmed.len() <= MAX_PREFIX_BYTES
        && trimmed.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        });
    if valid {
        Ok(format!("{trimmed}/"))
    } else {
        Err(AppError::InvalidData(
            "S3 prefix may contain only letters, digits, '.', '_', '-', and '/' separators"
                .to_string(),
        ))
    }
}

fn host_header(url: &Url) -> AppResult<String> {
    let host = url
        .host_str()
        .ok_or_else(|| AppError::InvalidData("S3 endpoint is missing a host".to_string()))?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

pub(crate) struct SigningInput<'a> {
    pub(crate) method: &'a str,
    pub(crate) url: &'a Url,
    /// Headers to sign, with lowercase names.
    pub(crate) headers: &'a [(String, String)],
    pub(crate) payload_sha256: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub(crate) amz_date: &'a str,
    pub(crate) region: &'a str,
    pub(crate) service: &'a str,
    pub(crate) access_key_id: &'a str,
    pub(crate) secret_access_key: &'a str,
}

/// The `Authorization` header value for AWS Signature Version 4.
pub(crate) fn authorization(input: &SigningInput<'_>) -> String {
    let mut headers: Vec<(String, String)> = input
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.to_ascii_lowercase(),
                value.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    headers.sort();
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let path = percent_encode(&percent_decode(input.url.path()), true);
    let canonical_uri = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };
    let canonical_request = format!(
        "{}\n{canonical_uri}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        input.method,
        canonical_query(input.url),
        input.payload_sha256
    );
    let date = &input.amz_date[..input.amz_date.len().min(8)];
    let scope = format!("{date}/{}/{}/aws4_request", input.region, input.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        input.amz_date,
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let secret = Zeroizing::new(format!("AWS4{}", input.secret_access_key));
    let key = hmac_sha256(secret.as_bytes(), date.as_bytes());
    let key = hmac_sha256(&key, input.region.as_bytes());
    let key = hmac_sha256(&key, input.service.as_bytes());
    let key = hmac_sha256(&key, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        input.access_key_id
    )
}

fn canonical_query(url: &Url) -> String {
    let Some(query) = url.query().filter(|query| !query.is_empty()) else {
        return String::new();
    };
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                percent_encode(&percent_decode(name), false),
                percent_encode(&percent_decode(value), false),
            )
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length, so construction cannot fail.
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

struct ListPage {
    keys: Vec<String>,
    truncated: bool,
    next_token: Option<String>,
}

fn parse_list_page(body: &[u8]) -> AppResult<ListPage> {
    let invalid = || AppError::InvalidData("S3 storage returned an invalid listing".to_string());
    let mut page = ListPage {
        keys: Vec::new(),
        truncated: false,
        next_token: None,
    };
    let mut saw_result = false;
    for (path, text) in xml_texts(body).ok_or_else(invalid)? {
        match path.as_slice() {
            [root, ..] if root == "listbucketresult" => {
                saw_result = true;
                match &path[1..] {
                    [contents, key] if contents == "contents" && key == "key" => {
                        page.keys.push(text)
                    }
                    [name] if name == "istruncated" => page.truncated = text.trim() == "true",
                    [name] if name == "nextcontinuationtoken" => {
                        if text.len() > MAX_CONTINUATION_TOKEN_BYTES {
                            return Err(invalid());
                        }
                        page.next_token = Some(text);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if !saw_result {
        return Err(invalid());
    }
    Ok(page)
}

/// The `<Code>` of an S3 error document, if the body is one.
fn error_code(body: &[u8]) -> Option<String> {
    xml_texts(body)?.into_iter().find_map(|(path, text)| {
        (path.len() == 2 && path[0] == "error" && path[1] == "code").then_some(text)
    })
}

/// Text content of every leaf element as `(lowercase local-name path, text)`.
fn xml_texts(body: &[u8]) -> Option<Vec<(Vec<String>, String)>> {
    let text = std::str::from_utf8(body).ok()?;
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut output = Vec::new();
    loop {
        match reader.read_event().ok()? {
            Event::Start(element) => {
                stack.push(element.local_name().as_ref().to_ascii_lowercase());
                current.clear();
                if stack.len() > 32 {
                    return None;
                }
            }
            Event::End(_) => {
                if !current.is_empty() {
                    output.push((stack.clone(), std::mem::take(&mut current)));
                }
                stack.pop()?;
            }
            Event::Text(content) => current.push_str(&content.xml10_content()),
            Event::CData(content) => current.push_str(content.as_ref()),
            Event::GeneralRef(reference) => match reference.resolve_char_ref() {
                Ok(Some(character)) => current.push(character),
                Ok(None) => current.push_str(match reference.xml10_content().as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => return None,
                }),
                Err(_) => return None,
            },
            Event::Eof if stack.is_empty() => break,
            Event::Eof => return None,
            _ => {}
        }
    }
    Some(output)
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

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn signature(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        payload: &str,
        date: &str,
        service: &str,
        key: (&str, &str),
    ) -> String {
        let url = Url::parse(url).unwrap();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        authorization(&SigningInput {
            method,
            url: &url,
            headers: &headers,
            payload_sha256: payload,
            amz_date: date,
            region: "us-east-1",
            service,
            access_key_id: key.0,
            secret_access_key: key.1,
        })
    }

    #[test]
    fn signs_aws_sigv4_test_suite_vectors() {
        let key = ("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY");
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        assert_eq!(
            signature(
                "GET",
                "https://example.amazonaws.com/",
                &headers,
                EMPTY_SHA256,
                "20150830T123600Z",
                "service",
                key
            ),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert!(
            signature(
                "GET",
                "https://example.amazonaws.com/?Param2=value2&Param1=value1",
                &headers,
                EMPTY_SHA256,
                "20150830T123600Z",
                "service",
                key
            )
            .ends_with(
                "Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
            )
        );
    }

    #[test]
    fn signs_aws_s3_documentation_examples() {
        let key = (
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        let get = signature(
            "GET",
            "https://examplebucket.s3.amazonaws.com/test.txt",
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", "20130524T000000Z"),
            ],
            EMPTY_SHA256,
            "20130524T000000Z",
            "s3",
            key,
        );
        assert_eq!(
            get,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        let list = signature(
            "GET",
            "https://examplebucket.s3.amazonaws.com/?max-keys=2&prefix=J",
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", "20130524T000000Z"),
            ],
            EMPTY_SHA256,
            "20130524T000000Z",
            "s3",
            key,
        );
        assert!(
            list.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{list}"
        );
    }

    #[derive(Default)]
    struct S3State {
        objects: BTreeMap<String, Vec<u8>>,
        conditional_unsupported: bool,
        fail_next: u32,
        requests: Vec<String>,
    }

    const ACCESS_KEY: &str = "AKIDSYNTHETIC";
    const SECRET_KEY: &str = "synthetic/secret+key";

    fn verify_signature(request: &MockRequest) -> bool {
        let Some(authorization_header) = request.header("authorization") else {
            return false;
        };
        let payload = hex::encode(Sha256::digest(&request.body));
        if request.header("x-amz-content-sha256") != Some(payload.as_str()) {
            return false;
        }
        let date = request.header("x-amz-date").unwrap_or_default();
        let url = Url::parse(&format!(
            "http://{}{}",
            request.header("host").unwrap(),
            request.target
        ))
        .unwrap();
        let headers = vec![
            (
                "host".to_string(),
                request.header("host").unwrap().to_string(),
            ),
            ("x-amz-content-sha256".to_string(), payload.clone()),
            ("x-amz-date".to_string(), date.to_string()),
        ];
        let expected = authorization(&SigningInput {
            method: &request.method,
            url: &url,
            headers: &headers,
            payload_sha256: &payload,
            amz_date: date,
            region: "us-east-1",
            service: "s3",
            access_key_id: ACCESS_KEY,
            secret_access_key: SECRET_KEY,
        });
        expected == authorization_header
    }

    fn handle(state: &mut S3State, request: &MockRequest) -> MockResponse {
        state
            .requests
            .push(format!("{} {}", request.method, request.target));
        if state.fail_next > 0 {
            state.fail_next -= 1;
            return MockResponse::status(503);
        }
        if !verify_signature(request) {
            return MockResponse::body(
                403,
                "<Error><Code>SignatureDoesNotMatch</Code><StringToSign>secret-free</StringToSign></Error>",
            );
        }
        let Some(key) = request.path().strip_prefix("/bucket/") else {
            return MockResponse::body(404, "<Error><Code>NoSuchBucket</Code></Error>");
        };
        let key = String::from_utf8(percent_decode(key)).unwrap();
        match (request.method.as_str(), key.is_empty()) {
            ("GET", true) => {
                assert_eq!(request.query("list-type").as_deref(), Some("2"));
                let prefix = request.query("prefix").unwrap_or_default();
                let start = request.query("continuation-token").unwrap_or_default();
                let matching: Vec<&String> = state
                    .objects
                    .keys()
                    .filter(|item| item.starts_with(&prefix) && item.as_str() > start.as_str())
                    .collect();
                let page = &matching[..matching.len().min(2)];
                let mut body = String::from(
                    "<?xml version=\"1.0\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
                );
                for item in page {
                    body.push_str(&format!(
                        "<Contents><Key>{item}</Key><Size>1</Size></Contents>"
                    ));
                }
                if matching.len() > 2 {
                    // Tokens with reserved characters must be re-encoded.
                    body.push_str(&format!(
                        "<IsTruncated>true</IsTruncated><NextContinuationToken>{}</NextContinuationToken>",
                        page[1]
                    ));
                } else {
                    body.push_str("<IsTruncated>false</IsTruncated>");
                }
                body.push_str("</ListBucketResult>");
                MockResponse::body(200, body)
            }
            ("GET", false) => match state.objects.get(&key) {
                Some(bytes) => MockResponse::body(200, bytes.clone()),
                None => MockResponse::body(404, "<Error><Code>NoSuchKey</Code></Error>"),
            },
            ("HEAD", false) => MockResponse::status(if state.objects.contains_key(&key) {
                200
            } else {
                404
            }),
            ("PUT", false) => {
                if request.header("if-none-match").is_some() {
                    if state.conditional_unsupported {
                        return MockResponse::body(
                            501,
                            "<Error><Code>NotImplemented</Code></Error>",
                        );
                    }
                    if state.objects.contains_key(&key) {
                        return MockResponse::body(
                            412,
                            "<Error><Code>PreconditionFailed</Code></Error>",
                        );
                    }
                }
                state.objects.insert(key, request.body.clone());
                MockResponse::status(200)
            }
            ("DELETE", false) => {
                state.objects.remove(&key);
                MockResponse::status(204)
            }
            _ => MockResponse::status(405),
        }
    }

    fn server(conditional_unsupported: bool) -> (MockServer, Arc<Mutex<S3State>>) {
        let state = Arc::new(Mutex::new(S3State {
            conditional_unsupported,
            ..S3State::default()
        }));
        let shared = Arc::clone(&state);
        let server = MockServer::start(move |request| handle(&mut shared.lock().unwrap(), request));
        (server, state)
    }

    fn backend(server: &MockServer, bucket: &str, secret: &str) -> S3Backend {
        S3Backend::with_client(
            &server.url(),
            "us-east-1",
            bucket,
            "/team/denote/",
            true,
            ACCESS_KEY,
            secret,
            HttpClient::new(SERVICE)
                .unwrap()
                .with_retry_delay(Duration::from_millis(1)),
        )
        .unwrap()
    }

    #[test]
    fn conforms_with_conditional_put_and_pagination() {
        let (server, state) = server(false);
        conformance(&backend(&server, "bucket", SECRET_KEY));
        let state = state.lock().unwrap();
        assert!(
            state
                .objects
                .contains_key("team/denote/denote-sync/v1/repository.json")
        );
        let list_pages = state
            .requests
            .iter()
            .filter(|line| line.contains("continuation-token="))
            .count();
        assert!(list_pages > 0, "pagination was not exercised");
        assert!(!state.requests.iter().any(|line| line.starts_with("HEAD")));
    }

    #[test]
    fn conforms_without_conditional_put() {
        let (server, state) = server(true);
        conformance(&backend(&server, "bucket", SECRET_KEY));
        let state = state.lock().unwrap();
        let rejected = state
            .requests
            .iter()
            .filter(|line| line.starts_with("PUT"))
            .count();
        assert!(state.requests.iter().any(|line| line.starts_with("HEAD")));
        assert!(rejected > 0);
    }

    #[test]
    fn retries_transient_failures() {
        let (server, state) = server(false);
        let backend = backend(&server, "bucket", SECRET_KEY);
        state.lock().unwrap().fail_next = 2;
        backend
            .put("repository.json", b"x", PutMode::Overwrite)
            .unwrap();
        assert_eq!(
            backend.get("repository.json").unwrap().as_deref(),
            Some(&b"x"[..])
        );
    }

    #[test]
    fn reports_auth_and_bucket_errors_without_secrets() {
        let (server, _) = server(false);
        let wrong = backend(&server, "bucket", "wrong-secret-value");
        let message = wrong.list("").unwrap_err().to_string();
        assert!(message.contains("access key"), "{message}");
        assert!(!message.contains("wrong-secret-value"));
        let missing = backend(&server, "other", SECRET_KEY);
        assert!(
            missing
                .get("repository.json")
                .unwrap_err()
                .to_string()
                .contains("bucket does not exist")
        );
    }

    #[test]
    fn validates_configuration() {
        let ok = |endpoint: &str, bucket: &str, prefix: &str, path_style: bool| {
            S3Backend::new(endpoint, "auto", bucket, prefix, path_style, "id", "secret").is_ok()
        };
        assert!(ok("https://s3.example.com", "my-bucket", "", false));
        assert!(ok("http://127.0.0.1:8333", "my-bucket", "a/b", true));
        assert!(!ok("http://s3.example.com", "my-bucket", "", true));
        assert!(!ok("https://s3.example.com", "My_Bucket", "", true));
        assert!(!ok("https://s3.example.com", "bucket", "../x", true));
        assert!(!ok("http://127.0.0.1:8333", "bucket", "", false));
        let virtual_host = S3Backend::new(
            "https://s3.example.com",
            "auto",
            "bucket",
            "",
            false,
            "id",
            "secret",
        )
        .unwrap();
        assert_eq!(
            virtual_host.object_url("repository.json").unwrap().as_str(),
            "https://bucket.s3.example.com/denote-sync/v1/repository.json"
        );
    }
}
