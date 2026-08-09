//! The agent-owned configuration as it lives in the config repository. The
//! generated `fractal.nix` module is the single source of truth: an edit reads
//! the model back by evaluating that module (never by parsing its text), mutates
//! it, and rewrites the whole file through the canonical serializer. A staged
//! change is thus an uncommitted edit to `fractal.nix`, and applying it commits
//! the file — the working copy is the durable staged state, so no edit is lost
//! across a restart.

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

/// The option-level difference between the committed model and the working copy:
/// exactly what this staging session has changed.
pub fn staged_diff(vcs: &dyn ConfigVcs) -> Result<Vec<OptionChange>> {
    let before = load_committed(vcs)?;
    let after = load(vcs)?;
    Ok(diff::option_diff(&before.leaves(), &after.leaves()))
}

/// Drop every staged change, restoring the working-copy module to the committed
/// one. Only the agent-owned file is touched; human-authored files are left
/// alone. This wipes everyone's work, so it is the deliberate form; the default
/// is [`discard_keys`].
pub fn discard(vcs: &dyn ConfigVcs) -> Result<()> {
    let committed = load_committed(vcs)?;
    write(vcs, &committed)
}

/// Drop the staged change at each of `keys`, restoring those keys alone to their
/// committed values and leaving everybody else's staged keys where they are.
///
/// A key with no committed value is removed rather than reset, because "not set"
/// is what it was before somebody staged it.
pub fn discard_keys(vcs: &dyn ConfigVcs, keys: &[String]) -> Result<()> {
    let committed = load_committed(vcs)?;
    let mut working = load(vcs)?;
    for key in keys {
        match committed.get(key) {
            Some(value) => {
                working.remove(key);
                working.set(key, value)?;
            }
            None => {
                working.remove(key);
            }
        }
    }
    write(vcs, &working)
}

/// Evaluate module source to its attrset and turn it into a model.
fn model_from_source(bytes: &[u8]) -> Result<Model> {
    let src = String::from_utf8_lossy(bytes);
    let json = nix::eval_module_source(&src)?;
    Ok(Model::from_eval_json(&json))
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
}
