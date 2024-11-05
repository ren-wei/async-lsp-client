use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{ChildStdin, ChildStdout},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tower_lsp::jsonrpc::{Request, Response};
use tracing::debug;

pub async fn get_message(stdout: &mut ChildStdout) -> Option<Message> {
    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;

    loop {
        let mut byte = [0];
        if stdout.read_exact(&mut byte).await.is_err() {
            return None;
        }
        headers.push(byte[0]);

        // Check if we've reached the end of the headers (double CRLF)
        if headers.ends_with(b"\r\n\r\n") {
            let headers_str = String::from_utf8_lossy(&headers);
            for line in headers_str.lines() {
                if line.starts_with("Content-Length:") {
                    let parts: Vec<&str> = line.splitn(2, ':').collect();
                    if parts.len() > 1 {
                        let length_str = parts[1].trim();
                        content_length = Some(length_str.parse().unwrap());
                        break;
                    }
                }
            }
            break;
        }
    }

    let content_length = content_length.expect("Failed to find Content-Length header");

    let mut body = vec![0u8; content_length];
    stdout.read_exact(&mut body).await.unwrap();

    let value: Map<String, Value> = serde_json::from_slice(&body).unwrap();
    if cfg!(feature = "tracing") {
        debug!("<==== {}", String::from_utf8(body).unwrap());
    }
    if value.contains_key("method") {
        if value.contains_key("id") {
            let request: Request = serde_json::from_value(Value::Object(value)).unwrap();
            Some(Message::Request(request))
        } else {
            let notification: NotificationMessage =
                serde_json::from_value(Value::Object(value)).unwrap();
            Some(Message::Notification(notification))
        }
    } else {
        let response: Response = serde_json::from_value(Value::Object(value)).unwrap();
        Some(Message::Response(response))
    }
}

pub async fn send_message(message: Value, stdin: &mut ChildStdin) {
    let request_str = message.to_string();
    if cfg!(feature = "tracing") {
        debug!("====> {}", request_str);
    }
    let content_length = request_str.len();
    let content = format!("Content-Length: {}\r\n\r\n{}", content_length, request_str);
    stdin.write_all(content.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(NotificationMessage),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct NotificationMessage {
    pub jsonrpc: String,
    pub method: String,
    pub params: Option<serde_json::Value>,
}
