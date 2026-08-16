//! Turn one request into a stream of response lines. Reads and the generation
//! record touch the (blocking, non-`Sync`) database on a blocking task; the
//! trigger calls are async. The agent keeps no state between `BeginActivation`
//! and `CompleteActivation`; the principal carries the context back.

use std::path::Path;
use std::sync::{Arc, Mutex};

use fractal_core::builds::{Build, Builds, NewBuild};
use fractal_core::config::{Model, Value};
use fractal_core::diff::{OptionChange, SemanticDiff};
use fractal_core::draft::{self, Uid};
use fractal_core::evidence::Evidence;
use fractal_core::generations::{Generation, Generations, Kind, NewGeneration, Outcome};
use fractal_core::protocol::{
    Challenge, DraftChange, Endpoint, Method, Payload, QuarantinedDraft, Request, Response,
    Revision, Solution,
};
use fractal_core::repo::{Author, FRACTAL_REFS, GitRepo};
use fractal_core::system_config::{ModelCache, NIX_FILE};
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
        Request::GetOption { key } => respond(w, get_option(state, peer, key).await).await,
        Request::SetOption { key, value } => {
            respond(w, set_option(state, peer, key, Some(value)).await).await
        }
        Request::UnsetOption { key } => respond(w, set_option(state, peer, key, None).await).await,
        Request::Drafts => respond(w, drafts(state).await).await,
        Request::Discard { keys } => respond(w, discard(state, peer, keys).await).await,
        Request::ListFiles { at } => respond(w, list_files(state, peer, at).await).await,
        Request::ReadFile { at, path } => respond(w, read_file(state, peer, at, path).await).await,
        Request::WriteFile {
            path,
            contents,
            base_digest,
        } => respond(w, write_file(state, peer, path, contents, base_digest).await).await,
        Request::Build { message } => build(state, peer, message, w).await,
        Request::Diff { from, to } => respond(w, diff(state, from, to).await).await,
        Request::Evidence { generation } => respond(w, evidence(state, generation).await).await,
        Request::BeginActivation { commit } => begin_activation(state, peer, commit, w).await,
        Request::CompleteActivation {
            commit,
            nonce,
            solution,
        } => match from_candidate(state, peer, commit).await {
            Ok(target) => complete(state, peer, target, nonce, solution, w).await,
            Err(message) => write(w, &Response::Error { message }).await,
        },
        Request::BeginRollback { generation } => begin_rollback(state, generation, w).await,
        Request::CompleteRollback {
            generation,
            nonce,
            solution,
        } => match from_generation(state, generation).await {
            Ok(target) => complete(state, peer, target, nonce, solution, w).await,
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

/// Hand a blocking section the repository and the models together, always in
/// that order, so nothing can take the two locks the other way round.
async fn with_repo<T, F>(state: &AppState, f: F) -> Result<T, String>
where
    F: FnOnce(&GitRepo, &mut ModelCache) -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let repo = state.repo.clone();
    let models = state.models.clone();
    blocking(move || {
        let repo = repo.lock().unwrap();
        let mut models = models.lock().unwrap();
        f(&repo, &mut models)
    })
    .await
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

async fn history(state: &AppState) -> Result<Response, String> {
    let gens = state.generations.clone();
    let generations = blocking(move || gens.lock().unwrap().list().map_err(err)).await?;
    Ok(Response::Generations { generations })
}

async fn current(state: &AppState) -> Result<Response, String> {
    let gens = state.generations.clone();
    let generation = blocking(move || gens.lock().unwrap().latest_success().map_err(err)).await?;
    Ok(Response::Current { generation: generation.map(Box::new) })
}

async fn catalog_entries(state: &AppState) -> Result<Response, String> {
    let catalog = state.catalog.clone();
    blocking(move || {
        let entries = catalog.entries().map_err(err)?;
        Ok(Response::Catalog { entries })
    })
    .await
}

/// The draft layer is this caller's own; the rest comes from the provider, so a
/// device that cannot evaluate answers the same question. The provider resolves
/// the effective layer against this caller's draft revision, which is what their
/// own apply would produce.
async fn get_option(state: &AppState, peer: Peer, key: String) -> Result<Response, String> {
    let catalog = state.catalog.clone();
    let drafted = draft_layer(state, peer, &key).await?;
    let read = blocking(move || catalog.read(&key, drafted, peer.uid).map_err(err)).await?;
    Ok(Response::OptionValue(Box::new(read)))
}

/// What this principal's draft asks for at `key`. A drafted removal reads as no
/// value, which is the same answer a key nobody drafted gives.
async fn draft_layer(state: &AppState, peer: Peer, key: &str) -> Result<Option<Value>, String> {
    let key = key.to_string();
    with_repo(state, move |repo, models| {
        let changes = draft::changes(repo, models, peer.uid).map_err(err)?;
        Ok(changes.into_iter().find(|(k, _)| *k == key).and_then(|(_, value)| value))
    })
    .await
}

/// Records what this principal wants, in their own draft. It displaces nobody:
/// another principal's draft of the same option stands beside it until one of
/// them is applied.
async fn set_option(
    state: &AppState,
    peer: Peer,
    key: String,
    value: Option<Value>,
) -> Result<Response, String> {
    if let Some(value) = &value {
        validate(&key, value)?;
    }
    // Refused here rather than at the build it would break: the model rejects a
    // value that would swallow a subtree or sit beneath a leaf, and that is
    // worth hearing while the person is still typing.
    with_repo(state, move |repo, models| {
        draft::amend(repo, models, peer.uid, |model| model.apply_change(&key, value)).map_err(err)
    })
    .await?;

    prewarm(state, peer.uid);
    Ok(Response::Ok)
}

/// Start this principal's evaluation now, so their next read is answered from
/// the cache rather than from an evaluator start. Nothing waits on it, and a
/// failure surfaces when they actually ask.
fn prewarm(state: &AppState, uid: Uid) {
    let catalog = state.catalog.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = catalog.warm(uid) {
            tracing::debug!("could not warm the catalog for uid {uid}: {e}");
        }
    });
}

