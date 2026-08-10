//! Turn one request into a stream of response lines. Reads and the generation
//! record touch the (blocking, non-`Sync`) database on a blocking task; the
//! trigger calls are async. The agent keeps no state between `BeginActivation`
//! and `CompleteActivation`; the principal carries the context back.

use std::path::Path;

use fractal_core::builds::NewBuild;
use fractal_core::config::{Model, Value};
use fractal_core::diff::SemanticDiff;
use fractal_core::evidence::Evidence;
use fractal_core::generations::{Generation, Generations, Kind, NewGeneration, Outcome};
use fractal_core::protocol::{
    self, Challenge, Method, Payload, Request, Response, Solution, StagedChange,
};
use fractal_core::repo::{Author, ConfigVcs, GitRepo};
use fractal_core::staged::Staged;
use fractal_core::system_config::WorkingCopy;
use fractal_core::{catalog, diff, nix, system_config};
use futures_util::StreamExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::build;
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
        Request::Catalog => respond(w, catalog_entries(state).await).await,
        Request::GetOption { key } => respond(w, get_option(state, key).await).await,
        Request::SetOption { key, value, override_staged } => {
            respond(w, set_option(state, peer, key, value, override_staged).await).await
        }
        Request::UnsetOption { key, override_staged } => {
            respond(w, unset_option(state, peer, key, override_staged).await).await
        }
        Request::StagedDiff => respond(w, staged_diff(state).await).await,
        Request::Apply { message, expect } => {
            respond(w, apply(state, peer, message, expect).await).await
        }
        Request::Discard { all } => respond(w, discard(state, peer, all).await).await,
        Request::Build => build(state, w).await,
        Request::Diff { from, to } => respond(w, diff_generations(state, from, to).await).await,
        Request::Evidence { generation } => respond(w, evidence(state, generation).await).await,
        Request::BeginActivation { store_path } => begin_activation(state, store_path, w).await,
        Request::CompleteActivation {
            store_path,
            nonce,
            solution,
        } => complete(state, peer, store_path, nonce, solution, Kind::Forward, w).await,
        Request::BeginRollback { generation } => begin_rollback(state, generation, w).await,
        Request::CompleteRollback {
            generation,
            nonce,
            solution,
        } => match target_of(state, generation).await {
            Ok(store_path) => {
                complete(state, peer, store_path, nonce, solution, Kind::Rollback, w).await
            }
            Err(message) => write(w, &Response::Error { message }).await,
        },
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

async fn catalog_entries(state: &AppState) -> Result<Response, String> {
    let catalog = state.catalog.clone();
    blocking(move || {
        let entries = catalog.entries().map_err(|e| e.to_string())?;
        Ok(Response::Catalog { entries })
    })
    .await
}

/// The staged layer comes from the working copy this agent owns; the rest from
/// the provider, so a device that cannot evaluate answers the same question.
async fn get_option(state: &AppState, key: String) -> Result<Response, String> {
    let catalog = state.catalog.clone();
    let staged = with_config(state, {
        let key = key.clone();
        move |config| Ok(config.get(&key))
    })
    .await?;
    let read = blocking(move || catalog.read(&key, staged).map_err(|e| e.to_string())).await?;
    Ok(Response::OptionValue(Box::new(read)))
}

/// Opens the configuration on first use and refreshes it before handing it over.
async fn with_config<T, F>(state: &AppState, f: F) -> Result<T, String>
where
    F: FnOnce(&mut WorkingCopy<GitRepo>) -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let dir = state.paths.config_dir();
    let config = state.config.clone();
    blocking(move || {
        let mut held = config.lock().unwrap();
        if held.is_none() {
            let repo = GitRepo::open_or_init(&dir).map_err(|e| e.to_string())?;
            *held = Some(WorkingCopy::open(repo).map_err(|e| e.to_string())?);
        }
        let working = held.as_mut().expect("just opened");
        // A refresh that fails leaves a stale model behind, so drop it and let
        // the next request reopen from disk.
        if let Err(e) = working.refresh() {
            *held = None;
            return Err(e.to_string());
        }
        f(working)
    })
    .await
}

async fn set_option(
    state: &AppState,
    peer: Peer,
    key: String,
    value: Value,
    override_staged: bool,
) -> Result<Response, String> {
    validate(&key, &value)?;
    edit_model(state, peer, key, override_staged, move |model, key| {
        model.set(key, value).map(|_| ())
    })
    .await
}

