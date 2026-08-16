//! The configuration the agent owns, held as structured data and projected to a
//! Nix file through one pure serializer. The file is never edited as text: an
//! edit reads the model, sets one key, and re-serializes the whole thing, so the
//! same model always produces the same bytes and the git history carries only
//! real changes. This totality is why written values are restricted to plain
//! data — no functions, let-bindings, or imports.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub use fractal_protocol::config::Value;

/// One leaf's worth of intent: a value to set, or `None` to remove what is
/// there. `None` is not `Value::Null`, which is a value somebody may mean.
pub type Change = (String, Option<Value>);

/// A tree of attribute names down to plain-data leaves, which is the shape
/// evaluation returns. Option keys are dotted paths into it, so
/// `services.openssh.settings` reads as the subtree and
/// `services.openssh.settings.PermitRootLogin` as the string, both correct.
///
/// A tree rather than a flat map of dotted keys, because `a.b = { c = 1; }` and
/// `a.b.c = 1` are indistinguishable once evaluated, so a flat model would have
/// to guess where the key ended, with nothing able to settle the guess. With a
/// tree on both sides, reading the file back is the identity function.
///
/// `BTreeMap` fixes iteration order, which is half of what makes serialization
/// canonical.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Model {
    root: BTreeMap<String, Value>,
}

/// An empty attrset is a leaf in its own right: a value somebody drafted, not a
/// subtree with no children. Treating it as one made the key vanish on the next
/// read.
fn is_subtree(value: &Value) -> bool {
    matches!(value, Value::Attrs(attrs) if !attrs.is_empty())
}

impl Model {
    pub fn new() -> Self {
        Self::default()
    }

    /// The value at a dotted path, materialized. A subtree comes back as
    /// `Value::Attrs`, which is why this returns an owned value rather than a
    /// borrow: a subtree is assembled from the tree, not stored beside it.
    pub fn get(&self, key: &str) -> Option<Value> {
        let mut level = &self.root;
        let mut segments = key.split('.').peekable();
        while let Some(segment) = segments.next() {
            let value = level.get(segment)?;
            if segments.peek().is_none() {
                return Some(value.clone());
            }
            match value {
                Value::Attrs(attrs) => level = attrs,
                _ => return None,
            }
        }
        None
    }

    /// Every leaf, keyed by its dotted path, for comparing two models with
    /// [`crate::diff`]. Comparing leaves loses nothing as long as an empty
    /// attrset counts as a leaf, which it does.
    pub fn leaves(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        collect_leaves("", &self.root, &mut out);
        out
    }

    /// Set the value at a dotted path, returning the previous value there.
    ///
    /// Refused as a conflict when the path would swallow an existing subtree, or
    /// when it descends beneath a key already holding a leaf. Replacing silently
    /// in either case would discard configuration the caller never mentioned,
    /// which is exactly what the model exists to make impossible.
    pub fn set(&mut self, key: &str, value: Value) -> Result<Option<Value>> {
        let segments: Vec<&str> = key.split('.').collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(Error::Conflict(format!("not a valid option path: {key:?}")));
        }
        let (last, parents) = segments.split_last().expect("split yields at least one segment");

        let mut level = &mut self.root;
        for (depth, segment) in parents.iter().enumerate() {
            let node = level
                .entry((*segment).to_string())
                .or_insert_with(|| Value::Attrs(BTreeMap::new()));
            match node {
                Value::Attrs(attrs) => level = attrs,
                _ => {
                    let held = segments[..=depth].join(".");
                    return Err(Error::Conflict(format!(
                        "cannot set {key}: {held} already holds a value"
                    )));
                }
            }
        }