/// Everybody's drafts, read against the configuration that is running. Two
/// entries may name one option, which is why each carries its author.
async fn drafts(state: &AppState) -> Result<Response, String> {
    with_repo(state, move |repo, models| {
        let tip = models.at_opt(repo, repo.head().map_err(err)?.as_deref()).map_err(err)?;
        let mut changes = Vec::new();
        let mut quarantined = Vec::new();
        for author in draft::authors(repo).map_err(err)? {
            for (key, after) in draft::changes(repo, models, author).map_err(err)? {
                changes.push(DraftChange {
                    change: OptionChange { before: tip.get(&key), after, key },
                    author: Some(author),
                });
            }
            // A draft that sits on the tip carries by construction, so only one
            // that does not is worth replaying.
            if draft::is_quarantined(repo, author).map_err(err)? {
                let conflicts = draft::conflicts(repo, models, author).map_err(err)?;
                quarantined.push(QuarantinedDraft { author, conflicts });
            }
        }
        Ok(Response::Drafts { changes, quarantined })
    })
    .await
}

/// Reaches the caller's own draft and no further. Named keys, or the whole draft
/// when none are named.
async fn discard(state: &AppState, peer: Peer, keys: Vec<String>) -> Result<Response, String> {
    with_repo(state, move |repo, models| {
        draft::discard(repo, models, peer.uid, &keys).map_err(err)?;
        Ok(Response::Ok)
    })
    .await
}

/// Which commit a file read resolves against. A principal who holds no draft
/// reads the running configuration, so `Draft` always names something.
async fn revision_of(state: &AppState, peer: Peer, at: Revision) -> Result<String, String> {
    match at {
        Revision::Generation { id } => {
            let gens = state.generations.clone();
            let found = blocking(move || generation(&gens.lock().unwrap(), id)).await?;
            Ok(found.commit)
        }
        Revision::Draft { author } => {
            let uid = author.unwrap_or(peer.uid);
            with_repo(state, move |repo, _| {
                match repo.ref_sha(&draft::draft_ref(uid)).map_err(err)? {
                    Some(sha) => Ok(sha),
                    None => tip(repo),
                }
            })
            .await
        }
        Revision::Commit { commit } => {
            with_repo(state, move |repo, _| match &commit {
                // A reference resolves to what it points at; anything else is
                // already an object id.
                Some(named) => Ok(repo.ref_sha(named).map_err(err)?.unwrap_or_else(|| named.clone())),
                None => tip(repo),
            })
            .await
        }
    }
}

fn tip(repo: &GitRepo) -> Result<String, String> {
    repo.head()
        .map_err(err)?
        .ok_or_else(|| "nothing has been provisioned yet".to_string())
}

async fn list_files(state: &AppState, peer: Peer, at: Revision) -> Result<Response, String> {
    let rev = revision_of(state, peer, at).await?;
    with_repo(state, move |repo, _| {
        let paths = repo.list_blobs(&rev).map_err(err)?.into_keys().collect();
        Ok(Response::Files { paths })
    })
    .await
}

