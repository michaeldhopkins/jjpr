//! A stub HTTP/1.1 server for exercising the forge backends without a
//! network, using only the standard library.
//!
//! The backends were the one layer with no unit coverage: every method
//! shelled out to a real forge and was only ever exercised by the
//! `JJPR_E2E` suite, which mutation testing cannot see. This is enough
//! server to assert which path a method hits, with which verb and body,
//! and what the client does with the status that comes back.
//!
//! It is deliberately small: one request per connection (every response
//! says `Connection: close`), request bodies must carry `Content-Length`
//! (no chunked encoding), and `Expect: 100-continue` is not honoured.
//! That is what `ForgeClient` sends today; if it ever grows past that,
//! grow this with it rather than reaching for a mocking crate.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A client that connects and then stalls would otherwise park the server
/// thread in `read` forever, and `Drop` joins that thread.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// One request as the server saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    /// The request target as sent, query string included.
    pub target: String,
    pub body: String,
}

/// A canned response for one `(method, target)`.
#[derive(Debug, Clone)]
pub struct Route {
    pub method: &'static str,
    pub target: String,
    pub status: u16,
    pub body: String,
}

pub fn route(method: &'static str, target: &str, status: u16, body: &str) -> Route {
    Route {
        method,
        target: target.to_string(),
        status,
        body: body.to_string(),
    }
}

pub struct StubServer {
    base_url: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StubServer {
    /// Serve `routes`; anything unmatched answers 404 with a JSON body.
    pub fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let table: HashMap<(String, String), (u16, String)> = routes
            .into_iter()
            .map(|r| ((r.method.to_string(), r.target), (r.status, r.body)))
            .collect();

        let thread = {
            let recorded = Arc::clone(&recorded);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                    serve_one(stream, &table, &recorded);
                }
            })
        };

        Self {
            base_url: format!("http://{addr}"),
            recorded,
            stop,
            thread: Some(thread),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("poisoned").clone()
    }

    /// `"METHOD target"` per request, in order. Bodies are dropped, which
    /// keeps the common assertion a one-liner.
    pub fn request_lines(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.target))
            .collect()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        // An atomic, not a mutex: this runs during a failing test's unwind
        // too, and a poisoned lock there would turn one panic into an abort.
        self.stop.store(true, Ordering::SeqCst);
        // `incoming()` blocks in accept; one throwaway connection wakes it so
        // it can observe the stop flag.
        let addr = self.base_url.trim_start_matches("http://");
        let _ = TcpStream::connect(addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_one(
    mut stream: TcpStream,
    table: &HashMap<(String, String), (u16, String)>,
    recorded: &Arc<Mutex<Vec<Recorded>>>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let body_start = head_end + 4;
    while buf.len() < body_start + content_length {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[body_start..]).to_string();

    recorded.lock().expect("poisoned").push(Recorded {
        method: method.clone(),
        target: target.clone(),
        body,
    });

    let (status, resp_body) = table
        .get(&(method, target))
        .cloned()
        .unwrap_or((404, r#"{"message":"Not Found"}"#.to_string()));
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Status",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
        resp_body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// The read timeout is what keeps a stalled client from parking the
    /// server thread, and with it the `Drop` that joins it.
    #[test]
    fn stalled_client_does_not_hang_shutdown() {
        let server = StubServer::start(vec![]);
        let addr = server.base_url().trim_start_matches("http://").to_string();
        let stalled = TcpStream::connect(&addr).expect("connect");
        // Give the server thread time to accept and block in read.
        std::thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        drop(server);
        assert!(
            started.elapsed() < READ_TIMEOUT + Duration::from_secs(2),
            "shutdown took {:?}",
            started.elapsed()
        );
        drop(stalled);
    }

    #[test]
    fn unmatched_requests_answer_404_and_are_still_recorded() {
        let server = StubServer::start(vec![]);
        let mut stream =
            TcpStream::connect(server.base_url().trim_start_matches("http://")).expect("connect");
        stream
            .write_all(b"POST /nowhere HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}")
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");

        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{response}"
        );
        assert_eq!(
            server.requests(),
            vec![Recorded {
                method: "POST".to_string(),
                target: "/nowhere".to_string(),
                body: "{}".to_string(),
            }]
        );
    }
}
