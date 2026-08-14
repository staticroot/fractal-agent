//! The agent-owned configuration as it lives in the config repository. The
//! generated `fractal.nix` module is the durable state: a staged change is an
//! uncommitted edit to it, so nothing is lost across a restart, and applying is
//! committing it.
//!
//! It is read by evaluating, never by parsing its text, and written by
//! re-serializing the whole model, so the same model always produces the same
//! bytes. [`WorkingCopy`] keeps that model in memory so ordinary reads and edits
//! cost no evaluation at all.

use std::path::{Path, PathBuf};

use crate::config::Model;
use crate::diff::{self, OptionChange};
use crate::error::{Error, Result};
use crate::nix;
use crate::repo::{self, ConfigVcs};

/// The generated Nix module the flake imports, and the model's source of truth.
pub const NIX_FILE: &str = "fractal.nix";

/// The working-copy model, read back by evaluating the module. The empty model
/// when the file does not exist yet (a freshly provisioned repository), so the
/// first edit needs no evaluation.
pub fn load(vcs: &dyn ConfigVcs) -> Result<Model> {
    match vcs.read_file(NIX_FILE)? {
        Some(bytes) => model_from_source(&bytes),
        None => Ok(Model::new()),
    }
}

/// The model as of the last commit — the baseline a staged change is measured
/// against. Empty if the repository is unborn or never held the module.
pub fn load_committed(vcs: &dyn ConfigVcs) -> Result<Model> {
    match vcs.read_file_at_head(NIX_FILE)? {
        Some(bytes) => model_from_source(&bytes),
        None => Ok(Model::new()),
    }
}

/// The model as of one commit: the configuration a generation was built from.
/// Empty if that commit predates the generated module.
pub fn load_at(vcs: &dyn ConfigVcs, commit: &str) -> Result<Model> {
    match vcs.read_file_at(commit, NIX_FILE)? {
        Some(bytes) => model_from_source(&bytes),
        None => Ok(Model::new()),
    }
}

/// Write the model's projection into the working copy. This stages a change; it
/// does not commit. Cosmetic formatting of the file is a separate, non-fatal
/// step the caller runs afterwards.
pub fn write(vcs: &dyn ConfigVcs, model: &Model) -> Result<()> {
    vcs.write_file(NIX_FILE, model.to_nix().as_bytes())
}

/// Evaluate module source to its attrset and turn it into a model.
fn model_from_source(bytes: &[u8]) -> Result<Model> {
    let src = String::from_utf8_lossy(bytes);
    let json = nix::eval_module_source(&src)?;
    Ok(Model::from_eval_json(&json))
}

/// Size and modification time of the generated file as we last saw it, which is
/// enough to notice somebody else writing it.
type Stamp = (u64, Option<std::time::SystemTime>);

/// The configuration held in memory, so reading a value costs nothing.
///
/// Reading through `load` spawns `nix` to turn the module back into a model, so
/// setting one boolean used to cost two evaluator startups. The agent has sole
/// authority over the generated file, so the model it last wrote is the model on
/// disk.
///
/// Still validated rather than assumed: [`Self::refresh`] compares the file's
/// size and modification time, and the repository's head, against what this last
/// saw, and re-evaluates only when one of them moved.
pub struct WorkingCopy<V: ConfigVcs> {
    vcs: V,
    working: Model,
    committed: Model,
    stamp: Stamp,
    head: Option<String>,
    backup: PathBuf,
}

impl<V: ConfigVcs> WorkingCopy<V> {
    /// `backup` holds the working file while a commit takes in only part of it.
    /// It belongs outside the working directory: everything inside is committed,
    /// so a copy kept there would land in the history it exists to protect.
    pub fn open(vcs: V, backup: impl Into<PathBuf>) -> Result<Self> {
        let backup = backup.into();
        restore_backup(&vcs, &backup)?;
        let working = load(&vcs)?;
        let committed = load_committed(&vcs)?;
        let head = vcs.head()?;
        let stamp = stamp_of(&vcs);
        Ok(Self { vcs, working, committed, stamp, head, backup })
    }

    pub fn vcs(&self) -> &V {
        &self.vcs
    }

