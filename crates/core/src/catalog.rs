//! The curated set of options a user may see and change. This is not an
//! abstraction over NixOS options — each entry names a real option key and reads
//! and writes its real value. The entry adds only what the option cannot express
//! itself: the policy-narrowed subset of allowed values, the reason for the
//! narrowing, and which enforcement layers back it. The option's own type is the
//! outer bound and its own validity check is the membership test, so in
//! standalone mode "unconstrained" is the identity case of the same set algebra
//! rather than a separate flag.

use serde::{Deserialize, Serialize};

use crate::config::Value;

/// The allowed set for an option. `Any` is the option's full type domain (the
/// outer bound); the narrower cases are what managed mode adds later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Allowed {
    /// Every value the option's type admits — the identity element.
    Any,
    /// A finite narrowed set of permitted values.
    OneOf { values: Vec<Value> },
    /// Narrowed to exactly one value (a lock).
    Fixed { value: Value },
}

impl Allowed {
    /// Whether `value` is permitted. `Any` defers to the option's own type check,
    /// which happens at evaluation time, so here it admits everything.
    pub fn allows(&self, value: &Value) -> bool {
        match self {
            Allowed::Any => true,
            Allowed::Fixed { value: v } => v == value,
            Allowed::OneOf { values } => values.contains(value),
        }
    }

    /// Composing two policies is intersecting their allowed sets. `None` is an
    /// empty intersection — a detectable conflict.
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

/// Which enforcement layers back a constraint. A key's lock strength is a
/// function of which layers it declares, so strength stays legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Config,
    Build,
    Runtime,
}

/// What a policy adds on top of the option's own type: the narrowed set, the
/// reason, and the backing layers. In standalone the set is `Any` and the reason
/// is empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constraint {
    pub allowed: Allowed,
    pub reason: Option<String>,
    pub enforcement: Vec<Layer>,
}

impl Constraint {
    /// The standalone identity: the option's full type domain, no reason, no
    /// extra enforcement.
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

/// Whether a key follows its owner across devices or belongs to one machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Shared,
    Local,
}

/// The option's own declared metadata, read from the option rather than restated
/// here, so there is no second copy to keep in step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionMeta {
    /// The type's own description, e.g. "boolean" or "list of string".
    pub type_name: Option<String>,
    pub default: Option<Value>,
    pub description: Option<String>,
    pub example: Option<Value>,
}

/// A curated option. `meta` is `None` until a provider fills it, because reading
/// it means evaluating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// The real NixOS option path, e.g. `networking.hostName`.
    pub key: String,
    pub constraint: Constraint,
    pub scope: Scope,
    pub meta: Option<OptionMeta>,
}

/// Where a value came from. Every layer except the staged one may be computed on
/// one machine and read on another, so a stale reading and a live one must not
/// look alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// This device evaluated its own flake.
    LocalEvaluation,
    /// Resolved elsewhere and delivered with the closure pointer.
    ExternalEvaluation,
    /// Measured on the running machine.
    RuntimeCheck,
}

/// A value together with where it came from and as of when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stamped<T> {
    pub value: T,
    pub source: Source,
    pub as_of: jiff::Timestamp,
}

/// A read of one option, in four layers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionRead {
    pub key: String,
    /// Set but not yet applied. Unstamped: it is this device's working copy, now.
    pub staged: Option<Value>,
    /// What a full evaluation resolves once everything is merged.
    pub effective: Option<Stamped<Value>>,
    /// The option's own declared default.
    pub declared: Option<Stamped<Value>>,
    /// Always `None` in v0. This layer belongs to a runtime checker that is named
    /// rather than built, and an empty slot says so honestly.
    pub runtime: Option<Stamped<Value>>,
}

/// Where a catalog comes from. A device that can evaluate resolves its own; one
/// that cannot has it resolved elsewhere. Both answer the same questions, so
/// callers never learn which kind of device they are on.
pub trait CatalogProvider: Send + Sync {
    fn entries(&self) -> crate::error::Result<Vec<CatalogEntry>>;
    /// `staged` comes from the caller, which owns the working copy.
    fn read(&self, key: &str, staged: Option<Value>) -> crate::error::Result<OptionRead>;
}

/// The v0 standalone catalog: a small, real set of options for a daily-driver
/// machine, each unconstrained and carrying no metadata until a provider
/// evaluates it.
pub fn standalone() -> Vec<CatalogEntry> {
    STANDALONE_KEYS
        .iter()
        .map(|key| CatalogEntry {
            key: (*key).to_string(),
            constraint: Constraint::unconstrained(),
            scope: scope_of(key),
            meta: None,
        })
        .collect()
}

