//! What each principal has drafted and not yet applied.
//!
//! A draft is a commit at `refs/fractal/draft/<uid>`, parented on the tip it was
//! drafted against, and what it asks for is derived by comparing it with that
//! parent rather than stored in a second place. Two principals may draft the
//! same option and neither is in the other's way: a shared slot would need a
//! lock, and a lock is takeable, holdable and clearable by anyone who can reach
//! it.
//!
//! Nothing here is a database. The references are the state, so an amend
//! replaces one reference atomically and a crash at any point leaves the old
//! draft or the new one.

use std::collections::BTreeMap;

use crate::config::{Change, Model};
use crate::error::{Error, Result};
use crate::repo::{Author, GitRepo};
use crate::system_config::{ModelCache, NIX_FILE};

/// Keyed by uid, because that is what the kernel attests and what survives a
/// rename.
pub type Uid = u32;

pub const DRAFT_REFS: &str = "refs/fractal/draft/";
pub const CANDIDATE_REFS: &str = "refs/fractal/candidate/";

/// Every draft carries the same message. What a draft says is its difference
/// from its parent, and a message would be a second account of it that nothing
/// keeps honest.
const MESSAGE: &str = "draft";

pub fn draft_ref(uid: Uid) -> String {
    format!("{DRAFT_REFS}{uid}")
}

pub fn candidate_ref(uid: Uid) -> String {
    format!("{CANDIDATE_REFS}{uid}")
}

#[derive(Debug, Clone)]
pub struct DraftState {
    pub sha: String,
    /// `None` only on a device provisioned before its first commit.
    pub parent: Option<String>,
    pub model: Model,
}

pub fn load(repo: &GitRepo, models: &mut ModelCache, uid: Uid) -> Result<Option<DraftState>> {
    let Some(sha) = repo.ref_sha(&draft_ref(uid))? else {
        return Ok(None);
    };
    let parent = repo.parent_of(&sha)?;
    let model = models.at(repo, &sha)?;
    Ok(Some(DraftState { sha, parent, model }))
}

pub fn authors(repo: &GitRepo) -> Result<Vec<Uid>> {
    let mut uids: Vec<Uid> = repo
        .list_refs(DRAFT_REFS)?
        .into_iter()
        .filter_map(|(name, _)| name.rsplit('/').next()?.parse().ok())
        .collect();
    uids.sort_unstable();
    Ok(uids)
}

/// A draft is quarantined exactly when it does not sit on the tip. Derived
/// rather than recorded, so there is no flag to go stale and nothing to repair
/// after a crash.
pub fn is_quarantined(repo: &GitRepo, uid: Uid) -> Result<bool> {
    let Some(sha) = repo.ref_sha(&draft_ref(uid))? else {
        return Ok(false);
    };
    Ok(repo.parent_of(&sha)? != repo.head()?)
}

/// Edit the caller's own draft, starting from it or from the tip if they hold
/// none, and carry the result onto the tip.
pub fn amend(
    repo: &GitRepo,
    models: &mut ModelCache,
    uid: Uid,
    f: impl FnOnce(&mut Model) -> Result<()>,
) -> Result<()> {
    let (parent, mut model) = base_of(repo, models, uid)?;
    f(&mut model)?;
    write(repo, models, uid, parent.as_deref(), &[(NIX_FILE, model.to_nix().as_bytes())])
}

/// The same for a human-authored file, whose bytes the agent carries verbatim.
///
/// The generated module is refused here: it changes through the model or not at
/// all, and a hand-written one would be overwritten by the next option drafted
/// without anything noticing.
pub fn amend_file(
    repo: &GitRepo,
    models: &mut ModelCache,
    uid: Uid,
    path: &str,
    bytes: &[u8],
) -> Result<()> {
    if path == NIX_FILE {
        return Err(Error::Conflict(format!("{NIX_FILE} is generated; set the option instead")));
    }
    let (parent, _) = base_of(repo, models, uid)?;
    write(repo, models, uid, parent.as_deref(), &[(path, bytes)])
}