    /// Re-evaluate whatever moved underneath us. Two cheap checks, no evaluation
    /// in the common case where nothing changed.
    pub fn refresh(&mut self) -> Result<()> {
        let stamp = stamp_of(&self.vcs);
        if stamp != self.stamp {
            self.working = load(&self.vcs)?;
            self.stamp = stamp;
        }
        let head = self.vcs.head()?;
        if head != self.head {
            self.committed = load_committed(&self.vcs)?;
            self.head = head;
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<crate::config::Value> {
        self.working.get(key)
    }

    pub fn staged_changes(&self) -> Vec<OptionChange> {
        diff::option_diff(&self.committed.leaves(), &self.working.leaves())
    }

    /// Mutate the model and write the projection. A mutation that fails leaves
    /// both the model and the file untouched.
    pub fn edit(&mut self, mutate: impl FnOnce(&mut Model) -> Result<()>) -> Result<()> {
        let mut next = self.working.clone();
        mutate(&mut next)?;
        self.replace(next)
    }

    /// Restore `keys` to their committed values, leaving every other staged key
    /// alone. A key with no committed value is removed rather than reset,
    /// because "not set" is what it was before somebody staged it.
    pub fn discard_keys(&mut self, keys: &[String]) -> Result<()> {
        let mut next = self.working.clone();
        for key in keys {
            next.remove(key);
            if let Some(value) = self.committed.get(key) {
                next.set(key, value)?;
            }
        }
        self.replace(next)
    }

    /// Restore everything, which wipes every principal's staged work.
    pub fn discard_all(&mut self) -> Result<()> {
        self.replace(self.committed.clone())
    }

    /// Leave the last commit plus `accepted` in the working file, so committing
    /// it takes in those changes and no others. Pairs with [`Self::restore`],
    /// which puts back what this copies aside; the copy is on disk for the whole
    /// window, so an agent that dies in it leaves the rest recoverable rather
    /// than in memory that has gone.
    pub fn reduce_to(&mut self, accepted: &[OptionChange]) -> Result<()> {
        let mut next = self.committed.clone();
        for change in accepted {
            match &change.after {
                Some(value) => {
                    next.set(&change.key, value.clone())?;
                }
                None => {
                    next.remove(&change.key);
                }
            }
        }

        // A copy only when something is left staged. Otherwise the file the commit
        // is made from is the file that stays, and putting the earlier bytes back
        // would undo the formatter and leave the tree dirty against a commit it
        // matches.
        if next != self.working {
            match self.vcs.read_file(NIX_FILE)? {
                Some(bytes) => repo::write_atomic(&self.backup, &bytes)?,
                None => return Err(Error::Other(format!("{NIX_FILE} is missing"))),
            }
        }
        write(&self.vcs, &next)
    }

    /// Put back what [`Self::reduce_to`] copied aside, and nothing if it copied
    /// nothing.
    ///
    /// Neither half updates the model this holds, deliberately: the formatter and
    /// the commit have moved the file and the head by now, so the next
    /// [`Self::refresh`] reads them rather than this assuming them.
    pub fn restore(&mut self) -> Result<()> {
        restore_backup(&self.vcs, &self.backup)
    }

    fn replace(&mut self, next: Model) -> Result<()> {
        write(&self.vcs, &next)?;
        self.working = next;
        self.stamp = stamp_of(&self.vcs);
        Ok(())
    }
}

/// Also the recovery path, which is why finding a copy at all is enough to act
/// on: it exists only between the file being reduced and being made whole again.
///
/// Two steps rather than one rename of the copy onto the file, because
/// `rename(2)` fails with `EXDEV` across a mount and the state directory is free
/// to be one. Dying between them repeats this on the next open, over a file that
/// already holds those bytes.
fn restore_backup(vcs: &dyn ConfigVcs, backup: &Path) -> Result<()> {
    let bytes = match std::fs::read(backup) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(backup, e)),
    };
    vcs.write_file(NIX_FILE, &bytes)?;
    std::fs::remove_file(backup).map_err(|e| Error::io(backup, e))
}

