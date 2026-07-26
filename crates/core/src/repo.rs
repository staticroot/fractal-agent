//! The system configuration repository, tracked with an embedded version-control
//! library rather than by shelling out to git. Two behaviours carry the model:
//! a staged change is an uncommitted change in the working copy, and applying a
//! change commits it. The backend sits behind a trait so it can later move to a
//! working-copy-is-a-commit model (the intended direction is jj) without
//! disturbing the rest of the agent.

use std::path::{Path, PathBuf};

use gix::bstr::BStr;
use gix::objs::tree::EntryKind;

use crate::error::{Error, Result};

/// The version-control operations the agent relies on. Deliberately small: it
/// says nothing about branches (not in v0) but nothing here forecloses them.
pub trait ConfigVcs {
    /// The working directory holding the tracked files.
    fn workdir(&self) -> &Path;
    /// Write a file in the working copy, creating parent directories.
    fn write_file(&self, rel: &str, contents: &[u8]) -> Result<()>;
    /// Read a working-copy file, or `None` if it does not exist.
    fn read_file(&self, rel: &str) -> Result<Option<Vec<u8>>>;
    /// Whether the working copy differs from the last commit — i.e. staged.
    fn is_dirty(&self) -> Result<bool>;
    /// Commit the whole working copy; returns the new commit hash.
    fn commit_all(&self, message: &str) -> Result<String>;
    /// The current commit hash, or `None` if the repository is unborn.
    fn head(&self) -> Result<Option<String>>;
}

pub struct GitRepo {
    repo: gix::Repository,
    workdir: PathBuf,
}

fn git<E: std::fmt::Display>(e: E) -> Error {
    Error::Git(e.to_string())
}

impl GitRepo {
    /// Open the repository at `path`, initializing it if it is not one yet.
    pub fn open_or_init(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let repo = match gix::open(&path) {
            Ok(repo) => repo,
            Err(_) => gix::init(&path).map_err(git)?,
        };
        let workdir = repo
            .workdir()
            .ok_or_else(|| Error::Git("configuration repository is bare".into()))?
            .to_path_buf();
        Ok(Self { repo, workdir })
    }

    /// Build a tree object from the current working-copy files and return its id.
    /// Reused for both committing and dirtiness, so the two can never disagree.
    fn worktree_tree(&self) -> Result<gix::ObjectId> {
        let empty = gix::ObjectId::empty_tree(self.repo.object_hash());
        let mut editor = self.repo.edit_tree(empty).map_err(git)?;

        let mut files = Vec::new();
        collect(&self.workdir, "", &mut files)?;
        files.sort();

        for (rel, abs) in &files {
            let bytes = std::fs::read(abs).map_err(|e| Error::io(abs, e))?;
            let blob = self.repo.write_blob(&bytes).map_err(git)?.detach();
            editor
                .upsert(rel.as_str(), EntryKind::Blob, blob)
                .map_err(git)?;
        }
        Ok(editor.write().map_err(git)?.detach())
    }

    fn head_tree(&self) -> Result<Option<gix::ObjectId>> {
        match self.repo.head_commit() {
            Ok(commit) => Ok(Some(commit.tree_id().map_err(git)?.detach())),
            Err(_) => Ok(None), // unborn: no commit yet
        }
    }
}

impl ConfigVcs for GitRepo {
    fn workdir(&self) -> &Path {
        &self.workdir
    }

    fn write_file(&self, rel: &str, contents: &[u8]) -> Result<()> {
        let path = self.workdir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        std::fs::write(&path, contents).map_err(|e| Error::io(&path, e))
    }

    fn read_file(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        let path = self.workdir.join(rel);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(&path, e)),
        }
    }

    fn is_dirty(&self) -> Result<bool> {
        let worktree = self.worktree_tree()?;
        match self.head_tree()? {
            Some(head) => Ok(worktree != head),
            None => Ok(worktree != gix::ObjectId::empty_tree(self.repo.object_hash())),
        }
    }

    fn commit_all(&self, message: &str) -> Result<String> {
        let tree = self.worktree_tree()?;
        let parents: Vec<gix::ObjectId> = match self.repo.head_commit() {
            Ok(commit) => vec![commit.id().detach()],
            Err(_) => vec![],
        };

        // A fixed identity, so committing never depends on ambient git config
        // that a service-owned repository in /var/lib would not carry.
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let time = format!("{secs} +0000");
        let sig = gix::actor::SignatureRef {
            name: BStr::new("fractal-agent"),
            email: BStr::new("agent@fractal.local"),
            time: &time,
        };

        let id = self
            .repo
            .commit_as(sig, sig, "HEAD", message, tree, parents)
            .map_err(git)?;
        Ok(id.detach().to_string())
    }

    fn head(&self) -> Result<Option<String>> {
        match self.repo.head_commit() {
            Ok(commit) => Ok(Some(commit.id().detach().to_string())),
            Err(_) => Ok(None),
        }
    }
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
        let entry = entry.map_err(|e| Error::io(dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let ft = entry.file_type().map_err(|e| Error::io(&path, e))?;
        if ft.is_dir() {
            collect(&path, &rel, out)?;
        } else if ft.is_file() {
            out.push((rel, path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_commit_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::open_or_init(dir.path()).unwrap();

        // Unborn repo, no files: clean and headless.
        assert!(!repo.is_dirty().unwrap());
        assert!(repo.head().unwrap().is_none());

        // Writing a file stages it.
        repo.write_file("fractal.nix", b"{ ... }: { }\n").unwrap();
        assert!(repo.is_dirty().unwrap());

        // Applying commits it; the working copy is now clean and HEAD exists.
        let first = repo.commit_all("initial").unwrap();
        assert!(!repo.is_dirty().unwrap());
        assert_eq!(repo.head().unwrap().as_deref(), Some(first.as_str()));

        // Re-reading round-trips the bytes.
        assert_eq!(
            repo.read_file("fractal.nix").unwrap().as_deref(),
            Some(&b"{ ... }: { }\n"[..])
        );
        assert!(repo.read_file("absent.nix").unwrap().is_none());

        // A further edit is dirty again and commits to a distinct child.
        repo.write_file("fractal.nix", b"{ ... }: { x = 1; }\n").unwrap();
        assert!(repo.is_dirty().unwrap());
        let second = repo.commit_all("edit").unwrap();
        assert_ne!(first, second);
        assert!(!repo.is_dirty().unwrap());
    }
}
