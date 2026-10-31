#![allow(dead_code)]

use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

#[derive(Clone, Debug)]
pub struct FakeConfig {
    pub models: String,
    pub chat: String,
    pub transcription: String,
    pub chat_status: u16,
    pub chat_delay: Duration,
}

impl FakeConfig {
    #[must_use]
    pub fn chat(models: &[&str], chat: impl Into<String>) -> Self {
        Self {
            models: models_fixture(models),
            chat: chat.into(),
            transcription: r#"{"text":""}"#.to_owned(),
            chat_status: 200,
            chat_delay: Duration::ZERO,
        }
    }
}

#[must_use]
pub fn models_fixture(ids: &[&str]) -> String {
    serde_json::json!({
        "object": "list",
        "data": ids
            .iter()
            .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "local"}))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

#[must_use]
pub fn text_reply(content: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-local",
        "object": "chat.completion",
        "model": "lfm3-1.7b",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12},
    })
    .to_string()
}

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub path: String,
    pub body: String,
}

pub struct FakeLlamaServer {
    url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    max_active: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

impl FakeLlamaServer {
    pub async fn start(config: FakeConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake server");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let config = Arc::new(config);
        let handle = tokio::spawn({
            let requests = Arc::clone(&requests);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let requests = Arc::clone(&requests);
                    let active = Arc::clone(&active);
                    let max_active = Arc::clone(&max_active);
                    let config = Arc::clone(&config);
                    tokio::spawn(async move {
                        let Ok((path, body)) = read_request(&mut socket).await else {
                            return;
                        };
                        requests.lock().expect("request log").push(RecordedRequest {
                            path: path.clone(),
                            body,
                        });
                        let (status, response_body, delay) = if path.contains("/v1/models") {
                            (200, config.models.clone(), Duration::ZERO)
                        } else if path.contains("/v1/audio/transcriptions") {
                            (200, config.transcription.clone(), Duration::ZERO)
                        } else {
                            (config.chat_status, config.chat.clone(), config.chat_delay)
                        };
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_active.fetch_max(current, Ordering::SeqCst);
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        active.fetch_sub(1, Ordering::SeqCst);
                        let _ = write_response(&mut socket, status, &response_body).await;
                    });
                }
            }
        });
        Self {
            url,
            requests,
            max_active,
            handle,
        }
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("request log").clone()
    }

    #[must_use]
    pub fn chat_requests(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.path.contains("/v1/chat/completions"))
            .collect()
    }

    #[must_use]
    pub fn max_concurrency(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }
}

impl Drop for FakeLlamaServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn read_request(socket: &mut TcpStream) -> io::Result<(String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&buffer[..end]).to_string();
            let content_length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let body_start = end + 4;
            while buffer.len() < body_start + content_length {
                let read = socket.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let path = header
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_owned();
            let body =
                String::from_utf8_lossy(buffer.get(body_start..).unwrap_or_default()).to_string();
            return Ok((path, body));
        }
    }
    Ok(("/".to_owned(), String::new()))
}

async fn write_response(socket: &mut TcpStream, status: u16, body: &str) -> io::Result<()> {
    let reason = if status == 200 { "OK" } else { "ERROR" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.shutdown().await
}
