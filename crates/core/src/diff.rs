//! History and diff shown to the user are semantic, not the raw text delta the
//! version-control backend produces: an option-level and closure-level
//! difference computed from the evaluated configurations, so the user sees "the
//! firewall was turned on" rather than "line 34 changed". The option-tree diff
//! here is pure; the closure diff is Nix's own `diff-closures`, assembled by the
//! caller.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::config::Value;

/// One option that differs between two evaluated configurations. `before`/`after`
/// are `None` when the key is absent on that side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionChange {
    pub key: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

/// The whole semantic difference between two generations: what changed at the
/// option level, and the closure-level package/version delta beneath it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticDiff {
    pub options: Vec<OptionChange>,
    /// Closure-level package delta from `nix store diff-closures`, filled by the caller.
    pub closure: ClosureDiff,
}

/// Package/version delta between two store closures, parsed from
/// `nix store diff-closures --json`. Keyed by package name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClosureDiff {
    pub packages: BTreeMap<String, PackageDelta>,
}

/// How one package changed across the closure: the versions present before and
/// after, and the signed change in total size (bytes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageDelta {
    pub size_delta: i64,
    pub versions_before: Vec<String>,
    pub versions_after: Vec<String>,
}

/// Compare two maps of evaluated option values, keyed by option path. Returns
/// only the keys that differ, sorted, so the view is stable.
pub fn option_diff(
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
) -> Vec<OptionChange> {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter_map(|key| {
            let b = before.get(key);
            let a = after.get(key);
            (b != a).then(|| OptionChange {
                key: key.clone(),
                before: b.cloned(),
                after: a.cloned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn reports_added_removed_changed_only() {
        let before = map(&[
            ("networking.firewall.enable", Value::Bool(false)),
            ("time.timeZone", Value::Str("UTC".into())),
            ("services.openssh.enable", Value::Bool(true)),
        ]);
        let after = map(&[
            ("networking.firewall.enable", Value::Bool(true)), // changed
            ("time.timeZone", Value::Str("UTC".into())),       // unchanged: omitted
            ("services.printing.enable", Value::Bool(true)),   // added
            // services.openssh.enable removed
        ]);

        let d = option_diff(&before, &after);
        assert_eq!(
            d,
            vec![
                OptionChange {
                    key: "networking.firewall.enable".into(),
                    before: Some(Value::Bool(false)),
                    after: Some(Value::Bool(true)),
                },
                OptionChange {
                    key: "services.openssh.enable".into(),
                    before: Some(Value::Bool(true)),
                    after: None,
                },
                OptionChange {
                    key: "services.printing.enable".into(),
                    before: None,
                    after: Some(Value::Bool(true)),
                },
            ]
        );
    }

    #[test]
    fn identical_configs_have_no_diff() {
        let m = map(&[("a", Value::Int(1))]);
        assert!(option_diff(&m, &m).is_empty());
    }
}