        if level.get(*last).is_some_and(is_subtree) {
            return Err(Error::Conflict(format!(
                "cannot set {key}: it would replace a subtree of existing settings"
            )));
        }
        Ok(level.insert((*last).to_string(), value))
    }

    /// Remove the value at a dotted path. Ancestors left empty by the removal are
    /// pruned, because an intermediate node with no children is not a value
    /// anybody set and writing `a.b = { };` for it would mean something else.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let segments: Vec<&str> = key.split('.').collect();
        remove_at(&mut self.root, &segments)
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_empty()
    }

    /// Reconstruct the model from the JSON that evaluating the generated module
    /// yields. Both sides are trees, so this is the identity function: no
    /// flattening, and nothing to guess about where an option key ended.
    pub fn from_eval_json(json: &serde_json::Value) -> Self {
        match json_to_value(json) {
            Value::Attrs(root) => Self { root },
            _ => Self::default(),
        }
    }

    pub fn diff(&self, base: &Model) -> Vec<Change> {
        let (before, after) = (base.leaves(), self.leaves());
        let mut out = Vec::new();
        for (key, value) in &after {
            if before.get(key) != Some(value) {
                out.push((key.clone(), Some(value.clone())));
            }
        }
        for key in before.keys() {
            if !after.contains_key(key) {
                out.push((key.clone(), None));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn apply(&mut self, changes: &[Change]) -> Result<()> {
        for (key, change) in changes {
            self.apply_change(key, change.clone())?;
        }
        Ok(())
    }

    /// Set the value, or remove what is at the key when there is none.
    pub fn apply_change(&mut self, key: &str, change: Option<Value>) -> Result<()> {
        match change {
            Some(value) => {
                self.set(key, value)?;
            }
            None => {
                self.remove(key);
            }
        }
        Ok(())
    }

    /// Project the whole model to a canonical NixOS module. Pure function of the
    /// model: identical models yield byte-identical output. Chains of single
    /// attribute names are written as dotted paths, so the file reads the way a
    /// person would have written it.
    pub fn to_nix(&self) -> String {
        let mut out = String::new();
        out.push_str("# Generated by fractal-agent — do not edit by hand.\n");
        out.push_str("{ ... }:\n");
        if self.root.is_empty() {
            out.push_str("{ }\n");
            return out;
        }
        out.push_str("{\n");
        write_definitions(&mut out, "", &self.root);
        out.push_str("}\n");
        out
    }
}

/// Emit one `path = value;` definition per leaf, joining attribute names with
/// dots on the way down.
fn write_definitions(out: &mut String, prefix: &str, level: &BTreeMap<String, Value>) {
    for (name, value) in level {
        let mut path = String::with_capacity(prefix.len() + name.len() + 1);
        path.push_str(prefix);
        if !prefix.is_empty() {
            path.push('.');
        }
        write_attr_name(&mut path, name);
        match value {
            Value::Attrs(attrs) if !attrs.is_empty() => write_definitions(out, &path, attrs),
            leaf => {
                out.push_str("  ");
                out.push_str(&path);
                out.push_str(" = ");
                write_value(out, leaf, 1);
                out.push_str(";\n");
            }
        }
    }
}

fn collect_leaves(prefix: &str, level: &BTreeMap<String, Value>, out: &mut BTreeMap<String, Value>) {
    for (name, value) in level {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match value {
            Value::Attrs(attrs) if !attrs.is_empty() => collect_leaves(&path, attrs, out),
            leaf => {
                out.insert(path, leaf.clone());
            }
        }
    }
}

/// Remove `segments` from `level`, pruning any ancestor the removal empties.
fn remove_at(level: &mut BTreeMap<String, Value>, segments: &[&str]) -> Option<Value> {
    let (first, rest) = segments.split_first()?;
    if rest.is_empty() {
        return level.remove(*first);
    }
    let Some(Value::Attrs(attrs)) = level.get_mut(*first) else {
        return None;
    };
    let removed = remove_at(attrs, rest);
    if attrs.is_empty() {
        level.remove(*first);
    }
    removed
}

/// Convert one evaluated JSON value to a plain-data [`Value`]. A number is an
/// integer when it fits, else a float; objects map to attrs, which on the way in
/// from evaluation are the model's subtrees.
fn json_to_value(json: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match json {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        J::String(s) => Value::Str(s.clone()),
        J::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        J::Object(map) => {
            Value::Attrs(map.iter().map(|(k, v)| (k.clone(), json_to_value(v))).collect())
        }
    }
}

fn write_value(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),
        // `{:?}` keeps a decimal point so a float never serializes as an int.
        Value::Float(f) => out.push_str(&format!("{f:?}")),
        Value::Str(s) => write_string(out, s),
        Value::List(items) => {
            if items.is_empty() {
                out.push_str("[ ]");
                return;
            }
            out.push_str("[\n");
            let pad = "  ".repeat(indent + 1);
            for item in items {
                out.push_str(&pad);
                write_value(out, item, indent + 1);
                out.push('\n');
            }
            out.push_str(&"  ".repeat(indent));
            out.push(']');
        }
        Value::Attrs(attrs) => {
            if attrs.is_empty() {
                out.push_str("{ }");
                return;
            }
            out.push_str("{\n");
            let pad = "  ".repeat(indent + 1);
            for (k, v) in attrs {
                out.push_str(&pad);
                write_attr_name(out, k);
                out.push_str(" = ");
                write_value(out, v, indent + 1);
                out.push_str(";\n");
            }
            out.push_str(&"  ".repeat(indent));
            out.push('}');
        }
    }
}

