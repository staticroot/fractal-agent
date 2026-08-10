//! The catalog for a device that can evaluate its own flake.
//!
//! Reading an option's declared metadata means evaluating the module system,
//! which is expensive and changes only when the flake's inputs change. So it is
//! evaluated once and memoized against the lock file.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jiff::Timestamp;

use crate::catalog::{
    CatalogEntry, CatalogProvider, OptionMeta, OptionRead, Source, Stamped, standalone,
};
use crate::config::Value;
use crate::error::Result;
use crate::nix;

/// The metadata query, kept as a real Nix file so it is readable and parseable
/// on its own rather than as a string this module happens to concatenate.
const OPTIONS_META: &str = include_str!("../nix/options-meta.nix");

pub struct LocalCatalog {
    dir: PathBuf,
    /// Attribute path to the `options` tree inside the flake. Which path that is,
    /// is authority wiring, so the caller supplies it.
    options_path: String,
    /// Flake installable for the resolved `config` tree.
    config_attr: String,
    cache: Mutex<Option<Cached>>,
}

struct Cached {
    lock_digest: u64,
    entries: Vec<CatalogEntry>,
}

impl LocalCatalog {
    pub fn new(dir: impl Into<PathBuf>, options_path: &str, config_attr: &str) -> Self {
        Self {
            dir: dir.into(),
            options_path: options_path.to_string(),
            config_attr: config_attr.to_string(),
            cache: Mutex::new(None),
        }
    }

    /// A change detector, not a cryptographic digest: it only has to differ when
    /// the inputs differ.
    fn lock_digest(dir: &Path) -> u64 {
        let mut hasher = DefaultHasher::new();
        std::fs::read(dir.join("flake.lock")).unwrap_or_default().hash(&mut hasher);
        hasher.finish()
    }
}

impl CatalogProvider for LocalCatalog {
    fn entries(&self) -> Result<Vec<CatalogEntry>> {
        let digest = Self::lock_digest(&self.dir);
        if let Some(cached) = self.cache.lock().unwrap().as_ref()
            && cached.lock_digest == digest
        {
            return Ok(cached.entries.clone());
        }

        let mut entries = standalone();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        let meta = eval_meta(&self.dir, &self.options_path, &keys)?;
        for entry in &mut entries {
            entry.meta = meta.get(&entry.key).cloned().flatten();
        }

        *self.cache.lock().unwrap() = Some(Cached {
            lock_digest: digest,
            entries: entries.clone(),
        });
        Ok(entries)
    }

    fn read(&self, key: &str, staged: Option<Value>) -> Result<OptionRead> {
        let now = Timestamp::now();
        let local = |value: Value| Stamped {
            value,
            source: Source::LocalEvaluation,
            as_of: now,
        };

        // An option that does not resolve is not an error: an unset option with
        // no default has no effective value.
        let effective = nix::eval_attr(&self.dir, &format!("{}.{key}", self.config_attr))
            .ok()
            .map(local);
        let declared = self
            .entries()?
            .into_iter()
            .find(|e| e.key == key)
            .and_then(|e| e.meta)
            .and_then(|m| m.default)
            .map(local);

        Ok(OptionRead {
            key: key.to_string(),
            staged,
            effective,
            declared,
            runtime: None,
        })
    }
}

/// Apply the metadata query to its arguments. Only the application is generated;
/// the logic lives in the `.nix` file.
fn meta_expr(dir: &Path, options_path: &str, keys: &[&str]) -> String {
    let list = keys.iter().map(|k| nix_str(k)).collect::<Vec<_>>().join(" ");
    format!(
        "({OPTIONS_META}) {{ dir = {}; optionsPath = {}; keys = [ {list} ]; }}",
        nix_str(&dir.to_string_lossy()),
        nix_str(options_path),
    )
}

fn nix_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

fn eval_meta(
    dir: &Path,
    options_path: &str,
    keys: &[&str],
) -> Result<std::collections::BTreeMap<String, Option<OptionMeta>>> {
    let json = nix::eval_expr(dir, &meta_expr(dir, options_path, keys))?;
    Ok(serde_json::from_value(json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_digest_tracks_the_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let empty = LocalCatalog::lock_digest(dir.path());

        std::fs::write(dir.path().join("flake.lock"), b"{\"nodes\":{}}").unwrap();
        let one = LocalCatalog::lock_digest(dir.path());
        assert_ne!(empty, one);
        assert_eq!(one, LocalCatalog::lock_digest(dir.path()), "and is stable");

        std::fs::write(dir.path().join("flake.lock"), b"{\"nodes\":{\"x\":1}}").unwrap();
        assert_ne!(one, LocalCatalog::lock_digest(dir.path()));
    }

    #[test]
    fn arguments_reach_the_query() {
        let expr = meta_expr(
            Path::new("/var/lib/fractal-agent/system-config"),
            "nixosConfigurations.fractal.options",
            &["time.timeZone", "networking.firewall.enable"],
        );
        assert!(expr.contains(r#"keys = [ "time.timeZone" "networking.firewall.enable" ]"#));
        assert!(expr.contains(r#"optionsPath = "nixosConfigurations.fractal.options""#));
        assert!(expr.contains(r#"dir = "/var/lib/fractal-agent/system-config""#));
        // `.#` is a flake installable, invalid inside an expression.
        assert!(!expr.contains(".#"), "{expr}");
    }

    #[test]
    fn a_path_with_a_quote_cannot_escape_the_string() {
        let expr = meta_expr(Path::new(r#"/tmp/a"b\c"#), "o", &[]);
        assert!(expr.contains(r#"dir = "/tmp/a\"b\\c""#), "{expr}");
    }

    /// The query is only ever seen by Nix, so check Nix accepts it. Skipped
    /// where Nix is absent rather than failing, since the rest of the suite has
    /// no such dependency.
    #[test]
    fn the_query_parses() {
        let expr = meta_expr(Path::new("/nonexistent"), "a.b", &["x"]);
        let out = std::process::Command::new("nix-instantiate")
            .args(["--parse", "-E", &expr])
            .output();
        match out {
            Ok(out) => assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("{e}"),
        }
    }
}
