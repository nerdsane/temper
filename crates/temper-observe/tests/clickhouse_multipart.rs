//! ARN-174: exercise the real reqwest multipart request over a TCP socket.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use temper_observe::{ClickHouseStore, ObservabilityStore, SqlParam};

async fn capture_request(mut stream: TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut expected_len = None;

    loop {
        let read = stream.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "client closed before sending a complete request");
        request.extend_from_slice(&chunk[..read]);

        if expected_len.is_none()
            && let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
        {
            let headers = std::str::from_utf8(&request[..header_end]).expect("ASCII headers");
            let content_len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| {
                        value
                            .trim()
                            .parse::<usize>()
                            .expect("numeric content length")
                    })
                })
                .expect("multipart request has a content length");
            expected_len = Some(header_end + 4 + content_len);
        }

        if expected_len.is_some_and(|length| request.len() >= length) {
            break;
        }
    }

    let body = "{\"service\":\"safe\"}\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write response");
    request
}

#[tokio::test]
async fn attacker_value_is_only_in_multipart_parameter_field() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind request capture socket");
    let address = listener.local_addr().expect("capture socket address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept request");
        capture_request(stream).await
    });

    let attack = "x\\' OR 1=1; DROP TABLE otel_traces; -- $2";
    let store = ClickHouseStore::new(format!("http://{address}"));
    let result = store
        .query_spans(
            "SELECT service FROM otel_traces WHERE service = $1",
            &[SqlParam::String(attack.to_string())],
        )
        .await
        .expect("mock ClickHouse request succeeds");
    assert_eq!(result.len(), 1);

    let request = String::from_utf8(server.await.expect("capture task")).expect("UTF-8 request");
    let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP envelope");
    let first_line = headers.lines().next().expect("request line");
    assert_eq!(first_line, "POST /?default_format=JSONEachRow HTTP/1.1");
    assert!(!first_line.contains(attack));
    assert!(headers.to_ascii_lowercase().contains("multipart/form-data"));

    let query_part = body
        .split("\r\n--")
        .find(|part| part.contains("name=\"query\""))
        .expect("query multipart field");
    assert!(query_part.contains("SELECT service FROM otel_traces WHERE service = {p1:String}"));
    assert!(!query_part.contains(attack));

    let parameter_part = body
        .split("\r\n--")
        .find(|part| part.contains("name=\"param_p1\""))
        .expect("typed parameter multipart field");
    assert!(parameter_part.contains(attack));
}