/// Most system options describe the machine and stay with it. The ones that
/// travel describe how their owner reads and types.
fn scope_of(key: &str) -> Scope {
    const SHARED: &[&str] = &[
        "time.timeZone",
        "i18n.defaultLocale",
        "console.keyMap",
        "services.xserver.xkb.layout",
    ];
    if SHARED.contains(&key) {
        Scope::Shared
    } else {
        Scope::Local
    }
}

/// Real option paths. Kept flat and legible; values and metadata are read from
/// Nix at request time, never duplicated here.
const STANDALONE_KEYS: &[&str] = &[
    "networking.hostName",
    "time.timeZone",
    "i18n.defaultLocale",
    "console.keyMap",
    "services.xserver.xkb.layout",
    "networking.firewall.enable",
    "networking.firewall.allowedTCPPorts",
    "networking.firewall.allowedUDPPorts",
    "networking.networkmanager.enable",
    "networking.wireless.enable",
    "services.openssh.enable",
    "services.openssh.settings.PermitRootLogin",
    "services.openssh.settings.PasswordAuthentication",
    "services.printing.enable",
    "services.avahi.enable",
    "hardware.bluetooth.enable",
    "services.pipewire.enable",
    "services.xserver.enable",
    "services.displayManager.autoLogin.enable",
    "services.displayManager.autoLogin.user",
    "services.desktopManager.plasma6.enable",
    "services.xserver.desktopManager.gnome.enable",
    "programs.firefox.enable",
    "virtualisation.docker.enable",
    "virtualisation.podman.enable",
    "services.tailscale.enable",
    "services.flatpak.enable",
    "nixpkgs.config.allowUnfree",
    "nix.settings.auto-optimise-store",
    "nix.gc.automatic",
    "system.autoUpgrade.enable",
    "zramSwap.enable",
    "boot.loader.systemd-boot.enable",
    "boot.loader.timeout",
    "services.fstrim.enable",
    "services.thermald.enable",
    "powerManagement.enable",
    "programs.steam.enable",
    "fonts.fontDir.enable",
    "services.tlp.enable",
    "programs.zsh.enable",
    "users.mutableUsers",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn catalog_is_a_real_curated_set() {
        let c = standalone();
        assert!((30..=50).contains(&c.len()), "catalog size {} out of range", c.len());
        assert!(c.iter().any(|e| e.key == "networking.firewall.enable"));
        // Standalone entries are the identity case: everything allowed, no reason.
        assert!(c.iter().all(|e| e.constraint.allowed == Allowed::Any));
        assert!(c.iter().all(|e| e.constraint.reason.is_none()));
    }

    #[test]
    fn any_is_the_identity_element() {
        let set = Allowed::OneOf { values: vec![int(1), int(2)] };
        assert_eq!(Allowed::Any.intersect(&set), Some(set.clone()));
        assert_eq!(set.intersect(&Allowed::Any), Some(set));
        assert!(Allowed::Any.allows(&int(999)));
    }

    #[test]
    fn intersection_narrows_and_detects_conflict() {
        let a = Allowed::OneOf { values: vec![int(1), int(2), int(3)] };
        let b = Allowed::OneOf { values: vec![int(2), int(3), int(4)] };
        assert_eq!(a.intersect(&b), Some(Allowed::OneOf { values: vec![int(2), int(3)] }));

        // Down to one becomes Fixed.
        let single = Allowed::OneOf { values: vec![int(3), int(9)] };
        assert_eq!(a.intersect(&single), Some(Allowed::Fixed { value: int(3) }));

        // Disjoint sets: empty intersection is a conflict.
        let d = Allowed::OneOf { values: vec![int(7), int(8)] };
        assert_eq!(a.intersect(&d), None);
    }

    #[test]
    fn fixed_composition() {
        let fixed = Allowed::Fixed { value: int(2) };
        let set = Allowed::OneOf { values: vec![int(1), int(2)] };
        assert_eq!(fixed.intersect(&set), Some(Allowed::Fixed { value: int(2) }));

        let out = Allowed::OneOf { values: vec![int(5), int(6)] };
        assert_eq!(fixed.intersect(&out), None);
        assert!(fixed.allows(&int(2)));
        assert!(!fixed.allows(&int(3)));
    }
}
