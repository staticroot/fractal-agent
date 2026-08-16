use serde::{Deserialize, Serialize};

use crate::config::Value;

/// `Any` is the identity element, which is why standalone mode needs no separate
/// "unconstrained" flag: it is the same set algebra managed mode narrows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Allowed {
    Any,
    OneOf { values: Vec<Value> },
    Fixed { value: Value },
}

impl Allowed {
    pub fn allows(&self, value: &Value) -> bool {
        match self {
            Allowed::Any => true,
            Allowed::Fixed { value: v } => v == value,
            Allowed::OneOf { values } => values.contains(value),
        }
    }

    pub fn intersect(&self, other: &Allowed) -> Option<Allowed> {
        use Allowed::*;
        match (self, other) {
            (Any, x) | (x, Any) => Some(x.clone()),
            (Fixed { value: a }, Fixed { value: b }) => (a == b).then(|| Fixed { value: a.clone() }),
            (Fixed { value: v }, set) | (set, Fixed { value: v }) => {
                set.allows(v).then(|| Fixed { value: v.clone() })
            }
            (OneOf { values: a }, OneOf { values: b }) => {
                let values: Vec<Value> = a.iter().filter(|v| b.contains(v)).cloned().collect();
                match values.len() {
                    0 => None,
                    1 => Some(Fixed { value: values.into_iter().next().unwrap() }),
                    _ => Some(OneOf { values }),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Config,
    Build,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constraint {
    pub allowed: Allowed,
    pub reason: Option<String>,
    pub enforcement: Vec<Layer>,
}

impl Constraint {
    pub fn unconstrained() -> Self {
        Self {
            allowed: Allowed::Any,
            reason: None,
            enforcement: Vec::new(),
        }
    }

    pub fn allows(&self, value: &Value) -> bool {
        self.allowed.allows(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Shared,
    Local,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionMeta {
    pub type_name: Option<String>,
    pub default: Option<Value>,
    pub description: Option<String>,
    pub example: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub key: String,
    pub constraint: Constraint,
    pub scope: Scope,
    pub meta: Option<OptionMeta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    LocalEvaluation,
    ExternalEvaluation,
    RuntimeCheck,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stamped<T> {
    pub value: T,
    pub source: Source,
    pub as_of: jiff::Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionRead {
    pub key: String,
    /// Unstamped where the others are: it is the reader's own draft, read now.
    pub draft: Option<Value>,
    pub effective: Option<Stamped<Value>>,
    pub declared: Option<Stamped<Value>>,
    /// Always `None` in v0. The runtime checker that fills it is named but unbuilt.
    pub runtime: Option<Stamped<Value>>,
}
