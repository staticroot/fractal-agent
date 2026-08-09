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
    /// Read a file as it is at the last commit, or `None` if the file is absent
    /// there or the repository is unborn. The committed baseline a staged diff
    /// compares against.
    fn read_file_at_head(&self, rel: &str) -> Result<Option<Vec<u8>>>;
    /// Read a file as it was at `commit`, or `None` if absent there. This is what
    /// makes history semantic: two generations are compared by evaluating the
    /// configuration each was built from, not by diffing text.
    fn read_file_at(&self, commit: &str, rel: &str) -> Result<Option<Vec<u8>>>;
    /// Whether the working copy differs from the last commit — i.e. staged.
    fn is_dirty(&self) -> Result<bool>;
    /// Take in the whole working copy, crediting `author` and listing
    /// `coauthors` as trailers; returns the new revision.
    ///
    /// Git separates author from committer, so work several principals
    /// contributed to is written by the agent while still naming who made it.
    fn commit_all(&self, message: &str, author: &Author, coauthors: &[Author]) -> Result<String>;
    /// The current commit hash, or `None` if the repository is unborn.
    fn head(&self) -> Result<Option<String>>;
}

/// Who a change is attributed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub name: String,
    pub email: String,
}

impl Author {
    /// The identity the agent itself commits under. It is the committer, never
    /// the author: the agent performs the write, it does not make the change.
    pub fn agent() -> Self {
        Self {
            name: "fractal-agent".into(),
            email: "agent@fractal.local".into(),
        }
    }

    /// Attribution derived from the kernel-attested uid, never from anything a
    /// caller says: a client that could name its own author would make
    /// attribution a claim rather than a fact.
    ///
    /// Prefers the principal's own git identity, then their account name, then
    /// the bare uid.
    pub fn for_uid(uid: u32) -> Self {
        use uzers::os::unix::UserExt;

        let user = uzers::get_user_by_uid(uid);
        let account = user
            .as_ref()
            .map(|u| u.name().to_string_lossy().into_owned())
            .filter(|name| !name.is_empty());

        let from_git = user
            .as_ref()
            .and_then(|u| git_identity(u.home_dir()))
            .filter(|(name, email)| !name.is_empty() && !email.is_empty());

        match (from_git, account) {
            (Some((name, email)), _) => Self { name, email },
            (None, Some(name)) => Self {
                email: format!("{name}@localhost"),
                name,
            },
            (None, None) => Self {
                name: format!("uid-{uid}"),
                email: format!("uid-{uid}@localhost"),
            },
        }
    }

    fn trailer(&self) -> String {
        format!("Co-authored-by: {} <{}>", self.name, self.email)
    }
}