async fn read_file(
    state: &AppState,
    peer: Peer,
    at: Revision,
    path: String,
) -> Result<Response, String> {
    let rev = revision_of(state, peer, at).await?;
    with_repo(state, move |repo, _| {
        let Some(bytes) = repo.read_blob(&rev, &path).map_err(err)? else {
            return Err(format!("no {path} at {rev}"));
        };
        let digest = repo.blob_id(&rev, &path).map_err(err)?.unwrap_or_default();
        let contents = String::from_utf8(bytes).map_err(|_| format!("{path} is not text"))?;
        Ok(Response::FileContents { contents, digest })
    })
    .await
}

/// Lands a whole file in the caller's draft, refusing it where the version they
/// edited is no longer the one they hold.
async fn write_file(
    state: &AppState,
    peer: Peer,
    path: String,
    contents: String,
    base_digest: String,
) -> Result<Response, String> {
    if path == NIX_FILE {
        return Err(format!("{NIX_FILE} is generated; set the option instead"));
    }
    // Bounced back rather than landed, the way visudo refuses a syntax error: a
    // draft that cannot parse would take its own author's reads down.
    if path.ends_with(".nix") {
        nix::parse_check(contents.as_bytes()).map_err(err)?;
    }
    with_repo(state, move |repo, models| {
        let held = match draft::load(repo, models, peer.uid).map_err(err)? {
            Some(draft) => Some(draft.sha),
            None => repo.head().map_err(err)?,
        };
        let current = match &held {
            Some(rev) => repo.blob_id(rev, &path).map_err(err)?.unwrap_or_default(),
            None => String::new(),
        };
        if current != base_digest {
            return Err(format!("{path} has changed since you read it; read it again"));
        }
        draft::amend_file(repo, models, peer.uid, &path, contents.as_bytes()).map_err(err)?;
        Ok(Response::Ok)
    })
    .await
}

/// The candidate is the caller's draft given their message and parented on the
/// tip, kept under a reference of its own because Lix will not fetch a revision
/// nothing covers, and because that reference is what makes it activatable.
///
/// Written even when the caller has drafted nothing, in which case its tree is
/// the tip's. A device that has been provisioned and never applied is exactly
/// that case, and its first apply is what puts the scaffolded configuration into
/// history.
async fn candidate(
    state: &AppState,
    peer: Peer,
    message: Option<String>,
) -> Result<(String, String), String> {
    let uid = peer.uid;
    let (tip, from, generated) = with_repo(state, move |repo, models| {
        let tip = repo
            .head()
            .map_err(err)?
            .ok_or_else(|| "nothing has been provisioned yet".to_string())?;

        draft::carry(repo, models, uid).map_err(err)?;
        if draft::is_quarantined(repo, uid).map_err(err)? {
            let conflicts = draft::conflicts(repo, models, uid).map_err(err)?;
            return Err(format!(
                "the configuration moved underneath your draft; edit it to resolve {}",
                conflicts.join(", ")
            ));
        }

        let from = match draft::load(repo, models, uid).map_err(err)? {
            Some(drafted) => drafted.sha,
            None => tip.clone(),
        };
        let generated = repo.read_blob(&from, NIX_FILE).map_err(err)?.unwrap_or_default();
        Ok((tip, from, generated))
    })
    .await?;

    let flake = {
        let repo = state.repo.lock().unwrap();
        nix::flake_url(repo.path(), "HEAD", &tip)
    };
    let generated = format(flake, generated).await?;

    let message = message.unwrap_or_else(|| "Apply drafted configuration".to_string());
    let gcroots = state.paths.gcroots_dir();
    with_repo(state, move |repo, _| {
        // The formatter evaluates the flake, which is far too long to hold the
        // repository for, so the tip is checked again on the way back in.
        if repo.head().map_err(err)?.as_deref() != Some(tip.as_str()) {
            return Err("the configuration moved while this was being built; build it again".into());
        }
        let commit = repo
            .commit_tree(
                Some(&from),
                Some(&tip),
                &[(NIX_FILE, &generated)],
                &Author::for_uid(uid),
                &message,
            )
            .map_err(err)?;

        let reference = draft::candidate_ref(uid);
        // The candidate this replaces stops being activatable here, so its
        // closure stops being wanted here too.
        if let Some(previous) = repo.ref_sha(&reference).map_err(err)?
            && previous != commit
        {
            release(&gcroots, &previous);
        }
        repo.set_ref(&reference, &commit).map_err(err)?;
        Ok((commit.clone(), nix::flake_url(repo.path(), &reference, &commit)))
    })
    .await
}

