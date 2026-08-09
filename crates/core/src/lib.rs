//! fractal-core: the authority-free mechanism shared by the agent and, later,
//! the per-user service. Everything here takes a repository handle, a reference,
//! or a store path and does a mechanical thing without knowing whose config it
//! is. Authority wiring — signatures, the trigger, the admin prompt — lives in
//! the agent, never here.

pub mod builds;
pub mod catalog;
pub mod catalog_local;
pub mod config;
pub mod diff;
pub mod error;
pub mod evidence;
pub mod generations;
pub mod logs;
pub mod nix;
pub mod protocol;
pub mod repo;
pub mod staged;
pub mod system_config;

pub use builds::{Build, Builds, NewBuild};
pub use catalog::{
    Allowed, CatalogEntry, CatalogProvider, Constraint, Layer, OptionMeta, OptionRead, Scope,
    Source, Stamped,
};
pub use catalog_local::LocalCatalog;
pub use config::{Model, Value};
pub use diff::{ClosureDiff, OptionChange, PackageDelta, SemanticDiff};
pub use error::{Error, Result};
pub use evidence::Evidence;
pub use generations::{Generation, Generations, Kind, LogRef, NewGeneration, Outcome};
pub use protocol::{Challenge, Method, Payload, Request, Response, Solution};
pub use repo::{Author, ConfigVcs, GitRepo};
pub use staged::Staged;