/// Restore the parent's value for the keys named, or drop the whole draft when
/// none are. Reaches the caller's own and no further.
pub fn discard(repo: &GitRepo, models: &mut ModelCache, uid: Uid, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        return repo.delete_ref(&draft_ref(uid));
    }
    let Some(draft) = load(repo, models, uid)? else {
        return Ok(());
    };
    let base = models.at_opt(repo, draft.parent.as_deref())?;
    let mut model = draft.model;
    for key in keys {
        model.apply_change(key, base.get(key))?;
    }
    write(repo, models, uid, draft.parent.as_deref(), &[(NIX_FILE, model.to_nix().as_bytes())])
}

/// What this principal's draft asks for, against the commit it was drafted on.
pub fn changes(repo: &GitRepo, models: &mut ModelCache, uid: Uid) -> Result<Vec<Change>> {
    let Some(draft) = load(repo, models, uid)? else {
        return Ok(Vec::new());
    };
    let base = models.at_opt(repo, draft.parent.as_deref())?;
    Ok(draft.model.diff(&base))
}

/// What the tip has made unapplicable in this principal's draft: option keys and
/// file paths, both as display strings. Empty for a draft that carries cleanly,
/// which is what makes resolving a conflict just editing.
pub fn conflicts(repo: &GitRepo, models: &mut ModelCache, uid: Uid) -> Result<Vec<String>> {
    let (Some(draft), Some(tip)) = (load(repo, models, uid)?, repo.head()?) else {
        return Ok(Vec::new());
    };
    match plan(repo, models, &draft, &tip)? {
        Plan::Quarantine(conflicts) => Ok(conflicts),
        Plan::Rebase(_) => Ok(Vec::new()),
    }
}

/// Carry every draft onto the branch tip. Run after the branch moves, forwards
/// or backwards.
///
/// A pure function of the two commits involved, so rerunning an interrupted
/// carry lands the same way and the conflict path writes nothing at all.
pub fn carry_all(repo: &GitRepo, models: &mut ModelCache) -> Result<()> {
    let Some(tip) = repo.head()? else {
        return Ok(());
    };
    for uid in authors(repo)? {
        carry_onto(repo, models, uid, &tip)?;
    }
    Ok(())
}

/// Retry the carry for one principal, which is what makes a quarantine clear
/// itself as soon as the conflict is edited away.
pub fn carry(repo: &GitRepo, models: &mut ModelCache, uid: Uid) -> Result<()> {
    match repo.head()? {
        Some(tip) => carry_onto(repo, models, uid, &tip),
        None => Ok(()),
    }
}

fn carry_onto(repo: &GitRepo, models: &mut ModelCache, uid: Uid, tip: &str) -> Result<()> {
    let Some(draft) = load(repo, models, uid)? else {
        return Ok(());
    };
    if draft.parent.as_deref() == Some(tip) {
        return Ok(());
    }
    match plan(repo, models, &draft, tip)? {
        Plan::Quarantine(_) => Ok(()),
        Plan::Rebase(files) => {
            let borrowed: Vec<(&str, &[u8])> =
                files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
            write_from(repo, models, uid, Some(&draft.sha), Some(tip), &borrowed)
        }
    }
}

enum Plan {
    /// Path and bytes for everything the draft holds, as it lands on the tip.
    Rebase(Vec<(String, Vec<u8>)>),
    /// Option keys and file paths the tip has made unapplicable.
    Quarantine(Vec<String>),
}

