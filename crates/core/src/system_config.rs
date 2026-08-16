//! The agent-owned configuration as it lives in the config repository. The
//! generated `fractal.nix` module holds the configuration of whichever commit is
//! being read: the branch tip is what is running, and a draft commit is what its
//! author is asking for.
//!
//! It is read by evaluating, never by parsing its text, and written by
//! re-serializing the whole model, so the same model always produces the same
//! bytes.

use std::collections::HashMap;

use crate::config::Model;
use crate::error::Result;
use crate::nix;
use crate::repo::GitRepo;

/// The generated Nix module the flake imports, and the model's source of truth.
pub const NIX_FILE: &str = "fractal.nix";

/// The model as of one commit: the configuration a generation was built from, or
/// a draft asks for. Empty if that commit predates the generated module.
pub fn load_at(repo: &GitRepo, commit: &str) -> Result<Model> {
    match repo.read_blob(commit, NIX_FILE)? {
        Some(bytes) => model_from_source(&bytes),
        None => Ok(Model::new()),
    }
}

/// Evaluate module source to its attrset and turn it into a model.
fn model_from_source(bytes: &[u8]) -> Result<Model> {
    let src = String::from_utf8_lossy(bytes);
    let json = nix::eval_module_source(&src)?;
    Ok(Model::from_eval_json(&json))
}

/// Models the agent has already evaluated, keyed by the content they came from.
///
/// Turning the module back into a model spawns an evaluator, and the tip and
/// every draft are read on nearly every request. Keying by the file's object id
/// rather than by the commit is what makes an amend that lands on identical
/// bytes, or a draft that matches the tip, cost nothing.
#[derive(Default)]
pub struct ModelCache {
    by_blob: HashMap<String, Model>,
}

impl ModelCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn at(&mut self, repo: &GitRepo, commit: &str) -> Result<Model> {
        let Some(blob) = repo.blob_id(commit, NIX_FILE)? else {
            return Ok(Model::new());
        };
        if let Some(model) = self.by_blob.get(&blob) {
            return Ok(model.clone());
        }
        let model = load_at(repo, commit)?;
        self.by_blob.insert(blob, model.clone());
        Ok(model)
    }

    /// The model of an unborn repository is the empty one, which is also what a
    /// device that has been provisioned and never applied should read as.
    pub fn at_opt(&mut self, repo: &GitRepo, commit: Option<&str>) -> Result<Model> {
        match commit {
            Some(commit) => self.at(repo, commit),
            None => Ok(Model::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Author;

    // The evaluating reads shell out to `nix` and are exercised at the VM stage;
    // the tree logic they lean on is unit tested in `config`. Here we cover only
    // the file-absent short circuit, which never evaluates.

    #[test]
    fn an_absent_module_is_the_empty_model_without_evaluating() {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        let commit = repo
            .commit_tree(None, None, &[("base.nix", &b"{ }\n"[..])], &Author::for_uid(0), "base")
            .unwrap();

        assert!(load_at(&repo, &commit).unwrap().is_empty());
        assert!(ModelCache::new().at(&repo, &commit).unwrap().is_empty());
        assert!(ModelCache::new().at_opt(&repo, None).unwrap().is_empty());
    }
}
