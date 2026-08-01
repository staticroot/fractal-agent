//! Turn one request into a stream of response lines. Reads and the generation
//! record touch the (blocking, non-`Sync`) database on a blocking task; the
//! trigger calls are async. The agent keeps no state between `BeginActivation`
//! and `CompleteActivation`; the principal carries the context back.

use fractal_core::config::{Model, Value};
use fractal_core::generations::{Kind, NewGeneration, Outcome};
use fractal_core::protocol::{Challenge, Method, Payload, Request, Response, Solution};
use fractal_core::repo::{ConfigVcs, GitRepo};
use fractal_core::{catalog, nix, system_config};
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
        Request::History => respond(w, history(state).await).await,
        Request::Current => respond(w, current(state).await).await,
        Request::Catalog => {
            respond(w, Ok(Response::Catalog { entries: catalog::standalone() })).await
        }
        Request::GetOption { key } => respond(w, get_option(state, key).await).await,
        Request::SetOption { key, value } => respond(w, set_option(state, key, value).await).await,
        Request::UnsetOption { key } => respond(w, unset_option(state, key).await).await,
        Request::StagedDiff => respond(w, staged_diff(state).await).await,
        Request::Apply { message } => respond(w, apply(state, message).await).await,
        Request::Discard => respond(w, discard(state).await).await,
        Request::BeginActivation { store_path } => begin_activation(state, store_path, w).await,
        Request::CompleteActivation {
            store_path,
            nonce,
            solution,
        } => complete_activation(state, peer, store_path, nonce, solution, w).await,
    }
}

async fn respond<W: AsyncWrite + Unpin>(
    w: &mut W,
    result: Result<Response, String>,
) -> std::io::Result<()> {
    match result {
        Ok(resp) => write(w, &resp).await,
        Err(message) => write(w, &Response::Error { message }).await,
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

async fn history(state: &AppState) -> Result<Response, String> {
    let gens = state.generations.clone();
    let generations =
        blocking(move || gens.lock().unwrap().list().map_err(|e| e.to_string())).await?;
    Ok(Response::Generations { generations })
}

async fn current(state: &AppState) -> Result<Response, String> {
    let gens = state.generations.clone();
    let generation =
        blocking(move || gens.lock().unwrap().latest_success().map_err(|e| e.to_string())).await?;
    Ok(Response::Current { generation: generation.map(Box::new) })
}

/// The staged value of one option straight from the working-copy model.
async fn get_option(state: &AppState, key: String) -> Result<Response, String> {
    let dir = state.paths.config_dir();
    blocking(move || {
        let repo = GitRepo::open_or_init(&dir).map_err(|e| e.to_string())?;
        let model = system_config::load(&repo).map_err(|e| e.to_string())?;
        Ok(Response::OptionValue { value: model.get(&key).cloned(), key })
    })
    .await
}

async fn set_option(state: &AppState, key: String, value: Value) -> Result<Response, String> {
    validate(&key, &value)?;
    edit_model(state, move |model| {
        model.set(&key, value);
    })
    .await
}

async fn unset_option(state: &AppState, key: String) -> Result<Response, String> {
    edit_model(state, move |model| {
        model.remove(&key);
    })
    .await
}

async fn staged_diff(state: &AppState) -> Result<Response, String> {
    let dir = state.paths.config_dir();
    blocking(move || {
        let repo = GitRepo::open_or_init(&dir).map_err(|e| e.to_string())?;
        let changes = system_config::staged_diff(&repo).map_err(|e| e.to_string())?;
        Ok(Response::StagedDiff { changes })
    })
    .await
}

async fn apply(state: &AppState, message: Option<String>) -> Result<Response, String> {
    let dir = state.paths.config_dir();
    blocking(move || {
        let repo = GitRepo::open_or_init(&dir).map_err(|e| e.to_string())?;
        if !repo.is_dirty().map_err(|e| e.to_string())? {
            return Ok(Response::Applied { commit: None });
        }
        let message = message.unwrap_or_else(|| "Apply staged configuration".to_string());
        let commit = repo.commit_all(&message).map_err(|e| e.to_string())?;
        Ok(Response::Applied { commit: Some(commit) })
    })
    .await
}

async fn discard(state: &AppState) -> Result<Response, String> {
    let dir = state.paths.config_dir();
    blocking(move || {
        let repo = GitRepo::open_or_init(&dir).map_err(|e| e.to_string())?;
        system_config::discard(&repo).map_err(|e| e.to_string())?;
        format(&repo);
        Ok(Response::Ok)
    })
    .await
}

/// Load the working-copy model, mutate it, write it back, and re-format the
/// projection. Every staging edit shares this path so they cannot drift.
async fn edit_model<F>(state: &AppState, mutate: F) -> Result<Response, String>
where
    F: FnOnce(&mut Model) + Send + 'static,
{
    let dir = state.paths.config_dir();
    blocking(move || {
        let repo = GitRepo::open_or_init(&dir).map_err(|e| e.to_string())?;
        let mut model = system_config::load(&repo).map_err(|e| e.to_string())?;
        mutate(&mut model);
        system_config::write(&repo, &model).map_err(|e| e.to_string())?;
        format(&repo);
        Ok(Response::Ok)
    })
    .await
}

/// Cosmetically format the generated module in place. Non-fatal: the serializer
/// already emits valid Nix, so a missing formatter or a flake that cannot yet
/// evaluate must not fail a staging edit.
fn format(repo: &GitRepo) {
    let file = repo.workdir().join(system_config::NIX_FILE);
    if let Err(e) = nix::format_file(repo.workdir(), &file) {
        tracing::debug!("skipped formatting {}: {e}", file.display());
    }
}

/// A staged value must name a catalog option and fall within its allowed set.
/// In standalone every entry is unconstrained, so this reduces to "is a real,
/// curated key"; the value's Nix type is checked later, at build time.
fn validate(key: &str, value: &Value) -> Result<(), String> {
    let entry = catalog::standalone()
        .into_iter()
        .find(|e| e.key == key)
        .ok_or_else(|| format!("unknown option: {key}"))?;
    if !entry.constraint.allows(value) {
        return Err(format!("value not permitted for {key}"));
    }
    Ok(())
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

    // A failed switch has no verifying key to record, because the trigger only
    // returns one once it has both verified and acted.
    let (outcome, verifying_key) = match &result {
        Ok(key) => (Outcome::Success, key.clone()),
        Err(e) => (Outcome::Failed { detail: e.to_string() }, String::new()),
    };
    let rec = record(state, peer, store_path, nonce, solution.signature, verifying_key, outcome);
    if let Err(e) = rec.await {
        return write(w, &Response::Error { message: e }).await;
    }
    match result {
        Ok(_) => write(w, &Response::Ok).await,
        Err(e) => write(w, &Response::Error { message: e.to_string() }).await,
    }
}

/// Record the activation from the *trigger's* result, never the principal's
/// claim. That now includes `verifying_key`, which the trigger returns and only
/// the trigger can know. `config_commit` is the config repo's HEAD, the
/// configuration that produced this closure; empty until the repo exists.
#[allow(clippy::too_many_arguments)]
async fn record(
    state: &AppState,
    peer: Peer,
    store_path: String,
    nonce: String,
    signature: String,
    verifying_key: String,
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
            verifying_key,
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
