//! Who is on the other end of the socket, straight from the kernel. In the
//! activation handshake the principal raises its own consent prompt from its own
//! session, so the agent needs no forwarded process reference, only the uid it
//! records as the actor behind a change.

use tokio::net::UnixStream;

/// Kernel-attested identity of the connected principal.
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub uid: u32,
}

impl Peer {
    pub fn from_stream(stream: &UnixStream) -> std::io::Result<Self> {
        Ok(Self {
            uid: stream.peer_cred()?.uid(),
        })
    }

    /// The actor string recorded against a generation.
    pub fn actor(&self) -> String {
        format!("uid:{}", self.uid)
    }
}
