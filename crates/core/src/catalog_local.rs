//! The catalog for a device that can evaluate its own flake.
//!
//! Everything the catalog needs comes from one evaluation, memoized against the
//! configuration directory. Starting the evaluator and building the module
//! system is the cost; pulling forty values out of it rather than one is free,
//! so asking per key would be forty times the work for the same answer.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jiff::Timestamp;
use serde::Deserialize;

use crate::catalog::{
    CatalogEntry, CatalogProvider, OptionMeta, OptionRead, Source, Stamped, standalone,
};
use crate::config::Value;
use crate::error::Result;
use crate::nix;

/// Kept as a real Nix file so it is readable and parseable on its own.
const CATALOG_QUERY: &str = include_str!("../nix/catalog.nix");

#[derive(Debug, Clone, Deserialize)]
struct Entry {
    meta: Option<OptionMeta>,
    effective: Option<Value>,
}

pub struct LocalCatalog {
    dir: PathBuf,
    /// Attribute path to the device's system inside the flake. Which path that
    /// is, is authority wiring, so the caller supplies it.
    system_path: String,
    cache: Mutex<Option<Cached>>,
}

struct Cached {
    stamp: u64,
    entries: Vec<CatalogEntry>,
    effective: BTreeMap<String, Value>,
    as_of: Timestamp,
}

impl LocalCatalog {
    pub fn new(dir: impl Into<PathBuf>, system_path: &str) -> Self {
        Self {
            dir: dir.into(),
            system_path: system_path.to_string(),
            cache: Mutex::new(None),
        }
    }

    /// Re-evaluate when anything that could change the answer has changed.
    ///
    /// That is more than `flake.lock`: a human-authored module can declare
    /// options and can set values, so the whole configuration directory counts.
    /// Sizes and modification times, never contents, so this stays a few stats.
    fn stamp(dir: &Path) -> u64 {
        let mut files = Vec::new();
        collect(dir, &mut files);
        files.sort();
        let mut hasher = DefaultHasher::new();
        files.hash(&mut hasher);
        hasher.finish()
    }

    fn cached(&self) -> Result<std::sync::MutexGuard<'_, Option<Cached>>> {
        let stamp = Self::stamp(&self.dir);
        let mut held = self.cache.lock().unwrap();
        if held.as_ref().is_some_and(|c| c.stamp == stamp) {
            return Ok(held);
        }

        let mut entries = standalone();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        let queried = query(&self.dir, &self.system_path, &keys)?;
        let mut effective = BTreeMap::new();
        for entry in &mut entries {
            if let Some(found) = queried.get(&entry.key) {
                entry.meta = found.meta.clone();
                if let Some(value) = &found.effective {
                    effective.insert(entry.key.clone(), value.clone());
                }
            }
        }

        *held = Some(Cached {
            stamp,
            entries,
            effective,
            as_of: Timestamp::now(),
        });
        Ok(held)
    }
}

impl CatalogProvider for LocalCatalog {
    fn entries(&self) -> Result<Vec<CatalogEntry>> {
        Ok(self.cached()?.as_ref().expect("just filled").entries.clone())
    }

    fn read(&self, key: &str, staged: Option<Value>) -> Result<OptionRead> {
        let held = self.cached()?;
        let cache = held.as_ref().expect("just filled");
        let stamp = |value: Value| Stamped {
            value,
            source: Source::LocalEvaluation,
            as_of: cache.as_of,
        };
        Ok(OptionRead {
            key: key.to_string(),
            staged,
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
}

/// Relative path, size and modification time of every file under `dir`, which is
/// what the cache key is built from.
fn collect(dir: &Path, out: &mut Vec<(PathBuf, u64, Option<std::time::SystemTime>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => collect(&path, out),
            Ok(ft) if ft.is_file() => {
                if let Ok(meta) = entry.metadata() {
                    out.push((path, meta.len(), meta.modified().ok()));
                }
            }
            _ => {}
        }
    }
}

/// Apply the query to its arguments. Only the application is generated; the
/// logic lives in the `.nix` file.
fn query_expr(dir: &Path, system_path: &str, keys: &[&str]) -> String {
    let list = keys.iter().map(|k| nix_str(k)).collect::<Vec<_>>().join(" ");
    format!(
        "({CATALOG_QUERY}) {{ dir = {}; systemPath = {}; keys = [ {list} ]; }}",
        nix_str(&dir.to_string_lossy()),
        nix_str(system_path),
    )
}

fn nix_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

fn query(dir: &Path, system_path: &str, keys: &[&str]) -> Result<BTreeMap<String, Entry>> {
    let json = nix::eval_expr(dir, &query_expr(dir, system_path, keys))?;
    Ok(serde_json::from_value(json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stamp_tracks_every_file_not_just_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let empty = LocalCatalog::stamp(dir.path());

        std::fs::write(dir.path().join("flake.lock"), b"{}").unwrap();
        let locked = LocalCatalog::stamp(dir.path());
        assert_ne!(empty, locked);
        assert_eq!(locked, LocalCatalog::stamp(dir.path()), "and is stable");

        // A human-authored module can declare options and set values, so it has
        // to invalidate too.
        std::fs::write(dir.path().join("base.nix"), b"{ }").unwrap();
        assert_ne!(locked, LocalCatalog::stamp(dir.path()));
    }

    #[test]
    fn the_stamp_ignores_the_repository_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("flake.lock"), b"{}").unwrap();
        let before = LocalCatalog::stamp(dir.path());

        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/index"), b"whatever").unwrap();
        assert_eq!(before, LocalCatalog::stamp(dir.path()));
    }

    #[test]
    fn arguments_reach_the_query() {
        let expr = query_expr(
            Path::new("/var/lib/fractal-agent/system-config"),
            "nixosConfigurations.fractal",
            &["time.timeZone", "networking.firewall.enable"],
        );
        assert!(expr.contains(r#"keys = [ "time.timeZone" "networking.firewall.enable" ]"#));
        assert!(expr.contains(r#"systemPath = "nixosConfigurations.fractal""#));
        assert!(expr.contains(r#"dir = "/var/lib/fractal-agent/system-config""#));
        // `.#` is a flake installable, invalid inside an expression.
        assert!(!expr.contains(".#"), "{expr}");
    }

    #[test]
    fn a_path_with_a_quote_cannot_escape_the_string() {
        let expr = query_expr(Path::new(r#"/tmp/a"b\c"#), "o", &[]);
        assert!(expr.contains(r#"dir = "/tmp/a\"b\\c""#), "{expr}");
    }

    /// The query is only ever seen by Nix, so check Nix accepts it. Skipped
    /// where Nix is absent rather than failing, since the rest of the suite has
    /// no such dependency.
    #[test]
    fn the_query_parses() {
        let expr = query_expr(Path::new("/nonexistent"), "a.b", &["x"]);
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
