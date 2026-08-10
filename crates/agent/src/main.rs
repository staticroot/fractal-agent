//! fractal-agent: the unprivileged system daemon. It owns the configuration and
//! its history and does every mechanical thing (evaluate, build, diff, record)
//! but holds no key and no authority. Activation is a handshake it brokers: it
//! mints a nonce from the trigger, hands the principal a challenge, relays the
//! solved challenge back to the trigger, and records what the trigger did.

mod build;
mod handler;
mod peer;
mod server;
mod state;
mod trigger;

use std::sync::{Arc, Mutex};

use fractal_core::builds::Builds;
use fractal_core::catalog_local::LocalCatalog;
use fractal_core::generations::Generations;
use fractal_core::staged::Staged;

use crate::state::{AppState, Paths};
use crate::trigger::TriggerProxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();

    let paths = Paths::from_env();
    std::fs::create_dir_all(&paths.state_dir)?;
    std::fs::create_dir_all(paths.logs_dir())?;
    std::fs::create_dir_all(paths.gcroots_dir())?;
    if let Some(parent) = paths.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = zbus::Connection::system().await?;
    let trigger = TriggerProxy::new(&conn).await?;
    let generations = Generations::open(paths.generations_db())?;
    let builds = Builds::open(paths.generations_db())?;
    let staged = Staged::open(paths.generations_db())?;
    let catalog = LocalCatalog::new(paths.config_dir(), build::SYSTEM_PATH);

    let state = AppState {
        paths: Arc::new(paths.clone()),
        trigger,
        generations: Arc::new(Mutex::new(generations)),
        builds: Arc::new(Mutex::new(builds)),
        staged: Arc::new(Mutex::new(staged)),
        catalog: Arc::new(catalog),
        config: Arc::new(Mutex::new(None)),
    };

    // Bind a fresh socket; a stale one from an unclean exit would refuse bind.
    let _ = std::fs::remove_file(&paths.socket);
    let listener = tokio::net::UnixListener::bind(&paths.socket)?;
    tracing::info!(socket = %paths.socket.display(), "fractal-agent listening");

    server::serve(state, listener).await?;
    Ok(())
}
