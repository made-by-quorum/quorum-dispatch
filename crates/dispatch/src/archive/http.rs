//! A minimal synchronous HTTP/1.1 client over `std::net::TcpStream` — one
//! request per connection, GET/PUT bodies only, no redirects, no
//! chunked-request-body support (chunked *responses* are read).
//!
//! **Why hand-rolled instead of reqwest/hyper**: those are already resolvable
//! from this workspace's `Cargo.lock` (pinned via `libduckdb-sys`), but
//! inspection of the lock file showed that edge is a **build-dependency**
//! (duckdb's build script fetches a prebuilt binary) — not a normal
//! dependency of any binary. Adding reqwest as a normal dependency of `qd`
//! would therefore be genuinely NEW compile surface for this target (async
//! reqwest + hyper + rustls + tokio's fuller feature set + quinn/http3),
//! not free reuse of something already built for this binary. With the box's
//! RAM critically tight at write time (the coordinator's brief: ~140MB free),
//! this hand-rolled client — using only `std`, plus `sha2`/`hmac` for
//! signing, both already tiny — was the more conservative call. This is a
//! DIVERGENCE from the coordinator's suggested "reqwest-based" framing,
//! flagged here and in the response back.
//!
//! **TLS (Atomic D)**: an `https://` endpoint (direct AWS S3) is spoken by
//! wrapping the same blocking `TcpStream` in a synchronous rustls session
//! behind the one `request_on` seam below — the HTTP/1.1 wire logic above it
//! is byte-identical for both schemes. Trust roots are the bundled Mozilla
//! set (`webpki-roots`), so per-host self-builds need no system cert store or
//! OpenSSL. Certificate or handshake failures surface as loud [`HttpError`]s,
//! never a silent plaintext fallback.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Process-wide rustls client config: built once (root-store parse is not
/// free), shared by every TLS request. ring provider pinned explicitly so the
/// build never depends on a process-default crypto provider being installed.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("rustls: ring provider supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
        )
    })
    .clone()
}

/// The one stream type the wire logic reads/writes — plaintext for
/// garage/minio-style local stores, rustls-wrapped for https endpoints.
enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
        }
    }
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct HttpError(pub String);

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for HttpError {}

/// Issue one HTTP/1.1 request over a fresh plaintext TCP connection.
/// `authority` is `host:port`. Thin shim over [`request_on`] with TLS off —
/// kept so plaintext callers (and this module's wire tests) are untouched by
/// the TLS seam.
pub fn request(
    authority: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<HttpResponse, HttpError> {
    request_on(authority, None, method, path, headers, body)
}

/// Issue one HTTP/1.1 request over a fresh TCP connection to `authority`
/// (`host:port`), TLS-wrapped iff `tls_server_name` is `Some` (the name is
/// what the server's certificate must match — SNI + verification). No
/// connection pooling — a persist/upload copy makes at most three requests
/// per invocation (GET existing, optional set-aside PUT, PUT canonical); a
/// short-lived CLI process gains nothing from pooling and loses the
/// complexity of managing a keep-alive connection's lifetime.
pub fn request_on(
    authority: &str,
    tls_server_name: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<HttpResponse, HttpError> {
    let stream = TcpStream::connect(authority)
        .map_err(|e| HttpError(format!("connect {authority}: {e}")))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|e| HttpError(format!("set_read_timeout: {e}")))?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|e| HttpError(format!("set_write_timeout: {e}")))?;
    let stream = match tls_server_name {
        None => Transport::Plain(stream),
        Some(name) => {
            let server_name = rustls::pki_types::ServerName::try_from(name.to_string())
                .map_err(|e| HttpError(format!("invalid TLS server name {name:?}: {e}")))?;
            let conn = rustls::ClientConnection::new(tls_config(), server_name)
                .map_err(|e| HttpError(format!("TLS session setup for {name}: {e}")))?;
            Transport::Tls(Box::new(rustls::StreamOwned::new(conn, stream)))
        }
    };

    let mut request_bytes = Vec::with_capacity(body.len() + 512);
    request_bytes.extend_from_slice(format!("{method} {path} HTTP/1.1\r\n").as_bytes());
    for (k, v) in headers {
        request_bytes.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    request_bytes.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
    request_bytes.extend_from_slice(b"connection: close\r\n\r\n");
    request_bytes.extend_from_slice(body);

    let mut stream = stream;
    stream
        .write_all(&request_bytes)
        .map_err(|e| HttpError(format!("write to {authority}: {e}")))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| HttpError(format!("read status line from {authority}: {e}")))?;
    let status = parse_status_line(&status_line)
        .ok_or_else(|| HttpError(format!("malformed status line: {status_line:?}")))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| HttpError(format!("read header from {authority}: {e}")))?;
        if line.is_empty() {
            return Err(HttpError(format!(
                "connection closed before headers completed ({authority})"
            )));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "content-length" => content_length = v.parse().ok(),
                "transfer-encoding" if v.eq_ignore_ascii_case("chunked") => chunked = true,
                _ => {}
            }
        }
    }

    // A response to a HEAD request never carries a message body, even though
    // the server echoes the `Content-Length`/`Transfer-Encoding` the matching
    // GET would return (RFC 9110 §9.3.2). Reading `content_length` bytes here
    // would block until the socket's read timeout, so HEAD short-circuits to an
    // empty body before either body-reading branch.
    let is_head = method.eq_ignore_ascii_case("HEAD");
    let body = if is_head {
        Vec::new()
    } else if chunked {
        read_chunked_body(&mut reader, authority)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| HttpError(format!("read body from {authority}: {e}")))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(|e| HttpError(format!("read body from {authority}: {e}")))?;
        buf
    };

    Ok(HttpResponse { status, body })
}