async fn unset_option(
    state: &AppState,
    peer: Peer,
    key: String,
    override_staged: bool,
) -> Result<Response, String> {
    edit_model(state, peer, key, override_staged, move |model, key| {
        model.remove(key);
        Ok(())
    })
    .await
}

/// Authorship comes from the staging table: the file records values only.
async fn staged_diff(state: &AppState) -> Result<Response, String> {
    let staged = state.staged.clone();
    with_config(state, move |config| {
        let changes = attributed(config, &staged.lock().unwrap())?;
        let fingerprint = protocol::fingerprint(&changes);
        Ok(Response::StagedDiff { changes, fingerprint })
    })
    .await
}

fn attributed(
    config: &WorkingCopy<GitRepo>,
    staged: &Staged,
) -> Result<Vec<StagedChange>, String> {
    let authors = staged.all().map_err(|e| e.to_string())?;
    Ok(config
        .staged_changes()
        .into_iter()
        .map(|change| StagedChange {
            staged_by: authors.get(&change.key).copied(),
            change,
        })
        .collect())
}

/// Takes in the whole working copy, not just the applier's own edits: the system
/// configuration is one entity, and a partial one would need a second working
/// copy for the remainder. The applier authors; the rest are co-authors.
async fn apply(
    state: &AppState,
    peer: Peer,
    message: Option<String>,
    expect: Option<String>,
) -> Result<Response, String> {
    let staged = state.staged.clone();
    with_config(state, move |config| {
        if !config.vcs().is_dirty().map_err(|e| e.to_string())? {
            return Ok(Response::Applied { commit: None });
        }
        {
            let staged = staged.lock().unwrap();
            let changes = attributed(config, &staged)?;
            match &expect {
                Some(seen) => {
                    let now = protocol::fingerprint(&changes);
                    if *seen != now {
                        return Err(
                            "the staged changes moved since you read them; read them again"
                                .to_string(),
                        );
                    }
                }
                // Safe only when there is nothing of anyone else's to have missed.
                None => {
                    if let Some(other) = changes
                        .iter()
                        .find(|c| c.staged_by.is_some_and(|uid| uid != peer.uid))
                    {
                        return Err(format!(
                            "uid {} has staged changes too; read them and apply with their fingerprint",
                            other.staged_by.expect("just matched")
                        ));
                    }
                }
            }
        }
        // Formatting is cosmetic, so it waits until the file is about to become
        // history rather than running on every keystroke's worth of edit.
        format(config.vcs());
        config.restamp();

        let staged = staged.lock().unwrap();
        let coauthors: Vec<_> = staged
            .contributors()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|uid| *uid != peer.uid)
            .map(Author::for_uid)
            .collect();

        let message = message.unwrap_or_else(|| "Apply staged configuration".to_string());
        let commit = config
            .vcs()
            .commit_all(&message, &Author::for_uid(peer.uid), &coauthors)
            .map_err(|e| e.to_string())?;
        config.applied(Some(commit.clone()));
        staged.clear().map_err(|e| e.to_string())?;
        Ok(Response::Applied { commit: Some(commit) })
    })
    .await
}

/// Defaults to the caller's own keys.
async fn discard(state: &AppState, peer: Peer, all: bool) -> Result<Response, String> {
    let staged = state.staged.clone();
    with_config(state, move |config| {
        let staged = staged.lock().unwrap();
        if all {
            config.discard_all().map_err(|e| e.to_string())?;
            staged.clear().map_err(|e| e.to_string())?;
        } else {
            let mine = staged.keys_of(peer.uid).map_err(|e| e.to_string())?;
            config.discard_keys(&mine).map_err(|e| e.to_string())?;
            for key in &mine {
                staged.release(key).map_err(|e| e.to_string())?;
            }
        }
        Ok(Response::Ok)
    })
    .await
}

/// Every staging edit shares this path, under the configuration lock, so two
/// concurrent edits cannot both read the same model and lose one write.
async fn edit_model<F>(
    state: &AppState,
    peer: Peer,
    key: String,
    override_staged: bool,
    mutate: F,
) -> Result<Response, String>
where
    F: FnOnce(&mut Model, &str) -> fractal_core::error::Result<()> + Send + 'static,
{
    let staged = state.staged.clone();
    with_config(state, move |config| {
        // Claim first: a refused claim must not leave the file changed.
        staged
            .lock()
            .unwrap()
            .claim(&key, peer.uid, override_staged)
            .map_err(|e| e.to_string())?;

        config
            .edit(|model| mutate(model, &key))
            .map_err(|e| e.to_string())?;
        Ok(Response::Ok)
    })
    .await
}

