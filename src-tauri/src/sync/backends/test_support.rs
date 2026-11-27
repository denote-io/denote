//! Shared test support for the sync transports: a minimal HTTP/1.1 server on
//! a loopback port, where each connection serves one request and is closed,
//! and a minimal backend conformance check.

use crate::sync::backend::{Backend, PutMode, PutOutcome};

use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone, Debug)]
pub(crate) struct MockRequest {
    pub(crate) method: String,
    /// Raw request target, including any query string.
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl MockRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    /// Decoded query parameter.
    pub(crate) fn query(&self, name: &str) -> Option<String> {
        let query = self.target.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then(|| {
                String::from_utf8(super::http::percent_decode(&value.replace('+', " "))).unwrap()
            })
        })
    }
}

pub(crate) struct MockResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl MockResponse {
    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub(crate) fn body(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

pub(crate) struct MockServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    pub(crate) fn start(
        handler: impl Fn(&MockRequest) -> MockResponse + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = stream {
                    serve(stream, &handler);
                }
            }
        });
        Self {
            port,
            stop,
            thread: Some(thread),
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(stream: TcpStream, handler: &impl Fn(&MockRequest) -> MockResponse) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let request = MockRequest {
        method,
        target,
        headers,
        body,
    };
    let response = handler(&request);
    let mut output = format!(
        "HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        output.push_str(&format!("{name}: {value}\r\n"));
    }
    output.push_str("\r\n");
    let mut stream = stream;
    let _ = stream.write_all(output.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

/// Conformance checks every transport passes against an empty store. They
/// mirror the engine's `backend_tests::conformance` and add prefix edge cases.
pub(crate) fn conformance(backend: &dyn Backend) {
    backend.refresh().unwrap();
    assert_eq!(backend.get("repository.json").unwrap(), None);
    assert!(backend.list("").unwrap().is_empty());
    backend.delete("objects/aa/missing").unwrap();
    assert_eq!(
        backend
            .put("repository.json", b"first", PutMode::CreateOnly)
            .unwrap(),
        PutOutcome::Written
    );
    assert_eq!(
        backend
            .put("repository.json", b"second", PutMode::CreateOnly)
            .unwrap(),
        PutOutcome::AlreadyExists
    );
    assert_eq!(
        backend.get("repository.json").unwrap().as_deref(),
        Some(&b"first"[..])
    );
    assert_eq!(
        backend
            .put("repository.json", b"third", PutMode::Overwrite)
            .unwrap(),
        PutOutcome::Written
    );
    assert_eq!(
        backend.get("repository.json").unwrap().as_deref(),
        Some(&b"third"[..])
    );
    let object = format!("objects/ab/ab{}", "0".repeat(62));
    let keys = [
        object.as_str(),
        "heads/0123456789abcdef0123456789abcdef/00000000000000000001",
        "heads/0123456789abcdef0123456789abcdef/00000000000000000002",
        "heads/fedcba9876543210fedcba9876543210/00000000000000000001",
        "retired/fedcba9876543210fedcba9876543210",
    ];
    for (index, key) in keys.iter().enumerate() {
        let value = vec![index as u8; index * 3];
        backend.put(key, &value, PutMode::CreateOnly).unwrap();
        assert_eq!(backend.get(key).unwrap(), Some(value));
    }
    backend.flush().unwrap();
    let sorted = |mut keys: Vec<String>| {
        keys.sort();
        keys
    };
    assert_eq!(
        sorted(backend.list("heads/").unwrap()),
        sorted(keys[1..4].iter().map(|key| key.to_string()).collect())
    );
    assert_eq!(
        backend
            .list("heads/0123456789abcdef0123456789abcdef/")
            .unwrap()
            .len(),
        2
    );
    assert_eq!(backend.list("objects/").unwrap(), vec![object.clone()]);
    assert_eq!(backend.list("objects/ab").unwrap(), vec![object.clone()]);
    assert!(backend.list("objects/cd/").unwrap().is_empty());
    let mut all: Vec<String> = keys.iter().map(|key| key.to_string()).collect();
    all.push("repository.json".to_string());
    assert_eq!(sorted(backend.list("").unwrap()), sorted(all));
    backend.delete(keys[1]).unwrap();
    backend.delete(keys[1]).unwrap();
    assert_eq!(backend.get(keys[1]).unwrap(), None);
    assert_eq!(backend.list("heads/").unwrap().len(), 2);

    let empty = "heads/00000000000000000000000000000000/00000000000000000001";
    backend.put(empty, b"", PutMode::CreateOnly).unwrap();
    assert_eq!(backend.get(empty).unwrap().as_deref(), Some(&b""[..]));
    let binary: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    let mut expected = BTreeSet::from([object.clone()]);
    for index in 0..40u32 {
        let id = format!("{index:02x}{}", "e".repeat(62));
        let key = format!("objects/{}/{id}", &id[..2]);
        backend.put(&key, &binary, PutMode::CreateOnly).unwrap();
        expected.insert(key);
    }
    backend.flush().unwrap();
    let listed: BTreeSet<String> = backend.list("objects/").unwrap().into_iter().collect();
    assert_eq!(listed, expected);
    let first = expected.iter().next().unwrap().clone();
    assert_eq!(
        backend.get(&first).unwrap().as_deref(),
        Some(binary.as_slice())
    );
    assert_eq!(backend.list("retired/").unwrap().len(), 1);
    backend.delete(&first).unwrap();
    assert_eq!(backend.get(&first).unwrap(), None);
    assert!(!backend.list("objects/").unwrap().contains(&first));

    for invalid in ["../escape", "/absolute", "Upper/case", "a//b", "Bad Key"] {
        assert!(backend.get(invalid).is_err(), "{invalid}");
        assert!(
            backend.put(invalid, b"x", PutMode::Overwrite).is_err(),
            "{invalid}"
        );
        assert!(backend.delete(invalid).is_err(), "{invalid}");
    }
    assert!(backend.list("../").is_err());
    backend.flush().unwrap();
}