fn plan(repo: &GitRepo, models: &mut ModelCache, draft: &DraftState, tip: &str) -> Result<Plan> {
    let base = models.at_opt(repo, draft.parent.as_deref())?;
    let mut merged = models.at(repo, tip)?;
    let mut conflicts = Vec::new();

    // One key at a time against a copy, so the answer names every key the tip
    // refuses rather than the first, and a refusal leaves nothing half applied.
    for change in draft.model.diff(&base) {
        let mut next = merged.clone();
        match next.apply(std::slice::from_ref(&change)) {
            Ok(()) => merged = next,
            Err(_) => conflicts.push(change.0),
        }
    }

    let mut files = vec![(NIX_FILE.to_string(), merged.to_nix().into_bytes())];
    for (path, merged) in freeform(repo, draft, tip)? {
        match merged {
            Some(bytes) => files.push((path, bytes)),
            None => conflicts.push(path),
        }
    }

    if conflicts.is_empty() {
        Ok(Plan::Rebase(files))
    } else {
        Ok(Plan::Quarantine(conflicts))
    }
}

/// Every human-authored file the draft changed, merged against the tip. `None`
/// where the merge left conflict markers.
///
/// Three-way at the file level rather than the model level, because these files
/// are carried verbatim and the agent has no model of what is in them.
fn freeform(
    repo: &GitRepo,
    draft: &DraftState,
    tip: &str,
) -> Result<Vec<(String, Option<Vec<u8>>)>> {
    let ours = repo.list_blobs(&draft.sha)?;
    let base = match &draft.parent {
        Some(parent) => repo.list_blobs(parent)?,
        None => BTreeMap::new(),
    };
    let theirs = repo.list_blobs(tip)?;

    let mut out = Vec::new();
    for (path, blob) in &ours {
        if path == NIX_FILE || base.get(path) == Some(blob) {
            continue;
        }
        if theirs.get(path) == Some(blob) {
            continue; // the tip has come to say it
        }
        let mine = repo.read_blob(&draft.sha, path)?.unwrap_or_default();
        if theirs.get(path) == base.get(path) {
            out.push((path.clone(), Some(mine)));
            continue;
        }
        // An absent file is the empty one, so adding a file the tip also added
        // merges rather than conflicting outright.
        let their_bytes = read_or_empty(repo, tip, path, &theirs)?;
        let base_bytes = match &draft.parent {
            Some(parent) => read_or_empty(repo, parent, path, &base)?,
            None => Vec::new(),
        };
        out.push((path.clone(), merge_text(&base_bytes, &their_bytes, &mine)));
    }
    Ok(out)
}

fn read_or_empty(
    repo: &GitRepo,
    commit: &str,
    path: &str,
    present: &BTreeMap<String, String>,
) -> Result<Vec<u8>> {
    if !present.contains_key(path) {
        return Ok(Vec::new());
    }
    Ok(repo.read_blob(commit, path)?.unwrap_or_default())
}

/// `None` when the merge could only be expressed with conflict markers.
fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    use gix::merge::blob::Resolution;
    use gix::merge::blob::builtin_driver::text;

    let mut input = imara_diff::InternedInput::new(base, ours);
    let mut out = Vec::new();
    let resolution = text(
        &mut out,
        &mut input,
        text::Labels::default(),
        ours,
        base,
        theirs,
        text::Options::default(),
    );
    match resolution {
        Resolution::Complete | Resolution::CompleteWithAutoResolvedConflict => Some(out),
        Resolution::Conflict => None,
    }
}

/// The draft to edit and the commit it sits on: the author's own if they hold
/// one, else the tip.
fn base_of(repo: &GitRepo, models: &mut ModelCache, uid: Uid) -> Result<(Option<String>, Model)> {
    match load(repo, models, uid)? {
        Some(draft) => Ok((draft.parent, draft.model)),
        None => {
            let tip = repo.head()?;
            let model = models.at_opt(repo, tip.as_deref())?;
            Ok((tip, model))
        }
    }
}

/// Replace the caller's draft with what it already holds plus `files`, then
/// carry it onto the tip.
fn write(
    repo: &GitRepo,
    models: &mut ModelCache,
    uid: Uid,
    parent: Option<&str>,
    files: &[(&str, &[u8])],
) -> Result<()> {
    let held = repo.ref_sha(&draft_ref(uid))?;
    let from = held.as_deref().or(parent);
    write_from(repo, models, uid, from, parent, files)
}

