//! Loopback HTTP/1.1 fixture server for adapter tests.
//!
//! Each accepted connection receives the next scripted response. Bodies are
//! sent with chunked transfer encoding in caller-chosen fragments, so tests can
//! split SSE records anywhere. A fragment may wait on a release channel to hold
//! the stream open at an exact boundary without sleeping.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// One recorded request.
#[derive(Clone, Debug)]
pub(crate) struct RecordedRequest {
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl RecordedRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn json(&self) -> tea_protocol::JsonValue {
        tea_protocol::JsonValue::parse(std::str::from_utf8(&self.body).expect("UTF-8 body"))
            .expect("JSON body")
    }
}

/// One piece of a scripted response body.
pub(crate) enum Piece {
    Bytes(Vec<u8>),
    /// Hold until the paired sender fires (or is dropped).
    Wait(Receiver<()>),
}

/// One scripted response.
pub(crate) struct Scripted {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) pieces: Vec<Piece>,
    /// Close the connection before sending anything.
    pub(crate) drop_connection: bool,
}

impl Scripted {
    /// A 200 SSE response delivered in `fragment`-byte pieces.
    pub(crate) fn sse(body: &[u8], fragment: usize) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            pieces: body
                .chunks(fragment.max(1))
                .map(|chunk| Piece::Bytes(chunk.to_vec()))
                .collect(),
            drop_connection: false,
        }
    }

    /// An error response with a JSON body.
    pub(crate) fn error(status: u16, headers: &[(&str, &str)], body: &str) -> Self {
        let mut all = vec![("content-type".to_owned(), "application/json".to_owned())];
        all.extend(
            headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        Self {
            status,
            headers: all,
            pieces: vec![Piece::Bytes(body.as_bytes().to_vec())],
            drop_connection: false,
        }
    }

    pub(crate) fn dropped() -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            pieces: Vec::new(),
            drop_connection: true,
        }
    }
}

/// A running fixture server.
pub(crate) struct FixtureServer {
    pub(crate) origin: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: Option<JoinHandle<()>>,
}

impl FixtureServer {
    pub(crate) fn start(responses: Vec<Scripted>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture binds");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                if let Some(request) = read_request(&mut stream) {
                    recorded.lock().expect("requests").push(request);
                }
                if response.drop_connection {
                    drop(stream);
                    continue;
                }
                write_response(&mut stream, response);
            }
        });
        Self {
            origin,
            requests,
            handle: Some(handle),
        }
    }

    pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("requests").clone()
    }

    pub(crate) fn join(mut self) -> Vec<RecordedRequest> {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("fixture thread");
        }
        self.requests()
    }
}

/// A release handle for a [`Piece::Wait`].
pub(crate) fn gate() -> (Sender<()>, Piece) {
    let (sender, receiver) = channel();
    (sender, Piece::Wait(receiver))
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(index) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect::<Vec<_>>();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while data.len() < header_end + length {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
    }
    Some(RecordedRequest {
        target,
        headers,
        body: data[header_end..(header_end + length).min(data.len())].to_vec(),
    })
}

fn write_response(stream: &mut TcpStream, response: Scripted) {
    let mut head = format!("HTTP/1.1 {} Fixture\r\n", response.status);
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("transfer-encoding: chunked\r\nconnection: close\r\n\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();
    for piece in response.pieces {
        match piece {
            Piece::Bytes(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                let chunk = format!("{:x}\r\n", bytes.len());
                if stream.write_all(chunk.as_bytes()).is_err()
                    || stream.write_all(&bytes).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
                let _ = stream.flush();
            }
            Piece::Wait(receiver) => {
                let _ = receiver.recv();
            }
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

/// Encode SSE records as `event: <name>\ndata: <json>\n\n`.
pub(crate) fn sse(events: &[(&str, String)]) -> Vec<u8> {
    let mut body = String::new();
    for (event, data) in events {
        body.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    }
    body.into_bytes()
}
