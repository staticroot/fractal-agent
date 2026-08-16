//! The curated set of options a user may see and change. This is not an
//! abstraction over NixOS options — each entry names a real option key and reads
//! and writes its real value. The entry adds only what the option cannot express
//! itself: the policy-narrowed subset of allowed values, the reason for the
//! narrowing, and which enforcement layers back it. The option's own type is the
//! outer bound and its own validity check is the membership test, so in
//! standalone mode "unconstrained" is the identity case of the same set algebra
//! rather than a separate flag.


use crate::config::Value;

pub use fractal_protocol::catalog::{
    Allowed, CatalogEntry, Constraint, Layer, OptionMeta, OptionRead, Scope, Source, Stamped,
};

/// Where a catalog comes from. A device that can evaluate resolves its own; one
/// that cannot has it resolved elsewhere. Both answer the same questions, so
/// callers never learn which kind of device they are on.
pub trait CatalogProvider: Send + Sync {
    fn entries(&self) -> crate::error::Result<Vec<CatalogEntry>>;
    /// `draft` is what this principal's own draft holds for the key.
    fn read(
        &self,
        key: &str,
        draft: Option<Value>,
        uid: crate::draft::Uid,
    ) -> crate::error::Result<OptionRead>;

    /// Resolve this reader's layer before they ask for it. A provider with
    /// nothing to prepare does nothing.
    fn warm(&self, _uid: crate::draft::Uid) -> crate::error::Result<()> {
        Ok(())
    }
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
