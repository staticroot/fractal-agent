//! The catalog for a device that can evaluate its own flake.
//!
//! Everything the catalog needs comes from one evaluation, memoized against the
//! revision it read. Starting the evaluator and building the module system is
//! the cost; pulling forty values out of it rather than one is free, so asking
//! per key would be forty times the work for the same answer.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use serde::Deserialize;

use crate::catalog::{
    CatalogEntry, CatalogProvider, OptionMeta, OptionRead, Source, Stamped, standalone,
};
use crate::config::Value;
use crate::draft::{Uid, draft_ref};
use crate::error::{Error, Result};
use crate::nix;
use crate::repo::GitRepo;

/// Kept as a real Nix file so it is readable and parseable on its own.
const CATALOG_QUERY: &str = include_str!("../nix/catalog.nix");

#[derive(Debug, Clone, Deserialize)]
struct Entry {
    meta: Option<OptionMeta>,
    effective: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Queried {
    options: BTreeMap<String, Entry>,
    source: Option<String>,
}

pub struct LocalCatalog {
    repo: Arc<Mutex<GitRepo>>,
    /// Attribute path to the device's system inside the flake. Which path that
    /// is, is authority wiring, so the caller supplies it.
    system_path: String,
    /// One answer per reader, since a reader with a draft resolves against their
    /// own revision. The shared entry under `None` is the branch tip.
    cache: Mutex<HashMap<Option<Uid>, Cached>>,
    /// The store copy each reader's last evaluation fetched, so the one it
    /// supersedes can be dropped.
    fetched: Mutex<HashMap<Option<Uid>, String>>,
}

struct Cached {
    tree: String,
    entries: Vec<CatalogEntry>,
    effective: BTreeMap<String, Value>,
    as_of: Timestamp,
}

/// Which commit a reader's catalog resolves against, and under which reference
/// Lix may reach it.
struct Revision {
    reference: String,
    commit: String,
}

impl LocalCatalog {
    pub fn new(repo: Arc<Mutex<GitRepo>>, system_path: &str) -> Self {
        Self {
            repo,
            system_path: system_path.to_string(),
            cache: Mutex::new(HashMap::new()),
            fetched: Mutex::new(HashMap::new()),
        }
    }

    /// A reader with a draft sees what their own apply would produce; everybody
    /// else shares the branch tip.
    fn revision(&self, repo: &GitRepo, uid: Option<Uid>) -> Result<Option<Revision>> {
        if let Some(uid) = uid {
            let reference = draft_ref(uid);
            if let Some(commit) = repo.ref_sha(&reference)? {
                return Ok(Some(Revision { reference, commit }));
            }
        }
        let Some(commit) = repo.head()? else {
            return Ok(None);
        };
        Ok(Some(Revision { reference: "HEAD".to_string(), commit }))
    }

    fn cached(
        &self,
        uid: Option<Uid>,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<Option<Uid>, Cached>>> {
        let (url, tree) = {
            let repo = self.repo.lock().unwrap();
            let revision = self
                .revision(&repo, uid)?
                .ok_or_else(|| Error::Nix("nothing has been provisioned yet".into()))?;
            (
                nix::flake_url(repo.path(), &revision.reference, &revision.commit),
                repo.tree_id(&revision.commit)?,
            )
        };

        let mut held = self.cache.lock().unwrap();
        if held.get(&uid).is_some_and(|c| c.tree == tree) {
            return Ok(held);
        }

        let mut entries = standalone();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        let queried = query(&url, &self.system_path, &keys)?;
        let mut effective = BTreeMap::new();
        for entry in &mut entries {
            if let Some(found) = queried.options.get(&entry.key) {
                entry.meta = found.meta.clone();
                if let Some(value) = &found.effective {
                    effective.insert(entry.key.clone(), value.clone());
                }
            }
        }
        self.supersede(uid, queried.source);

        held.insert(uid, Cached { tree, entries, effective, as_of: Timestamp::now() });
        Ok(held)
    }

    /// Drop the source tree this reader's previous evaluation fetched. Failure
    /// is cosmetic; `nix.gc.automatic` collects what this misses.
    fn supersede(&self, uid: Option<Uid>, fetched: Option<String>) {
        let Some(fetched) = fetched.filter(|p| !p.is_empty()) else {
            return;
        };
        let previous = self.fetched.lock().unwrap().insert(uid, fetched.clone());
        if let Some(previous) = previous
            && previous != fetched
            && let Err(e) = nix::store_delete(&previous)
        {
            tracing::debug!("kept {previous}: {e}");
        }
    }
}

impl CatalogProvider for LocalCatalog {
    /// Declarations and defaults are the same for every reader, so this asks at
    /// the tip and shares the answer.
    fn entries(&self) -> Result<Vec<CatalogEntry>> {
        Ok(self.cached(None)?.get(&None).expect("just filled").entries.clone())
    }