/// A Nix double-quoted string with every `$` escaped, so a drafted string value
/// can never open an interpolation (`${...}`) or otherwise escape into the
/// expression. This is the config-layer form of "drafted values are plain data".
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Emit an attribute name, quoting it unless it is a bare Nix identifier.
fn write_attr_name(out: &mut String, name: &str) {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '\'')
        && !name.chars().next().unwrap().is_ascii_digit();
    if bare {
        out.push_str(name);
    } else {
        write_string(out, name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, Value)]) -> Value {
        Value::Attrs(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    #[test]
    fn empty_model() {
        assert_eq!(
            Model::new().to_nix(),
            "# Generated by fractal-agent — do not edit by hand.\n{ ... }:\n{ }\n"
        );
    }

    /// Set that must succeed, for tests that are not about conflicts.
    fn set(m: &mut Model, key: &str, value: Value) {
        m.set(key, value).expect("no conflict");
    }

    #[test]
    fn canonical_and_deterministic() {
        let mut a = Model::new();
        set(&mut a, "networking.hostName", Value::Str("box".into()));
        set(&mut a, "networking.firewall.enable", Value::Bool(true));
        set(&mut a, "time.timeZone", Value::Str("UTC".into()));

        // Insertion order must not matter: same options, different order, same bytes.
        let mut b = Model::new();
        set(&mut b, "time.timeZone", Value::Str("UTC".into()));
        set(&mut b, "networking.hostName", Value::Str("box".into()));
        set(&mut b, "networking.firewall.enable", Value::Bool(true));

        assert_eq!(a.to_nix(), b.to_nix());
        assert_eq!(
            a.to_nix(),
            "# Generated by fractal-agent — do not edit by hand.\n\
             { ... }:\n\
             {\n  \
               networking.firewall.enable = true;\n  \
               networking.hostName = \"box\";\n  \
               time.timeZone = \"UTC\";\n\
             }\n"
        );
    }

    #[test]
    fn escapes_interpolation_and_quotes() {
        let mut m = Model::new();
        set(&mut m, "services.foo.motd", Value::Str("hi ${IFS} \"x\" \\ end".into()));
        let out = m.to_nix();
        assert!(out.contains(r#"= "hi \${IFS} \"x\" \\ end";"#), "got: {out}");
    }

    /// An attrset value drafted at a path is the same tree as the equivalent
    /// dotted keys, so both spellings project identically and read back the same.
    #[test]
    fn attrs_value_and_dotted_keys_are_one_tree() {
        let mut whole = Model::new();
        set(
            &mut whole,
            "services.x.settings",
            attrs(&[
                ("ports", Value::List(vec![Value::Int(22), Value::Int(80)])),
                ("names", Value::List(vec![])),
            ]),
        );

        let mut piecewise = Model::new();
        set(
            &mut piecewise,
            "services.x.settings.ports",
            Value::List(vec![Value::Int(22), Value::Int(80)]),
        );
        set(&mut piecewise, "services.x.settings.names", Value::List(vec![]));

        assert_eq!(whole, piecewise);
        let out = whole.to_nix();
        assert!(out.contains("services.x.settings.ports = [\n"), "got: {out}");
        assert!(out.contains("services.x.settings.names = [ ];"), "got: {out}");
    }

    /// The subtree and the leaf beneath it are both readable, and both answers
    /// are correct at the same time.
    #[test]
    fn a_path_reads_as_both_subtree_and_leaf() {
        let mut m = Model::new();
        set(&mut m, "services.openssh.settings.PermitRootLogin", Value::Str("no".into()));

        assert_eq!(
            m.get("services.openssh.settings"),
            Some(attrs(&[("PermitRootLogin", Value::Str("no".into()))]))
        );
        assert_eq!(
            m.get("services.openssh.settings.PermitRootLogin"),
            Some(Value::Str("no".into()))
        );
    }

    /// The bug the tree model fixes: an empty attrset is a value somebody set,
    /// not a subtree with nothing in it, so it survives the round trip.
    #[test]
    fn empty_attrs_is_a_leaf_and_survives() {
        let mut m = Model::new();
        set(&mut m, "services.x.settings", attrs(&[]));

        assert!(m.to_nix().contains("services.x.settings = { };"), "{}", m.to_nix());
        assert_eq!(m.leaves().get("services.x.settings"), Some(&attrs(&[])));

        let evaluated = serde_json::json!({ "services": { "x": { "settings": {} } } });
        assert_eq!(Model::from_eval_json(&evaluated), m);
    }

    #[test]
    fn set_refuses_to_swallow_a_subtree() {
        let mut m = Model::new();
        set(&mut m, "services.x.settings.a", Value::Int(1));

        let err = m.set("services.x.settings", Value::Int(2)).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
        // The refusal left the existing setting untouched.
        assert_eq!(m.get("services.x.settings.a"), Some(Value::Int(1)));
    }

    #[test]
    fn set_refuses_to_descend_beneath_a_leaf() {
        let mut m = Model::new();
        set(&mut m, "networking.hostName", Value::Str("box".into()));

        let err = m.set("networking.hostName.extra", Value::Int(1)).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
        assert_eq!(m.get("networking.hostName"), Some(Value::Str("box".into())));
    }

    /// Replacing an empty attrset is not swallowing anything, so it is allowed.
    #[test]
    fn set_over_an_empty_attrs_is_allowed() {
        let mut m = Model::new();
        set(&mut m, "services.x.settings", attrs(&[]));
        assert_eq!(m.set("services.x.settings", Value::Int(1)).unwrap(), Some(attrs(&[])));
    }

    #[test]
    fn remove_prunes_the_ancestors_it_empties() {
        let mut m = Model::new();
        set(&mut m, "networking.firewall.enable", Value::Bool(true));
        set(&mut m, "networking.hostName", Value::Str("box".into()));

        assert_eq!(m.remove("networking.firewall.enable"), Some(Value::Bool(true)));
        // `networking` survives because hostName still lives there.
        assert!(m.get("networking.firewall").is_none());
        assert_eq!(m.get("networking.hostName"), Some(Value::Str("box".into())));

        assert_eq!(m.remove("networking.hostName"), Some(Value::Str("box".into())));
        assert!(m.is_empty(), "the whole branch is gone: {m:?}");
    }

    #[test]
    fn remove_of_an_absent_or_shadowed_path_is_none() {
        let mut m = Model::new();
        set(&mut m, "networking.hostName", Value::Str("box".into()));
        assert!(m.remove("time.timeZone").is_none());
        assert!(m.remove("networking.hostName.extra").is_none());
        assert_eq!(m.get("networking.hostName"), Some(Value::Str("box".into())));
    }

    #[test]
    fn from_eval_json_is_the_identity_on_the_projection() {
        let mut m = Model::new();
        set(&mut m, "networking.hostName", Value::Str("box".into()));
        set(&mut m, "networking.firewall.enable", Value::Bool(true));
        set(&mut m, "networking.firewall.allowedTCPPorts", Value::List(vec![Value::Int(22)]));
        set(&mut m, "time.timeZone", Value::Str("UTC".into()));

        // Nested JSON is what evaluating the generated module would yield.
        let evaluated = serde_json::json!({
            "networking": {
                "hostName": "box",
                "firewall": { "enable": true, "allowedTCPPorts": [22] }
            },
            "time": { "timeZone": "UTC" }
        });
        assert_eq!(Model::from_eval_json(&evaluated), m);
        assert_eq!(
            m.leaves().keys().cloned().collect::<Vec<_>>(),
            [
                "networking.firewall.allowedTCPPorts",
                "networking.firewall.enable",
                "networking.hostName",
                "time.timeZone",
            ]
        );
    }

    #[test]
    fn from_eval_json_empty_body_is_empty_model() {
        assert!(Model::from_eval_json(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn a_diff_names_what_changed_and_what_went() {
        let mut base = Model::new();
        set(&mut base, "kept", Value::Int(1));
        set(&mut base, "moved", Value::Int(2));
        set(&mut base, "dropped", Value::Int(3));

        let mut theirs = base.clone();
        theirs.set("moved", Value::Int(9)).unwrap();
        theirs.remove("dropped");
        set(&mut theirs, "added", Value::Int(4));

        assert_eq!(
            theirs.diff(&base),
            [
                ("added".to_string(), Some(Value::Int(4))),
                ("dropped".to_string(), None),
                ("moved".to_string(), Some(Value::Int(9))),
            ]
        );
        assert!(base.diff(&base).is_empty());
    }

    /// A removal and a null differ, so a diff must not collapse them.
    #[test]
    fn a_null_is_a_value_a_diff_carries() {
        let base = Model::new();
        let mut theirs = Model::new();
        set(&mut theirs, "n", Value::Null);
        assert_eq!(theirs.diff(&base), [("n".to_string(), Some(Value::Null))]);

        let mut applied = base.clone();
        applied.apply(&theirs.diff(&base)).unwrap();
        assert_eq!(applied.get("n"), Some(Value::Null));
        assert_eq!(applied, theirs);
    }

    /// The empty attrset is a leaf, so it survives the round trip that carrying a
    /// draft is.
    #[test]
    fn applying_a_diff_reproduces_the_model_it_came_from() {
        let mut base = Model::new();
        set(&mut base, "kept", Value::Int(1));
        set(&mut base, "dropped", Value::Int(2));

        let mut theirs = base.clone();
        theirs.remove("dropped");
        set(&mut theirs, "added.deep", Value::Int(3));
        set(&mut theirs, "empty", attrs(&[]));

        let mut applied = base.clone();
        applied.apply(&theirs.diff(&base)).unwrap();
        assert_eq!(applied, theirs);
    }

    /// The tip changed shape underneath the draft, so the change no longer fits.
    /// This is what quarantines a draft.
    #[test]
    fn applying_over_a_changed_shape_is_a_conflict() {
        let mut base = Model::new();
        set(&mut base, "a", Value::Int(1));
        let mut theirs = base.clone();
        theirs.set("a", Value::Int(2)).unwrap();
        let changes = theirs.diff(&base);

        let mut tip = Model::new();
        set(&mut tip, "a.b", Value::Int(1));
        assert!(matches!(tip.apply(&changes), Err(Error::Conflict(_))));
    }

    #[test]
    fn value_round_trips_through_json() {
        let v = attrs(&[
            ("a", Value::Bool(true)),
            ("b", Value::Int(7)),
            ("c", Value::Str("s".into())),
            ("d", Value::List(vec![Value::Float(1.5), Value::Null])),
        ]);
        let json = serde_json::to_string(&v).unwrap();
        let back: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }
}
