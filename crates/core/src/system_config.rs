//! The agent-owned configuration as it lives in the config repository. The
//! generated `fractal.nix` module is the durable state: a staged change is an
//! uncommitted edit to it, so nothing is lost across a restart, and applying is
//! committing it.
//!
//! It is read by evaluating, never by parsing its text, and written by
//! re-serializing the whole model, so the same model always produces the same
//! bytes. [`WorkingCopy`] keeps that model in memory so ordinary reads and edits
//! cost no evaluation at all.

use crate::config::Model;
use crate::diff::{self, OptionChange};
use crate::error::Result;
use crate::nix;
use crate::repo::ConfigVcs;

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
}

impl<V: ConfigVcs> WorkingCopy<V> {
    pub fn open(vcs: V) -> Result<Self> {
        let working = load(&vcs)?;
        let committed = load_committed(&vcs)?;
        let head = vcs.head()?;
        let stamp = stamp_of(&vcs);
        Ok(Self { vcs, working, committed, stamp, head })
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

    /// Note that the working copy has been committed at `head`.
    pub fn applied(&mut self, head: Option<String>) {
        self.committed = self.working.clone();
        self.head = head;
    }

    /// Re-stamp after something outside this type wrote the file, such as the
    /// cosmetic formatter, so the next refresh does not read it as an edit.
    pub fn restamp(&mut self) {
        self.stamp = stamp_of(&self.vcs);
    }

    fn replace(&mut self, next: Model) -> Result<()> {
        write(&self.vcs, &next)?;
        self.working = next;
        self.restamp();
        Ok(())
    }
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
    use crate::repo::GitRepo;

    // The evaluating reads (`load`, `staged_diff`, `discard`) shell out to `nix`
    // and are exercised at the VM stage; the tree logic they lean on is unit
    // tested in `config`. Here we cover only what is pure: the projection write
    // and the file-absent short circuits.

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
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        let mut wc = WorkingCopy::open(repo).unwrap();

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
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        let mut wc = WorkingCopy::open(repo).unwrap();
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
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        let mut wc = WorkingCopy::open(repo).unwrap();

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
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        let mut wc = WorkingCopy::open(repo).unwrap();
        wc.edit(|m| m.set("a", Value::Int(1)).map(|_| ())).unwrap();
        assert_eq!(wc.get("a"), Some(Value::Int(1)));

        std::fs::remove_file(dir.path().join(NIX_FILE)).unwrap();
        wc.refresh().unwrap();
        assert!(wc.get("a").is_none(), "reloaded from disk");
    }
}
