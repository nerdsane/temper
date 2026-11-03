//! A local OTLP/HTTP listener that records every request it receives.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// One HTTP request as the listener received it.
#[derive(Clone, Debug)]
pub struct Received {
    pub path: String,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Received {
    /// The value of a header, or the empty string when it is absent.
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_default()
    }
}

/// Accepts OTLP/HTTP exports on a local port and answers each with success.
pub struct Listener {
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Listener {
    pub fn start() -> Self {
        Self::start_resetting(0)
    }

    /// A listener that closes each of the first `resets` connections as
    /// soon as it accepts it, without reading or answering, as the
    /// collector's end does to a new process's first sends around a handoff.
    pub fn start_resetting(resets: usize) -> Self {
        let socket = TcpListener::bind(("127.0.0.1", 0)).expect("bind the OTLP listener");
        let port = socket.local_addr().expect("listener address").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        std::thread::spawn(move || {
            for (accepted, stream) in socket.incoming().flatten().enumerate() {
                if accepted < resets {
                    drop(stream);
                    continue;
                }
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || serve_connection(stream, &sink));
            }
        });
        Self { port, received }
    }

    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request received so far, in arrival order.
    pub fn received(&self) -> Vec<Received> {
        self.received.lock().expect("listener lock").clone()
    }
}

fn serve_connection(stream: TcpStream, sink: &Mutex<Vec<Received>>) {
    let mut reader = BufReader::new(stream);
    // A request is recorded before it is answered, so once the exporter has
    // its response the request is visible to the test.
    while let Some(request) = read_request(&mut reader) {
        sink.lock().expect("listener lock").push(request);
        let response =
            b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n";
        if reader.get_mut().write_all(response).is_err() {
            return;
        }
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Received> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let path = request_line.split_whitespace().nth(1)?.to_string();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    let mut request = Received {
        path,
        headers,
        body: Vec::new(),
    };
    let length = request
        .header("content-length")
        .parse::<usize>()
        .expect("the exporter sends a content-length");
    request.body.resize(length, 0);
    reader.read_exact(&mut request.body).ok()?;
    Some(request)
}
