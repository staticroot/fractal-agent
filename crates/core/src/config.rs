//! The configuration the agent owns, held as structured data and projected to a
//! Nix file through one pure serializer. The file is never edited as text: an
//! edit reads the model, sets one key, and re-serializes the whole thing, so the
//! same model always produces the same bytes and the git history carries only
//! real changes. This totality is why written values are restricted to plain
//! data — no functions, let-bindings, or imports.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A plain-data value. This is the whole vocabulary the agent may write into the
/// configuration; anything expressive lives in human-authored modules the agent
/// never touches. `untagged` so it round-trips with the JSON that `nix eval`
/// emits when reading current values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Attrs(BTreeMap<String, Value>),
}

/// The agent-owned option model: a tree of attribute names down to plain-data
/// leaves, which is the shape evaluation returns. Option keys are dotted paths
/// that address into it, so `services.openssh.settings` reads as the subtree and
/// `services.openssh.settings.PermitRootLogin` reads as the string, and both
/// answers are correct at once.
///
/// A tree rather than a flat map of dotted keys, because `a.b = { c = 1; }` and
/// `a.b.c = 1` are indistinguishable once evaluated. A flat model has to guess
/// on every read where the option key ended and its value began, and nothing in
/// the evaluated result can settle that guess. With a tree on both sides,
/// reading the file back is the identity function.
///
/// `BTreeMap` fixes iteration order at every level, which is half of what makes
/// serialization canonical.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Model {
    root: BTreeMap<String, Value>,
}

/// A node is a subtree when it is a non-empty attrset, and a leaf otherwise. An
/// empty attrset is a leaf in its own right: it is a value somebody staged, and
/// treating it as a subtree with no children is what used to make the key vanish
/// on the next read.
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

/// A Nix double-quoted string with every `$` escaped, so a staged string value
/// can never open an interpolation (`${...}`) or otherwise escape into the
/// expression. This is the config-layer form of "staged values are plain data".
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

    /// An attrset value staged at a path is the same tree as the equivalent
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