/// The live-body outcome of [`request_stream_on`]: the parsed response head
/// plus the connection positioned exactly at the first body byte, so the
/// caller can stream the body without ever buffering it whole.
///
/// `reader` is the same TLS-or-plaintext transport [`request_on`] uses,
/// wrapped in a `BufReader` and advanced past the status line and headers.
/// The caller applies its own body framing (Content-Length `take`, or a
/// chunked decoder) using `content_length`/`chunked`, then reads to EOF —
/// the `connection: close` this request sends guarantees the socket closes
/// at end-of-body.
pub struct HttpStream {
    pub status: u16,
    pub content_length: Option<u64>,
    pub chunked: bool,
    pub reader: Box<dyn BufRead + Send + 'static>,
}

/// Issue one streaming HTTP/1.1 GET over a fresh connection to `authority`,
/// TLS-wrapped iff `tls_server_name` is `Some` (identical scheme handling to
/// [`request_on`]). Parses the status line and headers, then hands back the
/// live connection at the first body byte via [`HttpStream`] — the body is
/// NEVER read here, so an arbitrarily large object streams through the caller
/// without a full-object allocation [M3]. This is the streaming twin of
/// `request_on`; it exists so callers that must stream (qbt-serve's chunked
/// serve path) get the exact same rustls TLS session as the buffering path,
/// rather than re-implementing TLS at each call site.
///
/// `headers` are sent verbatim; a `content-length: 0` and `connection: close`
/// are appended (a GET carries no body and this client does one request per
/// connection). Only GET is supported — the streaming contract is a read.
pub fn request_stream_on(
    authority: &str,
    tls_server_name: Option<&str>,
    path: &str,
    headers: &[(String, String)],
) -> Result<HttpStream, HttpError> {
    let stream = TcpStream::connect(authority)
        .map_err(|e| HttpError(format!("connect {authority}: {e}")))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|e| HttpError(format!("set_read_timeout: {e}")))?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|e| HttpError(format!("set_write_timeout: {e}")))?;
    let stream = match tls_server_name {
        None => Transport::Plain(stream),
        Some(name) => {
            let server_name = rustls::pki_types::ServerName::try_from(name.to_string())
                .map_err(|e| HttpError(format!("invalid TLS server name {name:?}: {e}")))?;
            let conn = rustls::ClientConnection::new(tls_config(), server_name)
                .map_err(|e| HttpError(format!("TLS session setup for {name}: {e}")))?;
            Transport::Tls(Box::new(rustls::StreamOwned::new(conn, stream)))
        }
    };

    let mut request_bytes = Vec::with_capacity(512);
    request_bytes.extend_from_slice(format!("GET {path} HTTP/1.1\r\n").as_bytes());
    for (k, v) in headers {
        request_bytes.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    request_bytes.extend_from_slice(b"content-length: 0\r\nconnection: close\r\n\r\n");

    let mut stream = stream;
    stream
        .write_all(&request_bytes)
        .map_err(|e| HttpError(format!("write to {authority}: {e}")))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| HttpError(format!("read status line from {authority}: {e}")))?;
    let status = parse_status_line(&status_line)
        .ok_or_else(|| HttpError(format!("malformed status line: {status_line:?}")))?;

    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| HttpError(format!("read header from {authority}: {e}")))?;
        if line.is_empty() {
            return Err(HttpError(format!(
                "connection closed before headers completed ({authority})"
            )));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "content-length" => content_length = v.parse().ok(),
                "transfer-encoding" if v.eq_ignore_ascii_case("chunked") => chunked = true,
                _ => {}
            }
        }
    }

    Ok(HttpStream {
        status,
        content_length,
        chunked,
        reader: Box::new(reader),
    })
}

fn parse_status_line(line: &str) -> Option<u16> {
    // "HTTP/1.1 200 OK\r\n"
    let mut parts = line.split_whitespace();
    parts.next()?; // HTTP version
    parts.next()?.parse().ok()
}

