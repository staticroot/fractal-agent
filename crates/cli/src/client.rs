use std::path::PathBuf;

use fractal_protocol::messages::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub fn socket_path() -> PathBuf {
    std::env::var_os("FRACTAL_AGENT_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/fractal-agent/agent.sock"))
}

pub async fn call(
    request: &Request,
    mut on_progress: impl FnMut(&str),
) -> Result<Response, String> {
    let path = socket_path();
    let stream = UnixStream::connect(&path).await.map_err(|e| {
        format!("cannot reach the agent at {}: {e}", path.display())
    })?;
    let (read, mut write) = stream.into_split();

    let mut line = serde_json::to_vec(request).expect("Request serializes");
    line.push(b'\n');
    write.write_all(&line).await.map_err(|e| e.to_string())?;
    write.flush().await.map_err(|e| e.to_string())?;

    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        match serde_json::from_str::<Response>(&line) {
            Ok(Response::Progress { line }) => on_progress(&line),
            Ok(Response::Error { message }) => return Err(message),
            Ok(terminal) => return Ok(terminal),
            Err(e) => return Err(format!("cannot read the agent's answer: {e}")),
        }
    }
    Err("the agent closed the connection without answering".to_string())
}

pub async fn send(request: &Request) -> Result<Response, String> {
    call(request, |_| {}).await
}