/// Cosmetically format the generated module on its way into a candidate.
/// Non-fatal: the serializer already emits valid Nix, so a missing formatter or
/// a flake that cannot yet evaluate must not fail an apply.
///
/// Here and nowhere else, because the formatter is resolved by evaluating the
/// flake and drafting must not touch an evaluator. Every comparison that decides
/// a release or a conflict runs over models, so what this does to the bytes
/// cannot change what a draft means.
async fn format(flake: String, generated: Vec<u8>) -> Result<Vec<u8>, String> {
    blocking(move || match nix::format_bytes(&flake, &generated) {
        Ok(formatted) => Ok(formatted),
        Err(e) => {
            tracing::debug!("skipped formatting {NIX_FILE}: {e}");
            Ok(generated)
        }
    })
    .await
}

/// A drafted value must name a catalog option and fall within its allowed set.
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
async fn diff(state: &AppState, from: Endpoint, to: Endpoint) -> Result<Response, String> {
    let gens = state.generations.clone();
    let builds = state.builds.clone();
    let repo = state.repo.clone();
    blocking(move || {
        let a = {
            let gens = gens.lock().unwrap();
            let builds = builds.lock().unwrap();
            (resolve(&gens, &builds, from)?, resolve(&gens, &builds, to)?)
        };
        Ok(Response::Diff(Box::new(semantic_diff(&repo, &a.0, &a.1)?)))
    })
    .await
}

struct Side {
    store_path: String,
    commit: String,
}

impl From<&Generation> for Side {
    fn from(g: &Generation) -> Self {
        Self {
            store_path: g.store_path.clone(),
            commit: g.commit.clone(),
        }
    }
}

impl From<&Build> for Side {
    fn from(b: &Build) -> Self {
        Self {
            store_path: b.store_path.clone(),
            commit: b.commit.clone(),
        }
    }
}

fn resolve(gens: &Generations, builds: &Builds, at: Endpoint) -> Result<Side, String> {
    match at {
        Endpoint::Generation { id } => Ok((&generation(gens, id)?).into()),
        Endpoint::Running => gens
            .latest_success()
            .map_err(err)?
            .as_ref()
            .map(Side::from)
            .ok_or_else(|| "nothing has been activated yet".to_string()),
        Endpoint::Candidate { commit } => builds
            .by_commit(&commit)
            .map_err(err)?
            .as_ref()
            .map(Side::from)
            .ok_or_else(|| format!("{commit} was not built by this agent")),
    }
}

async fn evidence(state: &AppState, id: i64) -> Result<Response, String> {
    let gens = state.generations.clone();
    let repo = state.repo.clone();
    blocking(move || {
        let (this, parent) = {
            let gens = gens.lock().unwrap();
            let this = generation(&gens, id)?;
            // Activation lineage, not git parentage: the two diverge on a rollback.
            let parent = this.parent_id.map(|p| generation(&gens, p)).transpose()?;
            (this, parent)
        };
        let change = match parent {
            Some(parent) => Some(semantic_diff(&repo, &(&parent).into(), &(&this).into())?),
            None => None,
        };
        Ok(Response::Evidence(Box::new(Evidence { generation: this, change })))
    })
    .await
}

fn generation(gens: &Generations, id: i64) -> Result<Generation, String> {
    gens.get(id).map_err(err)?.ok_or_else(|| format!("no generation {id}"))
}

fn semantic_diff(
    repo: &Arc<Mutex<GitRepo>>,
    before: &Side,
    after: &Side,
) -> Result<SemanticDiff, String> {
    let options = {
        let repo = repo.lock().unwrap();
        let model_at =
            |commit: &str| -> Result<Model, String> { system_config::load_at(&repo, commit).map_err(err) };
        diff::option_diff(&model_at(&before.commit)?.leaves(), &model_at(&after.commit)?.leaves())
    };
    let closure = nix::diff_closures(&before.store_path, &after.store_path).map_err(err)?;
    Ok(SemanticDiff { options, closure })
}