/// Read with git's own parser rather than by scanning for a `[user]` section:
/// includes and value quoting are where a hand-rolled reader goes quietly wrong.
///
/// Best effort. The agent runs as its own system user, so a home it cannot read
/// simply yields nothing.
fn git_identity(home: &Path) -> Option<(String, String)> {
    let mut file = gix::config::File::default();
    for candidate in [home.join(".gitconfig"), home.join(".config/git/config")] {
        if let Ok(parsed) =
            gix::config::File::from_path_no_includes(candidate, gix::config::Source::User)
        {
            file.append(parsed);
        }
    }
    let get = |key: &str| {
        file.string_by("user", None, key)
            .map(|v| v.to_string())
            .filter(|s| !s.is_empty())
    };
    Some((get("name")?, get("email")?))
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

    fn read_file_at_head(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        let commit = match self.repo.head_commit() {
            Ok(commit) => commit,
            Err(_) => return Ok(None), // unborn: nothing committed yet
        };
        let tree = commit.tree().map_err(git)?;
        match tree.lookup_entry_by_path(Path::new(rel)).map_err(git)? {
            Some(entry) => Ok(Some(entry.object().map_err(git)?.data.clone())),
            None => Ok(None),
        }
    }

    fn read_file_at(&self, commit: &str, rel: &str) -> Result<Option<Vec<u8>>> {
        let id = gix::ObjectId::from_hex(commit.as_bytes()).map_err(git)?;
        let commit = self.repo.find_object(id).map_err(git)?.try_into_commit().map_err(git)?;
        let tree = commit.tree().map_err(git)?;
        match tree.lookup_entry_by_path(Path::new(rel)).map_err(git)? {
            Some(entry) => Ok(Some(entry.object().map_err(git)?.data.clone())),
            None => Ok(None),
        }
    }

    fn is_dirty(&self) -> Result<bool> {
        let worktree = self.worktree_tree()?;
        match self.head_tree()? {
            Some(head) => Ok(worktree != head),
            None => Ok(worktree != gix::ObjectId::empty_tree(self.repo.object_hash())),
        }
    }

    fn commit_all(&self, message: &str, author: &Author, coauthors: &[Author]) -> Result<String> {
        let tree = self.worktree_tree()?;
        let parents: Vec<gix::ObjectId> = match self.repo.head_commit() {
            Ok(commit) => vec![commit.id().detach()],
            Err(_) => vec![],
        };

        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let time = format!("{secs} +0000");

        // The agent commits; the principal authors. Committing as the agent
        // keeps it independent of ambient git config a repository in /var/lib
        // would not carry.
        let committer = Author::agent();
        let committer = gix::actor::SignatureRef {
            name: BStr::new(committer.name.as_str()),
            email: BStr::new(committer.email.as_str()),
            time: &time,
        };
        let author = gix::actor::SignatureRef {
            name: BStr::new(author.name.as_str()),
            email: BStr::new(author.email.as_str()),
            time: &time,
        };

        let mut message = message.to_string();
        if !coauthors.is_empty() {
            message.push_str("\n\n");
            for other in coauthors {
                message.push_str(&other.trailer());
                message.push('\n');
            }
        }

        let id = self
            .repo
            .commit_as(committer, author, "HEAD", &message, tree, parents)
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
        assert!(repo.read_file_at_head("fractal.nix").unwrap().is_none());

        // Writing a file stages it.
        repo.write_file("fractal.nix", b"{ ... }: { }\n").unwrap();
        assert!(repo.is_dirty().unwrap());
        // Still nothing at HEAD until the change is committed.
        assert!(repo.read_file_at_head("fractal.nix").unwrap().is_none());

        // Applying commits it; the working copy is now clean and HEAD exists.
        let alice = Author { name: "alice".into(), email: "alice@localhost".into() };
        let first = repo.commit_all("initial", &alice, &[]).unwrap();
        assert_eq!(
            repo.read_file_at_head("fractal.nix").unwrap().as_deref(),
            Some(&b"{ ... }: { }\n"[..])
        );
        assert!(repo.read_file_at_head("absent.nix").unwrap().is_none());
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
        let second = repo.commit_all("edit", &alice, &[]).unwrap();
        assert_ne!(first, second);
        assert!(!repo.is_dirty().unwrap());

        // The file at an arbitrary commit, which is what makes history semantic.
        assert_eq!(
            repo.read_file_at(&first, "fractal.nix").unwrap().as_deref(),
            Some(&b"{ ... }: { }\n"[..])
        );
        assert!(repo.read_file_at(&first, "absent.nix").unwrap().is_none());
    }

    #[test]
    fn git_identity_is_read_from_either_config_location() {
        let home = tempfile::tempdir().unwrap();
        assert!(git_identity(home.path()).is_none(), "nothing to read yet");

        std::fs::write(
            home.path().join(".gitconfig"),
            "[user]\n\tname = Ada Lovelace\n\temail = ada@example.org\n",
        )
        .unwrap();
        assert_eq!(
            git_identity(home.path()),
            Some(("Ada Lovelace".into(), "ada@example.org".into()))
        );

        let xdg = home.path().join(".config/git");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::write(xdg.join("config"), "[user]\n\temail = ada@work.example\n").unwrap();
        assert_eq!(
            git_identity(home.path()).unwrap().1,
            "ada@work.example",
            "the later file wins, as git resolves it"
        );
    }

    /// A uid with no account still has to produce something committable, because
    /// an ugly attribution beats an absent one.
    #[test]
    fn an_unknown_uid_falls_back_to_the_bare_uid() {
        let author = Author::for_uid(u32::MAX);
        assert_eq!(author.name, format!("uid-{}", u32::MAX));
        assert!(author.email.contains(&u32::MAX.to_string()));
    }

    /// The applier authors, the agent commits, and everyone else who staged is
    /// credited in the message.
    #[test]
    fn commit_credits_the_applier_and_co_authors() {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        repo.write_file("fractal.nix", b"{ ... }: { }\n").unwrap();

        let alice = Author { name: "alice".into(), email: "alice@localhost".into() };
        let bob = Author { name: "bob".into(), email: "bob@localhost".into() };
        let id = repo.commit_all("apply", &alice, &[bob]).unwrap();

        let commit = repo.repo.find_object(gix::ObjectId::from_hex(id.as_bytes()).unwrap())
            .unwrap()
            .try_into_commit()
            .unwrap();
        assert_eq!(commit.author().unwrap().name, "alice");
        assert_eq!(commit.committer().unwrap().name, "fractal-agent");
        assert!(
            commit.message_raw().unwrap().to_string().contains("Co-authored-by: bob <bob@localhost>"),
            "got: {:?}",
            commit.message_raw().unwrap().to_string()
        );
    }
}
