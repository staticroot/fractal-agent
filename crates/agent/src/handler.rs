//! Turn one request into a stream of response lines. Reads and the generation
//! record touch the (blocking, non-`Sync`) database on a blocking task; the
//! trigger calls are async. The agent keeps no state between `BeginActivation`
//! and `CompleteActivation`; the principal carries the context back.

use fractal_core::generations::{Kind, NewGeneration, Outcome};
use fractal_core::protocol::{Challenge, Method, Payload, Request, Response, Solution};
use fractal_core::repo::{ConfigVcs, GitRepo};
use futures_util::StreamExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::peer::Peer;
use crate::state::AppState;

pub async fn handle<W: AsyncWrite + Unpin>(
    state: &AppState,
    peer: Peer,
    req: Request,
    w: &mut W,
) -> std::io::Result<()> {
    match req {
        Request::Ping => write(w, &Response::Pong).await,
        Request::History => match history(state).await {
            Ok(generations) => write(w, &Response::Generations { generations }).await,
            Err(message) => write(w, &Response::Error { message }).await,
        },
        Request::Current => match current(state).await {
            Ok(generation) => {
                write(w, &Response::Current { generation: generation.map(Box::new) }).await
            }
            Err(message) => write(w, &Response::Error { message }).await,
        },
        Request::BeginActivation { store_path } => begin_activation(state, store_path, w).await,
        Request::CompleteActivation {
            store_path,
            nonce,
            solution,
        } => complete_activation(state, peer, store_path, nonce, solution, w).await,
    }
}

/// Serialize one response and terminate the line. `Response` is always
/// serializable, so the encode cannot fail.
async fn write<W: AsyncWrite + Unpin>(w: &mut W, resp: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(resp).expect("Response serializes");
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

async fn history(state: &AppState) -> Result<Vec<fractal_core::generations::Generation>, String> {
    let gens = state.generations.clone();
    blocking(move || gens.lock().unwrap().list().map_err(|e| e.to_string())).await
}

async fn current(
    state: &AppState,
) -> Result<Option<fractal_core::generations::Generation>, String> {
    let gens = state.generations.clone();
    blocking(move || gens.lock().unwrap().latest_success().map_err(|e| e.to_string())).await
}

async fn begin_activation<W: AsyncWrite + Unpin>(
    state: &AppState,
    store_path: String,
    w: &mut W,
) -> std::io::Result<()> {
    match state.trigger.issue_nonce().await {
        Ok(nonce) => {
            let challenge = Challenge {
                method: Method::LocalKey,
                payload: Payload::Activation { store_path, nonce },
            };
            write(w, &Response::Challenge(challenge)).await
        }
        Err(e) => write(w, &Response::Error { message: e.to_string() }).await,
    }
}

async fn complete_activation<W: AsyncWrite + Unpin>(
    state: &AppState,
    peer: Peer,
    store_path: String,
    nonce: String,
    solution: Solution,
    w: &mut W,
) -> std::io::Result<()> {
    // Forward the trigger's progress signal to the principal while the switch
    // runs. Scoped so the switch future's borrows of the inputs are released
    // before they are moved into the record below.
    let result = {
        let mut progress = match state.trigger.receive_progress().await {
            Ok(stream) => stream,
            Err(e) => return write(w, &Response::Error { message: e.to_string() }).await,
        };
        let switch = state
            .trigger
            .switch_to_store_path(&store_path, &solution.signature, &nonce);
        tokio::pin!(switch);
        loop {
            tokio::select! {
                done = &mut switch => break done,
                Some(signal) = progress.next() => {
                    if let Ok(args) = signal.args() {
                        write(w, &Response::Progress { line: args.line().to_string() }).await?;
                    }
                }
            }
        }
    };

    let outcome = match &result {
        Ok(()) => Outcome::Success,
        Err(e) => Outcome::Failed { detail: e.to_string() },
    };
    if let Err(e) = record(state, peer, store_path, nonce, solution.signature, outcome).await {
        return write(w, &Response::Error { message: e }).await;
    }
    match result {
        Ok(()) => write(w, &Response::Ok).await,
        Err(e) => write(w, &Response::Error { message: e.to_string() }).await,
    }
}

/// Record the activation from the *trigger's* result, never the principal's
/// claim. `config_commit` is the config repo's HEAD, the configuration that
/// produced this closure; empty until the repo exists.
async fn record(
    state: &AppState,
    peer: Peer,
    store_path: String,
    nonce: String,
    signature: String,
    outcome: Outcome,
) -> Result<(), String> {
    let gens = state.generations.clone();
    let config_dir = state.paths.config_dir();
    blocking(move || {
        let config_commit = if config_dir.join(".git").exists() {
            GitRepo::open_or_init(&config_dir)
                .and_then(|r| r.head())
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            String::new()
        };
        let gens = gens.lock().unwrap();
        let parent_id = gens.latest_success().ok().flatten().map(|g| g.id);
        let rec = NewGeneration {
            store_path,
            config_commit,
            parent_id,
            kind: Kind::Forward,
            description: String::new(),
            actor: peer.actor(),
            consent_event: "pkexec local-key".to_string(),
            signature,
            burned_nonce: nonce,
            outcome,
            policy_version: None,
            build_log: None,
            activation_log: None,
        };
        gens.record(&rec).map(|_| ()).map_err(|e| e.to_string())
    })
    .await
}

/// Run a blocking closure on the blocking pool, flattening the join error.
async fn blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}