async fn build<W: AsyncWrite + Unpin>(
    state: &AppState,
    peer: Peer,
    message: Option<String>,
    w: &mut W,
) -> std::io::Result<()> {
    let (commit, flake) = match candidate(state, peer, message).await {
        Ok(built) => built,
        Err(message) => return write(w, &Response::Error { message }).await,
    };

    let gc_root = state.paths.gcroots_dir().join(&commit);
    let log_path = state.paths.logs_dir().join(format!("build-{commit}.log"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // Drain after the loop: lines can arrive between the last poll and the end.
    let running = build::run(flake, gc_root.clone(), log_path, tx);
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
        commit: commit.clone(),
        gc_root: gc_root.to_string_lossy().into_owned(),
        log: built.log,
    };
    let builds = state.builds.clone();
    if let Err(message) =
        blocking(move || builds.lock().unwrap().record(&rec).map(|_| ()).map_err(err)).await
    {
        return write(w, &Response::Error { message }).await;
    }

    let response = Response::Built {
        store_path: built.store_path,
        commit,
    };
    write(w, &response).await
}

/// Where an activation is going: the closure the trigger will switch to, the
/// configuration it was built from, and which of the two intents this is.
///
/// Both entry points resolve one of these before anything is signed, and resolve
/// it again before the trigger is called, so nothing is carried across the gap
/// where the principal is at the prompt.
struct Target {
    store_path: String,
    commit: String,
    kind: Kind,
}

/// A commit is activatable exactly while it is the caller's own candidate.
/// Building again supersedes it, and so does anybody else's apply landing,
/// which is the same rule that decides what the repository and the
/// garbage-collection roots keep.
async fn from_candidate(state: &AppState, peer: Peer, commit: String) -> Result<Target, String> {
    let held = with_repo(state, move |repo, _| {
        repo.ref_sha(&draft::candidate_ref(peer.uid)).map_err(err)
    })
    .await?;
    if held.as_deref() != Some(commit.as_str()) {
        return Err(format!("{commit} is not your candidate; build again"));
    }

    let builds = state.builds.clone();
    let build = blocking(move || builds.lock().unwrap().by_commit(&commit).map_err(err)).await?;
    let build = build.ok_or_else(|| "your candidate has not been built".to_string())?;
    Ok(Target {
        store_path: build.store_path,
        commit: build.commit,
        kind: Kind::Forward,
    })
}

/// Only a generation that actually ran is a place to return to, and both halves
/// come from its record: two candidates with the same content build to the same
/// closure, and only one of them was ever activated.
async fn from_generation(state: &AppState, id: i64) -> Result<Target, String> {
    let gens = state.generations.clone();
    blocking(move || {
        let found = generation(&gens.lock().unwrap(), id)?;
        match found.outcome {
            Outcome::Success => Ok(Target {
                store_path: found.store_path,
                commit: found.commit,
                kind: Kind::Rollback,
            }),
            Outcome::Failed { .. } => Err(format!("generation {id} never activated successfully")),
        }
    })
    .await
}

async fn begin_activation<W: AsyncWrite + Unpin>(
    state: &AppState,
    peer: Peer,
    commit: String,
    w: &mut W,
) -> std::io::Result<()> {
    // Refused here rather than after the prompt, so a principal who lost the race
    // is told before a human is asked anything.
    let target = match from_candidate(state, peer, commit).await {
        Ok(target) => target,
        Err(message) => return write(w, &Response::Error { message }).await,
    };
    if let Err(message) = fast_forwards(state, target.commit.clone()).await {
        return write(w, &Response::Error { message }).await;
    }
    issue_challenge(state, target.store_path, w).await
}

async fn begin_rollback<W: AsyncWrite + Unpin>(
    state: &AppState,
    generation: i64,
    w: &mut W,
) -> std::io::Result<()> {
    match from_generation(state, generation).await {
        Ok(target) => issue_challenge(state, target.store_path, w).await,
        Err(message) => write(w, &Response::Error { message }).await,
    }
}

/// Whether the candidate still sits one step off the branch tip.
///
/// Somebody else's apply landing first makes it a configuration derived from a
/// state that is no longer current, so activating it would drop their change
/// while claiming to be a step forward from it.
async fn fast_forwards(state: &AppState, commit: String) -> Result<(), String> {
    with_repo(state, move |repo, _| {
        let head = repo.head().map_err(err)?;
        // Already the tip is reachable: two applies in the same second with the
        // same message and an empty draft write byte-identical commit objects.
        if head.as_deref() == Some(commit.as_str()) {
            return Ok(());
        }
        if repo.parent_of(&commit).map_err(err)? == head {
            return Ok(());
        }
        Err("the configuration moved since this was built; build it again".to_string())
    })
    .await
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

/// Shared by forward activation and rollback, which by here differ only in the
/// intent being recorded. The target was resolved a second time by the caller,
/// so nothing was carried across the gap where the principal was at the prompt.
async fn complete<W: AsyncWrite + Unpin>(
    state: &AppState,
    peer: Peer,
    target: Target,
    nonce: String,
    solution: Solution,
    w: &mut W,
) -> std::io::Result<()> {
    let Target { store_path, commit, kind } = target;

    // Checked again here, because the branch may have moved while the principal
    // was at the prompt.
    if kind == Kind::Forward
        && let Err(message) = fast_forwards(state, commit.clone()).await
    {
        return write(w, &Response::Error { message }).await;
    }

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

    // History follows the machine: the branch moves only now, and only because
    // the trigger acted. A failure leaves the branch where it was, so what it
    // names is still what is running.
    if result.is_ok()
        && let Err(message) = land(state, commit.clone()).await
    {
        return write(w, &Response::Error { message }).await;
    }

    let rec = record(
        state,
        peer,
        store_path,
        commit.clone(),
        nonce,
        solution.signature,
        verifying_key,
        kind,
        outcome,
    );
    let generation = match rec.await {
        Ok(generation) => generation,
        Err(message) => return write(w, &Response::Error { message }).await,
    };

    // A commit a rollback has left behind is reachable from nothing else, and
    // the generation is what a person names to come back to it.
    if result.is_ok()
        && let Err(message) = remember(state, generation.id, commit).await
    {
        return write(w, &Response::Error { message }).await;
    }

    match result {
        Ok(_) => {
            let answer = Response::Activated { generation: Box::new(generation) };
            write(w, &answer).await
        }
        Err(e) => write(w, &Response::Error { message: e.to_string() }).await,
    }
}

/// Put the configuration that has just been activated on the branch, retire the
/// candidates the move has settled, and carry every draft onto it.
async fn land(state: &AppState, commit: String) -> Result<(), String> {
    let gcroots = state.paths.gcroots_dir();
    with_repo(state, move |repo, models| {
        repo.advance(&commit).map_err(err)?;

        // The branch moving settles every candidate there is. The one just
        // activated is rooted by the system profile from here on, and every
        // other was parented on the tip this replaced, so it can no longer be
        // activated and has to be built again.
        for (name, sha) in repo.list_refs(draft::CANDIDATE_REFS).map_err(err)? {
            repo.delete_ref(&name).map_err(err)?;
            release(&gcroots, &sha);
        }

        draft::carry_all(repo, models).map_err(err)?;
        repo.collect_garbage().map_err(err)
    })
    .await
}

/// Hold the commit a generation was activated from, so one the branch has since
/// moved off is still there to be audited.
async fn remember(state: &AppState, generation: i64, commit: String) -> Result<(), String> {
    with_repo(state, move |repo, _| {
        repo.set_ref(&format!("{FRACTAL_REFS}activated/{generation}"), &commit).map_err(err)
    })
    .await
}

/// Drop the garbage-collection root holding a candidate's closure. A candidate
/// reference and its root have one lifetime, so nothing else has to remember
/// which closures are still wanted.
fn release(gcroots: &Path, commit: &str) {
    let root = gcroots.join(commit);
    if let Err(e) = std::fs::remove_file(&root)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!("kept {}: {e}", root.display());
    }
}

/// Recorded from the trigger's result, never the principal's claim.
///
/// `commit` is the configuration this closure was activated from, which for a
/// rollback is the one the earlier generation recorded.
#[allow(clippy::too_many_arguments)]
async fn record(
    state: &AppState,
    peer: Peer,
    store_path: String,
    commit: String,
    nonce: String,
    signature: String,
    verifying_key: String,
    kind: Kind,
    outcome: Outcome,
) -> Result<Generation, String> {
    let gens = state.generations.clone();
    let builds = state.builds.clone();
    blocking(move || {
        let build = builds.lock().unwrap().by_commit(&commit).ok().flatten();
        let build_log = build.and_then(|b| b.log);
        let gens = gens.lock().unwrap();
        let parent_id = gens.latest_success().ok().flatten().map(|g| g.id);
        let rec = NewGeneration {
            store_path,
            commit,
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
        let id = gens.record(&rec).map_err(err)?;
        gens.get(id)
            .map_err(err)?
            .ok_or_else(|| "the generation vanished after being recorded".to_string())
    })
    .await
}

/// Run a blocking closure on the blocking pool, flattening the join error.
async fn blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(err)?
}
