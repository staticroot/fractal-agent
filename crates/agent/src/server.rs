//! The socket server: accept principals, frame requests as one JSON object per
//! line, and hand each to the dispatcher. One connection may carry several
//! requests in sequence; each is answered in full before the next is read.

use fractal_core::protocol::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::handler;
use crate::peer::Peer;
use crate::state::AppState;

pub async fn serve(state: AppState, listener: UnixListener) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(&state, stream).await {
                tracing::warn!("connection ended: {e}");
            }
        });
    }
}

async fn handle_connection(state: &AppState, stream: UnixStream) -> std::io::Result<()> {
    let peer = Peer::from_stream(&stream)?;
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => handler::handle(state, peer, req, &mut write).await?,
            Err(e) => {
                let resp = Response::Error { message: format!("malformed request: {e}") };
                let mut bytes = serde_json::to_vec(&resp).expect("Response serializes");
                bytes.push(b'\n');
                write.write_all(&bytes).await?;
                write.flush().await?;
            }
        }
    }
}