/// Cosmetically format the generated module in place. Non-fatal: the serializer
/// already emits valid Nix, so a missing formatter or a flake that cannot yet
/// evaluate must not fail an apply. Runs only here, because `nix fmt` evaluates
/// the flake to find the formatter and that has no business in a staging edit.
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

/// Both halves are derived rather than stored, so they recompute to the same
/// answer at any later time.
async fn diff_generations(state: &AppState, from: i64, to: i64) -> Result<Response, String> {
    let gens = state.generations.clone();
    let dir = state.paths.config_dir();
    blocking(move || {
        let gens = gens.lock().unwrap();
        let a = generation(&gens, from)?;
        let b = generation(&gens, to)?;
        Ok(Response::Diff(Box::new(semantic_diff(&dir, &a, &b)?)))
    })
    .await
}

async fn evidence(state: &AppState, id: i64) -> Result<Response, String> {
    let gens = state.generations.clone();
    let dir = state.paths.config_dir();
    blocking(move || {
        let gens = gens.lock().unwrap();
        let this = generation(&gens, id)?;
        // Activation lineage, not git parentage: the two diverge on a rollback.
        let change = match this.parent_id {
            Some(parent) => Some(semantic_diff(&dir, &generation(&gens, parent)?, &this)?),
            None => None,
        };
        Ok(Response::Evidence(Box::new(Evidence { generation: this, change })))
    })
    .await
}

