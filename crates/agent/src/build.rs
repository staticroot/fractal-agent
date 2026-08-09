//! Turning a configuration into a store path, and nothing else. When a build is
//! allowed, what is done with the result, and the socket protocol are the
//! handler's: they are about the request rather than about building.
//!
//! Which attribute holds a *system* closure rather than a home one is authority
//! wiring, so it lives here rather than in the shared library.

use std::path::PathBuf;

use fractal_core::generations::LogRef;
use fractal_core::logs::LogFile;
use fractal_core::nix;
use tokio::sync::mpsc::UnboundedSender;

/// A fixed attribute name, not the hostname. NixOS convention keys
/// `nixosConfigurations` by `networking.hostName`, which is an ordinary option a
/// principal may stage, so the convention would move the attribute the agent
/// builds from the moment somebody renames their machine.
const SYSTEM_ATTR: &str = ".#nixosConfigurations.fractal.config.system.build.toplevel";

pub const CONFIG_ATTR: &str = ".#nixosConfigurations.fractal.config";

/// An attribute path *inside* the flake, not an installable: reading option
/// metadata needs one expression, and `.#` is invalid inside an expression.
pub const OPTIONS_PATH: &str = "nixosConfigurations.fractal.options";

pub struct Output {
    pub store_path: String,
    pub log: Option<LogRef>,
}

/// Output goes to `progress` live and to the log file for the generation record.
/// `gc_root` keeps the closure alive between being built and being used.
pub async fn run(
    dir: PathBuf,
    gc_root: PathBuf,
    log_path: PathBuf,
    progress: UnboundedSender<String>,
) -> Result<Output, String> {
    tokio::task::spawn_blocking(move || {
        let mut log = LogFile::create(&log_path).map_err(|e| e.to_string())?;
        let store_path = nix::build_attr(&dir, SYSTEM_ATTR, Some(&gc_root), |line| {
            let _ = log.write_line(line);
            let _ = progress.send(line.to_string());
        })
        .map_err(|e| e.to_string())?;
        Ok(Output {
            store_path,
            log: log.finish().ok(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}
