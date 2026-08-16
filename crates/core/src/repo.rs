//! The system configuration repository, tracked with an embedded version-control
//! library rather than by shelling out to git. It is bare, and every read names a
//! commit: the branch tip is the configuration that is running, a draft is a
//! commit at `refs/fractal/draft/<uid>`, and a candidate is one at
//! `refs/fractal/candidate/<uid>`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use gix::bstr::BStr;
use gix::objs::tree::EntryKind;

use crate::error::{Error, Result};

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

/// The agent's own bookkeeping, and the reference set
/// [`GitRepo::collect_garbage`] treats as roots.
pub const FRACTAL_REFS: &str = "refs/fractal/";

/// Paths to write into a tree, replacing whatever is already at them.
pub type TreeSpec<'a> = &'a [(&'a str, &'a [u8])];

pub struct GitRepo {
    repo: gix::Repository,
    path: PathBuf,
}

fn git<E: std::fmt::Display>(e: E) -> Error {
    Error::Git(e.to_string())
}

fn object_id(sha: &str) -> Result<gix::ObjectId> {
    gix::ObjectId::from_hex(sha.as_bytes()).map_err(git)
}

impl GitRepo {
    /// Open the repository at `path`, initializing it if it is not one yet.
    pub fn open_or_init(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let repo = match gix::open(&path) {
            Ok(repo) => repo,
            Err(_) => gix::init_bare(&path).map_err(git)?,
        };
        Ok(Self { repo, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn commit(&self, sha: &str) -> Result<gix::Commit<'_>> {
        self.repo
            .find_object(object_id(sha)?)
            .map_err(git)?
            .try_into_commit()
            .map_err(git)
    }

    pub fn read_blob(&self, commit: &str, rel: &str) -> Result<Option<Vec<u8>>> {
        let tree = self.commit(commit)?.tree().map_err(git)?;
        match tree.lookup_entry_by_path(Path::new(rel)).map_err(git)? {
            Some(entry) => Ok(Some(entry.object().map_err(git)?.data.clone())),
            None => Ok(None),
        }
    }

    pub fn blob_id(&self, commit: &str, rel: &str) -> Result<Option<String>> {
        let tree = self.commit(commit)?.tree().map_err(git)?;
        Ok(tree
            .lookup_entry_by_path(Path::new(rel))
            .map_err(git)?
            .map(|entry| entry.object_id().to_string()))
    }

    pub fn list_blobs(&self, commit: &str) -> Result<BTreeMap<String, String>> {
        let tree = self.commit(commit)?.tree().map_err(git)?;
        let mut out = BTreeMap::new();
        walk_tree(&tree, "", &mut out)?;
        Ok(out)
    }

    pub fn tree_id(&self, commit: &str) -> Result<String> {
        Ok(self.commit(commit)?.tree_id().map_err(git)?.detach().to_string())
    }

    /// Write a commit that takes `from`'s tree with `files` replaced, under
    /// `parent`. Nothing else writes an object.
    ///
    /// Both are `None` for a repository's first commit, which starts from an
    /// empty tree and descends from nothing. They differ from each other
    /// whenever a commit is rewritten onto a new parent.
    pub fn commit_tree(
        &self,
        from: Option<&str>,
        parent: Option<&str>,
        files: TreeSpec<'_>,
        author: &Author,
        message: &str,
    ) -> Result<String> {
        let base = match from {
            Some(from) => self.commit(from)?.tree_id().map_err(git)?.detach(),
            None => gix::ObjectId::empty_tree(self.repo.object_hash()),
        };
        let mut editor = self.repo.edit_tree(base).map_err(git)?;
        for (rel, bytes) in files {
            let blob = self.repo.write_blob(bytes).map_err(git)?.detach();
            editor.upsert(*rel, EntryKind::Blob, blob).map_err(git)?;
        }
        let tree = editor.write().map_err(git)?.detach();
        self.write_commit(tree, parent, author, message)
    }

    fn write_commit(
        &self,
        tree: gix::ObjectId,
        parent: Option<&str>,
        author: &Author,
        message: &str,
    ) -> Result<String> {
        let parents: Vec<gix::ObjectId> = match parent {
            Some(parent) => vec![object_id(parent)?],
            None => vec![],
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

        let commit = gix::objs::Commit {
            tree,
            parents: parents.into(),
            author: author.to_owned().map_err(git)?,
            committer: committer.to_owned().map_err(git)?,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        Ok(self.repo.write_object(&commit).map_err(git)?.detach().to_string())
    }

    pub fn set_ref(&self, name: &str, commit: &str) -> Result<()> {
        self.repo
            .reference(name, object_id(commit)?, gix::refs::transaction::PreviousValue::Any, "set")
            .map_err(git)?;
        Ok(())
    }

    /// Deleting a reference that is not there is not a failure: every caller is
    /// establishing that it is gone rather than observing that it was.
    pub fn delete_ref(&self, name: &str) -> Result<()> {
        match self.repo.find_reference(name) {
            Ok(reference) => reference.delete().map_err(git),
            Err(_) => Ok(()),
        }
    }

    pub fn ref_sha(&self, name: &str) -> Result<Option<String>> {
        match self.repo.find_reference(name) {
            Ok(mut reference) => Ok(Some(reference.peel_to_id().map_err(git)?.detach().to_string())),
            Err(_) => Ok(None),
        }
    }

    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let platform = self.repo.references().map_err(git)?;
        let iter = platform.prefixed(prefix.as_bytes()).map_err(git)?;
        let mut out = Vec::new();
        for reference in iter {
            let mut reference = reference.map_err(git)?;
            let name = reference.name().as_bstr().to_string();
            let id = reference.peel_to_id().map_err(git)?.detach().to_string();
            out.push((name, id));
        }
        out.sort();
        Ok(out)
    }

    /// Point the branch at `commit`, forwards for an activation or backwards for
    /// a rollback.
    pub fn advance(&self, commit: &str) -> Result<()> {
        let name = self.branch()?;
        self.set_ref(&name, commit)
    }

    pub fn parent_of(&self, commit: &str) -> Result<Option<String>> {
        Ok(self.commit(commit)?.parent_ids().next().map(|id| id.detach().to_string()))
    }

    /// The current commit hash, or `None` if the repository is unborn.
    pub fn head(&self) -> Result<Option<String>> {
        match self.repo.head_commit() {
            Ok(commit) => Ok(Some(commit.id().detach().to_string())),
            Err(_) => Ok(None),
        }
    }

    /// Delete every loose object nothing references.
    ///
    /// A full walk rather than bookkeeping at the moment of supersession: the
    /// repository holds one small configuration and the agent is its only
    /// writer, so reachability is cheap to recompute and impossible to get
    /// subtly wrong.
    pub fn collect_garbage(&self) -> Result<()> {
        let mut roots = Vec::new();
        if let Some(head) = self.head()? {
            roots.push(head);
        }
        roots.extend(self.list_refs(FRACTAL_REFS)?.into_iter().map(|(_, sha)| sha));

        let mut reached = BTreeSet::new();
        for root in roots {
            self.reach_commit(&root, &mut reached)?;
        }

        let objects = self.path.join("objects");
        for shard in std::fs::read_dir(&objects).map_err(|e| Error::io(&objects, e))?.flatten() {
            let name = shard.file_name().to_string_lossy().into_owned();
            if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            for object in std::fs::read_dir(shard.path()).map_err(|e| Error::io(shard.path(), e))?.flatten() {
                let id = format!("{name}{}", object.file_name().to_string_lossy());
                if !reached.contains(&id) {
                    std::fs::remove_file(object.path()).map_err(|e| Error::io(object.path(), e))?;
                }
            }
        }
        Ok(())
    }

    fn reach_commit(&self, sha: &str, reached: &mut BTreeSet<String>) -> Result<()> {
        let mut next = vec![sha.to_string()];
        while let Some(sha) = next.pop() {
            if !reached.insert(sha.clone()) {
                continue;
            }
            let commit = self.commit(&sha)?;
            next.extend(commit.parent_ids().map(|id| id.detach().to_string()));
            let tree = commit.tree().map_err(git)?;
            self.reach_tree(&tree, reached)?;
        }
        Ok(())
    }

    fn reach_tree(&self, tree: &gix::Tree<'_>, reached: &mut BTreeSet<String>) -> Result<()> {
        reached.insert(tree.id().detach().to_string());
        for entry in tree.iter() {
            let entry = entry.map_err(git)?;
            let id = entry.object_id().to_string();
            if entry.mode().is_tree() {
                let sub = self.repo.find_object(entry.object_id()).map_err(git)?.try_into_tree().map_err(git)?;
                self.reach_tree(&sub, reached)?;
            } else {
                reached.insert(id);
            }
        }
        Ok(())
    }

    /// The branch HEAD names, whether or not it exists yet: an unborn repository
    /// has a symbolic HEAD and no commit, and the first activation creates it.
    fn branch(&self) -> Result<String> {
        let head = self.repo.head().map_err(git)?;
        match head.referent_name() {
            Some(name) => Ok(name.as_bstr().to_string()),
            None => Err(Error::Git("HEAD is detached".into())),
        }
    }
}

fn walk_tree(tree: &gix::Tree<'_>, prefix: &str, out: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in tree.iter() {
        let entry = entry.map_err(git)?;
        let name = entry.filename().to_string();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode().is_tree() {
            let sub = entry.object().map_err(git)?.try_into_tree().map_err(git)?;
            walk_tree(&sub, &path, out)?;
        } else {
            out.insert(path, entry.object_id().to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alice() -> Author {
        Author { name: "alice".into(), email: "alice@localhost".into() }
    }

    fn open() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::open_or_init(dir.path()).unwrap();
        (dir, repo)
    }

    fn commit(repo: &GitRepo, parent: Option<&str>, contents: &[u8]) -> String {
        repo.commit_tree(parent, parent, &[("fractal.nix", contents)], &alice(), "edit").unwrap()
    }

    #[test]
    fn a_commit_becomes_history_only_when_the_branch_is_moved() {
        let (_dir, repo) = open();
        assert!(repo.head().unwrap().is_none(), "unborn");

        let first = commit(&repo, None, b"{ ... }: { }\n");
        assert!(repo.head().unwrap().is_none(), "the branch has not moved");
        assert_eq!(
            repo.read_blob(&first, "fractal.nix").unwrap().as_deref(),
            Some(&b"{ ... }: { }\n"[..])
        );

        repo.advance(&first).unwrap();
        assert_eq!(repo.head().unwrap().as_deref(), Some(first.as_str()));

        let second = commit(&repo, Some(&first), b"{ ... }: { x = 1; }\n");
        assert_eq!(repo.parent_of(&second).unwrap().as_deref(), Some(first.as_str()));
        assert_eq!(repo.parent_of(&first).unwrap(), None);
        assert!(repo.read_blob(&second, "absent.nix").unwrap().is_none());
    }

    /// A rollback moves the branch backwards, so the state it left has to stay
    /// reachable under a name of its own.
    #[test]
    fn a_kept_commit_survives_the_branch_moving_off_it() {
        let (dir, repo) = open();
        let first = commit(&repo, None, b"{ ... }: { }\n");
        repo.advance(&first).unwrap();
        let second = commit(&repo, Some(&first), b"{ ... }: { x = 1; }\n");
        repo.set_ref("refs/fractal/activated/2", &second).unwrap();
        repo.advance(&second).unwrap();

        repo.advance(&first).unwrap();
        repo.collect_garbage().unwrap();
        assert_eq!(
            repo.read_blob(&second, "fractal.nix").unwrap().as_deref(),
            Some(&b"{ ... }: { x = 1; }\n"[..]),
            "still readable off the branch"
        );
        assert!(dir.path().join("refs/fractal/activated/2").exists());
    }

    /// The whole point of the bare repository: `nix` reads it, and it reads it by
    /// revision. Skipped where git is absent.
    #[test]
    fn a_commit_leaves_a_repository_git_can_read() {
        let (dir, repo) = open();
        let scaffold = repo
            .commit_tree(None, None, &[("flake.nix", &b"{ outputs = _: { }; }\n"[..])], &alice(), "scaffold")
            .unwrap();
        repo.advance(&scaffold).unwrap();

        let out = std::process::Command::new("git")
            .args(["cat-file", "-p", &format!("{scaffold}:flake.nix")])
            .current_dir(dir.path())
            .output();
        match out {
            Ok(out) => {
                assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
                assert_eq!(out.stdout, b"{ outputs = _: { }; }\n");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn a_commit_keeps_the_files_it_does_not_name() {
        let (_dir, repo) = open();
        let base = repo
            .commit_tree(
                None,
                None,
                &[("base.nix", &b"{ }\n"[..]), ("nested/thing.nix", &b"1\n"[..])],
                &alice(),
                "base",
            )
            .unwrap();
        let next = commit(&repo, Some(&base), b"{ ... }: { }\n");

        assert_eq!(
            repo.list_blobs(&next).unwrap().keys().cloned().collect::<Vec<_>>(),
            ["base.nix", "fractal.nix", "nested/thing.nix"]
        );
        assert_eq!(repo.read_blob(&next, "nested/thing.nix").unwrap().as_deref(), Some(&b"1\n"[..]));
        assert_eq!(
            repo.blob_id(&next, "base.nix").unwrap(),
            repo.blob_id(&base, "base.nix").unwrap(),
            "an untouched file keeps its identity"
        );
    }

    /// The candidate's shape: one commit's tree onto another commit's parent.
    #[test]
    fn a_commit_can_take_its_tree_from_somewhere_other_than_its_parent() {
        let (_dir, repo) = open();
        let tip = commit(&repo, None, b"{ ... }: { }\n");
        repo.advance(&tip).unwrap();
        let draft = commit(&repo, Some(&tip), b"{ ... }: { x = 1; }\n");

        let candidate = repo.commit_tree(Some(&draft), Some(&tip), &[], &alice(), "apply").unwrap();
        assert_eq!(repo.tree_id(&candidate).unwrap(), repo.tree_id(&draft).unwrap());
        assert_eq!(repo.parent_of(&candidate).unwrap().as_deref(), Some(tip.as_str()));
        assert_ne!(candidate, draft, "a different message is a different commit");
    }

    #[test]
    fn references_are_listed_by_prefix_and_deleted_idempotently() {
        let (_dir, repo) = open();
        let a = commit(&repo, None, b"a\n");
        let b = commit(&repo, None, b"b\n");
        repo.set_ref("refs/fractal/draft/1000", &a).unwrap();
        repo.set_ref("refs/fractal/candidate/1000", &b).unwrap();

        assert_eq!(
            repo.list_refs("refs/fractal/draft/").unwrap(),
            [("refs/fractal/draft/1000".to_string(), a.clone())]
        );
        assert_eq!(repo.list_refs(FRACTAL_REFS).unwrap().len(), 2);
        assert_eq!(repo.ref_sha("refs/fractal/draft/1000").unwrap().as_deref(), Some(a.as_str()));

        repo.delete_ref("refs/fractal/draft/1000").unwrap();
        repo.delete_ref("refs/fractal/draft/1000").unwrap();
        assert!(repo.ref_sha("refs/fractal/draft/1000").unwrap().is_none());
    }

    #[test]
    fn collecting_keeps_what_a_reference_reaches_and_drops_the_rest() {
        let (_dir, repo) = open();
        let tip = commit(&repo, None, b"{ ... }: { }\n");
        repo.advance(&tip).unwrap();
        let kept = commit(&repo, Some(&tip), b"{ ... }: { kept = 1; }\n");
        repo.set_ref("refs/fractal/draft/1000", &kept).unwrap();
        let dropped = commit(&repo, Some(&tip), b"{ ... }: { dropped = 1; }\n");

        repo.collect_garbage().unwrap();
        assert!(repo.read_blob(&kept, "fractal.nix").unwrap().is_some());
        assert!(repo.read_blob(&dropped, "fractal.nix").is_err(), "unreferenced and gone");

        repo.delete_ref("refs/fractal/draft/1000").unwrap();
        repo.collect_garbage().unwrap();
        assert!(repo.read_blob(&kept, "fractal.nix").is_err());
        assert!(repo.read_blob(&tip, "fractal.nix").unwrap().is_some(), "the branch still holds");
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

    #[test]
    fn the_principal_authors_and_the_agent_commits() {
        let (_dir, repo) = open();
        let id = commit(&repo, None, b"{ ... }: { }\n");
        let commit = repo.commit(&id).unwrap();
        assert_eq!(commit.author().unwrap().name, "alice");
        assert_eq!(commit.committer().unwrap().name, "fractal-agent");
    }
}