fn write_from(
    repo: &GitRepo,
    models: &mut ModelCache,
    uid: Uid,
    from: Option<&str>,
    parent: Option<&str>,
    files: &[(&str, &[u8])],
) -> Result<()> {
    let sha = repo.commit_tree(from, parent, files, &Author::for_uid(uid), MESSAGE)?;
    if says_nothing(repo, models, &sha, parent)? {
        return repo.delete_ref(&draft_ref(uid));
    }
    repo.set_ref(&draft_ref(uid), &sha)?;
    carry(repo, models, uid)
}

/// Whether `sha` asks for anything `parent` does not already say.
///
/// The generated module is compared as a model and every other file as bytes,
/// because the formatter rewrites the generated bytes on the way into a
/// candidate, and a draft the configuration has come to satisfy has to be
/// released all the same.
fn says_nothing(
    repo: &GitRepo,
    models: &mut ModelCache,
    sha: &str,
    parent: Option<&str>,
) -> Result<bool> {
    let mut ours = repo.list_blobs(sha)?;
    let mut theirs = match parent {
        Some(parent) => repo.list_blobs(parent)?,
        None => BTreeMap::new(),
    };
    ours.remove(NIX_FILE);
    theirs.remove(NIX_FILE);
    Ok(ours == theirs && models.at(repo, sha)? == models.at_opt(repo, parent)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Value;

    const ME: Uid = 1000;
    const THEM: Uid = 1001;

    /// Reading a model evaluates it, so these need `nix`. Skipped where it is
    /// absent rather than failing, since the rest of the suite has no such
    /// dependency.
    fn have_nix() -> bool {
        std::process::Command::new(std::env::var("FRACTAL_NIX_BIN").unwrap_or("nix".into()))
            .arg("--version")
            .output()
            .is_ok()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: GitRepo,
        models: ModelCache,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = GitRepo::open_or_init(dir.path()).unwrap();
            let base = repo
                .commit_tree(
                    None,
                    None,
                    &[("base.nix", &b"{ }\n"[..]), (NIX_FILE, Model::new().to_nix().as_bytes())],
                    &Author::for_uid(0),
                    "scaffold",
                )
                .unwrap();
            repo.advance(&base).unwrap();
            Self { _dir: dir, repo, models: ModelCache::new() }
        }

        fn set(&mut self, uid: Uid, key: &str, value: Option<Value>) {
            amend(&self.repo, &mut self.models, uid, |model| {
                match value {
                    Some(value) => {
                        model.set(key, value)?;
                    }
                    None => {
                        model.remove(key);
                    }
                }
                Ok(())
            })
            .unwrap();
        }

        fn changes(&mut self, uid: Uid) -> Vec<Change> {
            changes(&self.repo, &mut self.models, uid).unwrap()
        }

        fn held(&self, uid: Uid) -> bool {
            self.repo.ref_sha(&draft_ref(uid)).unwrap().is_some()
        }

        /// Stand in for an activation: put `f`'s model on the branch and carry
        /// every draft onto it.
        fn land(&mut self, f: impl FnOnce(&mut Model)) {
            let tip = self.repo.head().unwrap().unwrap();
            let mut model = self.models.at(&self.repo, &tip).unwrap();
            f(&mut model);
            let landed = self
                .repo
                .commit_tree(
                    Some(&tip),
                    Some(&tip),
                    &[(NIX_FILE, model.to_nix().as_bytes())],
                    &Author::for_uid(0),
                    "applied",
                )
                .unwrap();
            self.repo.advance(&landed).unwrap();
            carry_all(&self.repo, &mut self.models).unwrap();
        }
    }

    fn int(n: i64) -> Option<Value> {
        Some(Value::Int(n))
    }

    #[test]
    fn two_principals_may_draft_the_same_option() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        f.set(ME, "time.timeZone", int(1));
        f.set(THEM, "time.timeZone", int(2));

        assert_eq!(f.changes(ME), [("time.timeZone".to_string(), int(1))]);
        assert_eq!(f.changes(THEM), [("time.timeZone".to_string(), int(2))]);
        assert_eq!(authors(&f.repo).unwrap(), [ME, THEM]);
    }

    #[test]
    fn drafting_again_replaces_your_own() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        f.set(ME, "a", int(1));
        f.set(ME, "a", int(2));
        assert_eq!(f.changes(ME), [("a".to_string(), int(2))]);
    }

    /// Null is a value somebody may mean, so it must not read back as a removal.
    #[test]
    fn a_removal_and_a_null_are_not_the_same_draft() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        f.land(|model| {
            model.set("gone", Value::Int(1)).unwrap();
            model.set("null", Value::Int(1)).unwrap();
        });

        f.set(ME, "gone", None);
        f.set(ME, "null", Some(Value::Null));
        assert_eq!(
            f.changes(ME),
            [("gone".to_string(), None), ("null".to_string(), Some(Value::Null))]
        );
    }

    #[test]
    fn discarding_reaches_your_own_and_no_further() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        f.set(ME, "a", int(1));
        f.set(ME, "b", int(3));
        f.set(THEM, "a", int(2));

        discard(&f.repo, &mut f.models, ME, &["a".to_string()]).unwrap();
        assert_eq!(f.changes(ME), [("b".to_string(), int(3))], "named key only");

        discard(&f.repo, &mut f.models, ME, &[]).unwrap();
        assert!(!f.held(ME));
        assert_eq!(f.changes(THEM), [("a".to_string(), int(2))], "somebody else's is untouched");
    }

    /// Released without the agent recording who applied what, so two principals
    /// who drafted the same value are both answered.
    #[test]
    fn a_change_the_tip_satisfies_is_released() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        f.set(ME, "landed", int(1));
        f.set(THEM, "landed", int(1));
        f.set(THEM, "pending", int(9));

        f.land(|model| {
            model.set("landed", Value::Int(1)).unwrap();
        });

        assert!(!f.held(ME), "nothing left to ask for");
        assert_eq!(f.changes(THEM), [("pending".to_string(), int(9))]);
    }

    /// A change the new tip contradicts stays, still asking for its value,
    /// exactly as if it had been drafted a minute later.
    #[test]
    fn a_change_the_tip_contradicts_survives_the_carry() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        f.set(ME, "x", int(2));
        f.land(|model| {
            model.set("x", Value::Int(3)).unwrap();
        });

        assert_eq!(f.changes(ME), [("x".to_string(), int(2))]);
        assert!(!is_quarantined(&f.repo, ME).unwrap(), "it applied cleanly");
    }

    /// The tree changed shape underneath the draft, so the draft is left on its
    /// old parent, readable but not applyable.
    #[test]
    fn a_shape_change_quarantines_the_whole_draft() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        f.set(ME, "a", int(1));
        f.set(ME, "unrelated", int(5));
        let before = f.repo.ref_sha(&draft_ref(ME)).unwrap();

        f.land(|model| {
            model.remove("a");
            model.set("a.b", Value::Int(1)).unwrap();
        });

        assert!(is_quarantined(&f.repo, ME).unwrap());
        assert_eq!(f.repo.ref_sha(&draft_ref(ME)).unwrap(), before, "the carry wrote nothing");
        assert_eq!(conflicts(&f.repo, &mut f.models, ME).unwrap(), ["a"]);
    }

    #[test]
    fn a_quarantine_clears_when_the_conflicting_key_is_discarded() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        f.set(ME, "a", int(1));
        f.set(ME, "unrelated", int(5));
        f.land(|model| {
            model.remove("a");
            model.set("a.b", Value::Int(1)).unwrap();
        });
        assert!(is_quarantined(&f.repo, ME).unwrap());

        discard(&f.repo, &mut f.models, ME, &["a".to_string()]).unwrap();

        assert!(!is_quarantined(&f.repo, ME).unwrap());
        assert_eq!(f.changes(ME), [("unrelated".to_string(), int(5))]);
    }

    #[test]
    fn a_freeform_edit_lands_in_the_draft_and_the_generated_file_is_refused() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        amend_file(&f.repo, &mut f.models, ME, "base.nix", b"{ mine = 1; }\n").unwrap();

        let sha = f.repo.ref_sha(&draft_ref(ME)).unwrap().unwrap();
        assert_eq!(
            f.repo.read_blob(&sha, "base.nix").unwrap().as_deref(),
            Some(&b"{ mine = 1; }\n"[..])
        );
        assert!(f.changes(ME).is_empty(), "no option changed");

        let refused = amend_file(&f.repo, &mut f.models, ME, NIX_FILE, b"{ }\n");
        assert!(matches!(refused, Err(Error::Conflict(_))), "got {refused:?}");
    }

    #[test]
    fn a_freeform_edit_the_tip_makes_verbatim_is_released() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        amend_file(&f.repo, &mut f.models, ME, "base.nix", b"{ shared = 1; }\n").unwrap();

        let tip = f.repo.head().unwrap().unwrap();
        let landed = f
            .repo
            .commit_tree(
                Some(&tip),
                Some(&tip),
                &[("base.nix", &b"{ shared = 1; }\n"[..])],
                &Author::for_uid(0),
                "applied",
            )
            .unwrap();
        f.repo.advance(&landed).unwrap();
        carry_all(&f.repo, &mut f.models).unwrap();

        assert!(!f.held(ME));
    }

    #[test]
    fn a_freeform_edit_conflicting_with_the_tip_quarantines() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        amend_file(&f.repo, &mut f.models, ME, "base.nix", b"{ mine = 1; }\n").unwrap();

        let tip = f.repo.head().unwrap().unwrap();
        let landed = f
            .repo
            .commit_tree(
                Some(&tip),
                Some(&tip),
                &[("base.nix", &b"{ theirs = 2; }\n"[..])],
                &Author::for_uid(0),
                "applied",
            )
            .unwrap();
        f.repo.advance(&landed).unwrap();
        carry_all(&f.repo, &mut f.models).unwrap();

        assert!(is_quarantined(&f.repo, ME).unwrap());
        assert_eq!(conflicts(&f.repo, &mut f.models, ME).unwrap(), ["base.nix"]);
    }

    /// Disjoint edits to one file are carried rather than refused, which is what
    /// makes a text merge worth having at all.
    #[test]
    fn disjoint_freeform_edits_merge_across_a_carry() {
        if !have_nix() {
            return;
        }
        let mut f = Fixture::new();
        let tip = f.repo.head().unwrap().unwrap();
        let seeded = f
            .repo
            .commit_tree(
                Some(&tip),
                Some(&tip),
                &[("base.nix", &b"one\ntwo\nthree\n"[..])],
                &Author::for_uid(0),
                "seed",
            )
            .unwrap();
        f.repo.advance(&seeded).unwrap();

        amend_file(&f.repo, &mut f.models, ME, "base.nix", b"one\ntwo\nMINE\n").unwrap();

        let landed = f
            .repo
            .commit_tree(
                Some(&seeded),
                Some(&seeded),
                &[("base.nix", &b"THEIRS\ntwo\nthree\n"[..])],
                &Author::for_uid(0),
                "applied",
            )
            .unwrap();
        f.repo.advance(&landed).unwrap();
        carry_all(&f.repo, &mut f.models).unwrap();

        assert!(!is_quarantined(&f.repo, ME).unwrap());
        let sha = f.repo.ref_sha(&draft_ref(ME)).unwrap().unwrap();
        assert_eq!(
            f.repo.read_blob(&sha, "base.nix").unwrap().as_deref(),
            Some(&b"THEIRS\ntwo\nMINE\n"[..])
        );
    }
}