fn generation(gens: &Generations, id: i64) -> Result<Generation, String> {
    gens.get(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no generation {id}"))
}

/// A generation whose configuration predates the generated module contributes an
/// empty model rather than an error, so history stays readable across the
/// changeover.
fn semantic_diff(dir: &Path, before: &Generation, after: &Generation) -> Result<SemanticDiff, String> {
    let repo = GitRepo::open_or_init(dir).map_err(|e| e.to_string())?;
    let model_at = |commit: &str| -> Result<_, String> {
        if commit.is_empty() {
            return Ok(Model::new());
        }
        system_config::load_at(&repo, commit).map_err(|e| e.to_string())
    };
    let options = diff::option_diff(
        &model_at(&before.config_commit)?.leaves(),
        &model_at(&after.config_commit)?.leaves(),
    );
    let closure = nix::diff_closures(&before.store_path, &after.store_path)
        .map_err(|e| e.to_string())?;
    Ok(SemanticDiff { options, closure })
}

/// The tree must be clean: a dirty working copy would produce a closure nothing
/// in history accounts for, and the binding this establishes would be a guess.
async fn build<W: AsyncWrite + Unpin>(state: &AppState, w: &mut W) -> std::io::Result<()> {
    let dir = state.paths.config_dir();
    let commit = match blocking({
        let dir = dir.clone();
        move || {
            let repo = GitRepo::open_or_init(&dir).map_err(|e| e.to_string())?;
            if repo.is_dirty().map_err(|e| e.to_string())? {
                return Err("there are staged changes; apply them before building".to_string());
            }
            repo.head()
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "nothing committed to build yet".to_string())
        }
    })
    .await
    {
        Ok(commit) => commit,
        Err(message) => return write(w, &Response::Error { message }).await,
    };

    let gc_root = state.paths.gcroots_dir().join(&commit);
    let log_path = state.paths.logs_dir().join(format!("build-{commit}.log"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // Drain after the loop: lines can arrive between the last poll and the end.
    let running = build::run(dir, gc_root.clone(), log_path, tx);
    tokio::pin!(running);
    let built = loop {
        tokio::select! {
            done = &mut running => break done,
            Some(line) = rx.recv() => write(w, &Response::Progress { line }).await?,
        }
    };
    while let Ok(line) = rx.try_recv() {
        write(w, &Response::Progress { line }).await?;
    }

    let built = match built {
        Ok(built) => built,
        Err(message) => return write(w, &Response::Error { message }).await,
    };

    let rec = NewBuild {
        store_path: built.store_path.clone(),
        config_commit: commit.clone(),
        gc_root: gc_root.to_string_lossy().into_owned(),
        log: built.log,
    };
    let builds = state.builds.clone();
    if let Err(message) = blocking(move || {
        builds.lock().unwrap().record(&rec).map(|_| ()).map_err(|e| e.to_string())
    })
    .await
    {
        return write(w, &Response::Error { message }).await;
    }

    let response = Response::Built {
        store_path: built.store_path,
        config_commit: commit,
    };
    write(w, &response).await
}

async fn begin_activation<W: AsyncWrite + Unpin>(
    state: &AppState,
    store_path: String,
    w: &mut W,
) -> std::io::Result<()> {
    // A path the agent built binds to a configuration; one it already activated
    // is a generation. Anything else has no provenance to record.
    match known_closure(state, store_path.clone()).await {
        Ok(true) => {}
        Ok(false) => {
            let message = format!("{store_path} was not built by this agent");
            return write(w, &Response::Error { message }).await;
        }
        Err(message) => return write(w, &Response::Error { message }).await,
    }

    issue_challenge(state, store_path, w).await
}

/// The agent keeps no state between this and the completion: the principal holds
/// the context, and the nonce-to-path binding lives in the signature.
async fn issue_challenge<W: AsyncWrite + Unpin>(
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

/// Only a generation that actually ran is a place to return to.
async fn target_of(state: &AppState, generation: i64) -> Result<String, String> {
    let gens = state.generations.clone();
    blocking(move || {
        let found = gens
            .lock()
            .unwrap()
            .get(generation)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no generation {generation}"))?;
        match found.outcome {
            Outcome::Success => Ok(found.store_path),
            Outcome::Failed { .. } => {
                Err(format!("generation {generation} never activated successfully"))
            }
        }
    })
    .await
}

async fn begin_rollback<W: AsyncWrite + Unpin>(
    state: &AppState,
    generation: i64,
    w: &mut W,
) -> std::io::Result<()> {
    let store_path = match target_of(state, generation).await {
        Ok(path) => path,
        Err(message) => return write(w, &Response::Error { message }).await,
    };
    // No provenance check: a generation is a closure this agent already
    // activated. Its store path being on disk is what allows an offline rollback.
    issue_challenge(state, store_path, w).await
}

/// Shared by forward activation and rollback, which differ only in the recorded
/// intent.
async fn complete<W: AsyncWrite + Unpin>(
    state: &AppState,
    peer: Peer,
    store_path: String,
    nonce: String,
    solution: Solution,
    kind: Kind,
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

    // A failed switch has no key: the trigger returns one only after it acts.
    let (outcome, verifying_key) = match &result {
        Ok(key) => (Outcome::Success, key.clone()),
        Err(e) => (Outcome::Failed { detail: e.to_string() }, String::new()),
    };
    let rec = record(
        state,
        peer,
        store_path,
        nonce,
        solution.signature,
        verifying_key,
        kind,
        outcome,
    );
    if let Err(e) = rec.await {
        return write(w, &Response::Error { message: e }).await;
    }
    match result {
        Ok(_) => write(w, &Response::Ok).await,
        Err(e) => write(w, &Response::Error { message: e.to_string() }).await,
    }
}

/// Recorded from the trigger's result, never the principal's claim.
///
/// `config_commit` comes from the build that produced this closure, not from
/// HEAD: the two diverge as soon as anything lands between build and activation.
#[allow(clippy::too_many_arguments)]
async fn record(
    state: &AppState,
    peer: Peer,
    store_path: String,
    nonce: String,
    signature: String,
    verifying_key: String,
    kind: Kind,
    outcome: Outcome,
) -> Result<(), String> {
    let gens = state.generations.clone();
    let builds = state.builds.clone();
    let config_dir = state.paths.config_dir();
    blocking(move || {
        let build = builds.lock().unwrap().by_store_path(&store_path).ok().flatten();
        let config_commit = match &build {
            Some(b) => b.config_commit.clone(),
            None if config_dir.join(".git").exists() => GitRepo::open_or_init(&config_dir)
                .and_then(|r| r.head())
                .ok()
                .flatten()
                .unwrap_or_default(),
            None => String::new(),
        };
        let build_log = build.and_then(|b| b.log);
        let gens = gens.lock().unwrap();
        let parent_id = gens.latest_success().ok().flatten().map(|g| g.id);
        let rec = NewGeneration {
            store_path,
            config_commit,
            parent_id,
            kind,
            description: String::new(),
            actor: peer.actor(),
            verifying_key,
            signature,
            burned_nonce: nonce,
            outcome,
            policy_version: None,
            build_log,
            activation_log: None,
        };
        gens.record(&rec).map(|_| ()).map_err(|e| e.to_string())
    })
    .await
}

/// Whether the agent has provenance for this closure: it built it, or it has
/// activated it before.
async fn known_closure(state: &AppState, store_path: String) -> Result<bool, String> {
    let builds = state.builds.clone();
    let gens = state.generations.clone();
    blocking(move || {
        if builds
            .lock()
            .unwrap()
            .by_store_path(&store_path)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Ok(true);
        }
        gens.lock()
            .unwrap()
            .has_store_path(&store_path)
            .map_err(|e| e.to_string())
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