    fn read(&self, key: &str, draft: Option<Value>, uid: Uid) -> Result<OptionRead> {
        let held = self.cached(Some(uid))?;
        let cache = held.get(&Some(uid)).expect("just filled");
        let stamp = |value: Value| Stamped {
            value,
            source: Source::LocalEvaluation,
            as_of: cache.as_of,
        };
        Ok(OptionRead {
            key: key.to_string(),
            draft,
            effective: cache.effective.get(key).cloned().map(stamp),
            declared: cache
                .entries
                .iter()
                .find(|e| e.key == key)
                .and_then(|e| e.meta.as_ref())
                .and_then(|m| m.default.clone())
                .map(stamp),
            runtime: None,
        })
    }

    fn warm(&self, uid: Uid) -> Result<()> {
        self.cached(Some(uid)).map(|_| ())
    }
}

/// Apply the query to its arguments. Only the application is generated; the
/// logic lives in the `.nix` file.
fn query_expr(url: &str, system_path: &str, keys: &[&str]) -> String {
    let list = keys.iter().map(|k| nix_str(k)).collect::<Vec<_>>().join(" ");
    format!(
        "({CATALOG_QUERY}) {{ url = {}; systemPath = {}; keys = [ {list} ]; }}",
        nix_str(url),
        nix_str(system_path),
    )
}

fn nix_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

fn query(url: &str, system_path: &str, keys: &[&str]) -> Result<Queried> {
    let json = nix::eval_expr(&query_expr(url, system_path, keys))?;
    Ok(serde_json::from_value(json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_reach_the_query() {
        let expr = query_expr(
            "git+file:///var/lib/fractal-agent/system-config?ref=HEAD&rev=abc",
            "nixosConfigurations.fractal",
            &["time.timeZone", "networking.firewall.enable"],
        );
        assert!(expr.contains(r#"keys = [ "time.timeZone" "networking.firewall.enable" ]"#));
        assert!(expr.contains(r#"systemPath = "nixosConfigurations.fractal""#));
        assert!(expr.contains(r#"rev=abc"#));
        // `.#` is a flake installable, invalid inside an expression.
        assert!(!expr.contains(".#"), "{expr}");
    }

    #[test]
    fn a_url_with_a_quote_cannot_escape_the_string() {
        let expr = query_expr(r#"git+file:///tmp/a"b\c"#, "o", &[]);
        assert!(expr.contains(r#"url = "git+file:///tmp/a\"b\\c""#), "{expr}");
    }

    /// The query is only ever seen by Nix, so check Nix accepts it. Skipped
    /// where Nix is absent rather than failing, since the rest of the suite has
    /// no such dependency.
    #[test]
    fn the_query_parses() {
        let expr = query_expr("git+file:///nonexistent?rev=abc", "a.b", &["x"]);
        let out = std::process::Command::new("nix-instantiate")
            .args(["--parse", "-E", &expr])
            .output();
        match out {
            Ok(out) => assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("{e}"),
        }
    }
}