fn read_chunked_body(reader: &mut impl BufRead, authority: &str) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        reader
            .read_line(&mut size_line)
            .map_err(|e| HttpError(format!("read chunk size from {authority}: {e}")))?;
        let size_str = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| HttpError(format!("malformed chunk size: {size_line:?}")))?;
        if size == 0 {
            // Optional trailing headers, up to the final blank line.
            loop {
                let mut trailer = String::new();
                reader
                    .read_line(&mut trailer)
                    .map_err(|e| HttpError(format!("read chunk trailer from {authority}: {e}")))?;
                if trailer.trim_end_matches(['\r', '\n']).is_empty() {
                    break;
                }
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|e| HttpError(format!("read chunk body from {authority}: {e}")))?;
        out.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|e| HttpError(format!("read chunk CRLF from {authority}: {e}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Spawn a one-shot fake HTTP server: accepts exactly one connection,
    /// reads the request, hands it to `handler`, writes back the raw
    /// response bytes `handler` returns. Returns the bound authority.
    fn one_shot_server(handler: impl FnOnce(String) -> Vec<u8> + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            // Read whatever the client sent (best-effort: enough for these
            // small fixed test requests) before responding.
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let received = String::from_utf8_lossy(&buf[..n]).into_owned();
            let response = handler(received);
            stream.write_all(&response).unwrap();
        });
        addr
    }

    #[test]
    fn parses_status_and_content_length_body() {
        let addr = one_shot_server(|_req| {
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec()
        });
        let resp = request(&addr, "GET", "/bucket/key", &[], &[]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn parses_404_with_empty_body() {
        let addr = one_shot_server(|_req| {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
        });
        let resp = request(&addr, "GET", "/bucket/missing", &[], &[]).unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body.is_empty());
    }

    #[test]
    fn head_response_has_no_body_even_with_a_nonzero_content_length() {
        // A HEAD response echoes the Content-Length the matching GET would
        // return, but sends NO body. A reader that trusts Content-Length would
        // block on read_exact until the timeout; `request` must short-circuit
        // HEAD to an empty body and return promptly.
        let addr = one_shot_server(|req| {
            assert!(req.starts_with("HEAD /bucket/blob HTTP/1.1\r\n"), "{req}");
            b"HTTP/1.1 200 OK\r\nContent-Length: 999999\r\n\r\n".to_vec()
        });
        let started = std::time::Instant::now();
        let resp = request(&addr, "HEAD", "/bucket/blob", &[], &[]).unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_empty(), "HEAD carries no body");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "HEAD must not block waiting for a body the server never sends"
        );
    }

    #[test]
    fn head_404_is_a_clean_absent_not_an_error() {
        let addr = one_shot_server(|_req| {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
        });
        let resp = request(&addr, "HEAD", "/bucket/missing", &[], &[]).unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body.is_empty());
    }

    #[test]
    fn parses_chunked_response_body() {
        let addr = one_shot_server(|_req| {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"
                .to_vec()
        });
        let resp = request(&addr, "GET", "/bucket/key", &[], &[]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello world");
    }

    #[test]
    fn sends_headers_and_body_and_content_length() {
        let addr = one_shot_server(|req| {
            assert!(req.starts_with("PUT /bucket/key HTTP/1.1\r\n"));
            assert!(req.contains("x-amz-date: 20260707T000000Z\r\n"));
            assert!(req.contains("content-length: 4\r\n"));
            assert!(req.ends_with("body"));
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec()
        });
        let resp = request(
            &addr,
            "PUT",
            "/bucket/key",
            &[("x-amz-date".to_string(), "20260707T000000Z".to_string())],
            b"body",
        )
        .unwrap();
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn stream_get_returns_head_and_a_live_content_length_body() {
        let addr = one_shot_server(|req| {
            assert!(req.starts_with("GET /bucket/blob HTTP/1.1\r\n"), "{req}");
            assert!(req.contains("connection: close\r\n"), "{req}");
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world".to_vec()
        });
        let mut s = request_stream_on(
            &addr,
            None,
            "/bucket/blob",
            &[("x-amz-date".to_string(), "20260707T000000Z".to_string())],
        )
        .unwrap();
        assert_eq!(s.status, 200);
        assert_eq!(s.content_length, Some(11));
        assert!(!s.chunked);
        // The body is still on the wire — read it now, framed by content-length.
        let mut body = Vec::new();
        (&mut s.reader).take(11).read_to_end(&mut body).unwrap();
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn stream_get_surfaces_chunked_flag_and_leaves_body_on_the_wire() {
        let addr = one_shot_server(|_req| {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n".to_vec()
        });
        let s = request_stream_on(&addr, None, "/b/k", &[]).unwrap();
        assert_eq!(s.status, 200);
        assert!(s.chunked, "chunked flag must be surfaced to the caller");
        assert_eq!(s.content_length, None);
    }

    #[test]
    fn stream_get_reports_status_without_reading_body() {
        let addr = one_shot_server(|_req| {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
        });
        let s = request_stream_on(&addr, None, "/b/missing", &[]).unwrap();
        assert_eq!(s.status, 404);
        assert_eq!(s.content_length, Some(0));
    }

    #[test]
    fn connect_failure_is_a_loud_error_not_a_panic() {
        // Port 0 after binding-and-dropping is unlikely to be reachable;
        // instead connect to a closed listener for a deterministic refusal.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener); // now nothing is listening on `addr`
        let err = request(&addr, "GET", "/x/y", &[], &[]).unwrap_err();
        assert!(err.0.contains(&addr) || err.0.to_lowercase().contains("connect"));
    }
}
