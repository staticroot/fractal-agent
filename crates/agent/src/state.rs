//! Where the agent keeps its things, and the handles a request handler needs.
//! The generations database is not `Sync`, so it sits behind a mutex and is only
//! ever touched from a blocking section; everything else here is cheaply cloned.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fractal_core::builds::Builds;
use fractal_core::catalog::CatalogProvider;
use fractal_core::generations::Generations;
use fractal_core::repo::GitRepo;
use fractal_core::system_config::ModelCache;

use crate::trigger::TriggerProxy;

/// Filesystem layout under the state root. One place so the daemon and its tests
/// agree on every path.
#[derive(Debug, Clone)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
}

impl Paths {
    /// The default layout, rooted at `/var/lib/fractal-agent` with the socket
    /// under `/run`. `FRACTAL_AGENT_STATE` relocates the root for tests.
    pub fn from_env() -> Self {
        let state_dir = std::env::var_os("FRACTAL_AGENT_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/fractal-agent"));
        let socket = std::env::var_os("FRACTAL_AGENT_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/fractal-agent/agent.sock"));
        Self { state_dir, socket }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.state_dir.join("system-config")
    }

    pub fn generations_db(&self) -> PathBuf {
        self.state_dir.join("generations.db")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }

    /// Garbage-collection roots for built-but-not-yet-activated closures.
    pub fn gcroots_dir(&self) -> PathBuf {
        self.state_dir.join("gcroots")
    }
}

/// The handles every request handler shares. Cloneable: the trigger proxy and
/// the mutex/arc handles are all cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub paths: Arc<Paths>,
    pub trigger: TriggerProxy<'static>,
    pub generations: Arc<Mutex<Generations>>,
    pub builds: Arc<Mutex<Builds>>,
    /// A trait object so an externally resolved catalog can be dropped in
    /// without the handlers learning which kind of device they are on.
    pub catalog: Arc<dyn CatalogProvider>,
    /// The configuration repository. Its lock also serializes read-modify-write:
    /// without it two concurrent draft edits read the same model and the later
    /// write drops the first.
    pub repo: Arc<Mutex<GitRepo>>,
    pub models: Arc<Mutex<ModelCache>>,
}