fn stamp_of(vcs: &dyn ConfigVcs) -> Stamp {
    match std::fs::metadata(vcs.workdir().join(NIX_FILE)) {
        Ok(meta) => (meta.len(), meta.modified().ok()),
        Err(_) => (0, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Value;
    use crate::repo::{Author, GitRepo};

    // The evaluating reads (`load`, `staged_diff`, `discard`) shell out to `nix`
    // and are exercised at the VM stage; the tree logic they lean on is unit
    // tested in `config`. Here we cover only what is pure: the projection write
    // and the file-absent short circuits.

    /// The repository sits in a subdirectory so the commit backup can be beside
    /// it rather than in it, which is where the agent keeps it too.
    fn working_copy(root: &Path) -> WorkingCopy<GitRepo> {
        let repo = GitRepo::open_or_init(root.join("config")).unwrap();
        WorkingCopy::open(repo, root.join("pending")).unwrap()
    }

    #[test]
    fn write_projects_the_model_to_the_nix_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        let mut model = Model::new();
        model.set("networking.hostName", Value::Str("box".into())).unwrap();

        write(&repo, &model).unwrap();
        assert_eq!(
            repo.read_file(NIX_FILE).unwrap().as_deref(),
            Some(model.to_nix().as_bytes())
        );
    }

    #[test]
    fn absent_module_loads_the_empty_model_without_evaluating() {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        assert!(load(&repo).unwrap().is_empty());
        assert!(load_committed(&repo).unwrap().is_empty());
    }

    /// An empty repository needs no evaluation, so the cached path is testable
    /// without `nix`: edits, discards and the staged view all run off the model.
    #[test]
    fn edits_run_off_the_model_and_write_through() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = working_copy(dir.path());

        wc.edit(|m| m.set("time.timeZone", Value::Str("UTC".into())).map(|_| ()))
            .unwrap();
        assert_eq!(wc.get("time.timeZone"), Some(Value::Str("UTC".into())));

        let staged = wc.staged_changes();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].key, "time.timeZone");

        // The projection reached disk, not just memory.
        let on_disk = wc.vcs().read_file(NIX_FILE).unwrap().unwrap();
        assert!(String::from_utf8_lossy(&on_disk).contains(r#"time.timeZone = "UTC""#));
    }

    #[test]
    fn a_failed_edit_changes_neither_model_nor_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = working_copy(dir.path());
        wc.edit(|m| m.set("a.b", Value::Int(1)).map(|_| ())).unwrap();
        let before = wc.vcs().read_file(NIX_FILE).unwrap();

        // Setting a value beneath a leaf is a conflict.
        assert!(wc.edit(|m| m.set("a.b.c", Value::Int(2)).map(|_| ())).is_err());
        assert_eq!(wc.get("a.b"), Some(Value::Int(1)));
        assert!(wc.get("a.b.c").is_none());
        assert_eq!(wc.vcs().read_file(NIX_FILE).unwrap(), before);
    }

    #[test]
    fn discard_keys_restores_only_what_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = working_copy(dir.path());

        wc.edit(|m| m.set("mine", Value::Int(1)).map(|_| ())).unwrap();
        wc.edit(|m| m.set("theirs", Value::Int(2)).map(|_| ())).unwrap();
        wc.discard_keys(&["mine".to_string()]).unwrap();

        assert!(wc.get("mine").is_none(), "no committed value, so removed");
        assert_eq!(wc.get("theirs"), Some(Value::Int(2)), "left alone");
    }

    /// The cache is validated, not assumed: a file changed behind its back is
    /// noticed on the next refresh. Removing it is the one outside change that
    /// can be checked without `nix`, since an absent module needs no evaluation.
    #[test]
    fn refresh_notices_the_file_changing_underneath() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = working_copy(dir.path());
        wc.edit(|m| m.set("a", Value::Int(1)).map(|_| ())).unwrap();
        assert_eq!(wc.get("a"), Some(Value::Int(1)));

        std::fs::remove_file(wc.vcs().workdir().join(NIX_FILE)).unwrap();
        wc.refresh().unwrap();
        assert!(wc.get("a").is_none(), "reloaded from disk");
    }

    fn accepting(key: &str, value: i64) -> [OptionChange; 1] {
        [OptionChange {
            key: key.into(),
            before: None,
            after: Some(Value::Int(value)),
        }]
    }

    #[test]
    fn a_commit_takes_in_the_accepted_keys_and_leaves_the_rest_staged() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = working_copy(dir.path());
        wc.edit(|m| m.set("mine", Value::Int(1)).map(|_| ())).unwrap();
        wc.edit(|m| m.set("theirs", Value::Int(2)).map(|_| ())).unwrap();

        wc.reduce_to(&accepting("mine", 1)).unwrap();
        let head = wc.vcs().commit_all("take mine", &Author::for_uid(0), &[]).unwrap();
        wc.restore().unwrap();

        let committed = wc.vcs().read_file_at(&head, NIX_FILE).unwrap().unwrap();
        let committed = String::from_utf8(committed).unwrap();
        assert!(committed.contains("mine = 1"), "got: {committed}");
        assert!(!committed.contains("theirs"), "rode along: {committed}");

        let working = wc.vcs().read_file(NIX_FILE).unwrap().unwrap();
        let working = String::from_utf8(working).unwrap();
        assert!(working.contains("theirs = 2"), "lost from the file: {working}");
        assert!(working.contains("mine = 1"), "lost from the file: {working}");
        assert!(!dir.path().join("pending").exists(), "copy left behind");
    }

    /// The whole working copy was committed, so putting the pre-commit bytes back
    /// would only undo the formatter and leave the tree dirty against a commit it
    /// matches.
    #[test]
    fn committing_everything_leaves_the_committed_file_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let mut wc = working_copy(dir.path());
        wc.edit(|m| m.set("mine", Value::Int(1)).map(|_| ())).unwrap();

        wc.reduce_to(&accepting("mine", 1)).unwrap();
        wc.vcs().write_file(NIX_FILE, b"formatted by somebody else").unwrap();
        wc.vcs().commit_all("all of it", &Author::for_uid(0), &[]).unwrap();
        wc.restore().unwrap();

        assert_eq!(
            wc.vcs().read_file(NIX_FILE).unwrap().unwrap(),
            b"formatted by somebody else"
        );
    }

    #[test]
    fn an_interrupted_commit_leaves_the_working_file_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::open_or_init(dir.path().join("config")).unwrap();
        let backup = dir.path().join("pending");
        repo.write_file(NIX_FILE, b"only what was accepted").unwrap();
        std::fs::write(&backup, b"everything staged").unwrap();

        restore_backup(&repo, &backup).unwrap();
        assert_eq!(repo.read_file(NIX_FILE).unwrap().unwrap(), b"everything staged");
        assert!(!backup.exists());
    }
}
