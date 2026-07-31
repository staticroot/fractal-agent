//! fractal-core: the authority-free mechanism shared by the agent and, later,
//! the per-user service. Everything here takes a repository handle, a reference,
//! or a store path and does a mechanical thing without knowing whose config it
//! is. Authority wiring — signatures, the trigger, the admin prompt — lives in
//! the agent, never here.

pub mod catalog;
pub mod config;
pub mod diff;
pub mod error;
pub mod generations;
pub mod logs;
pub mod nix;
pub mod protocol;
pub mod repo;
pub mod system_config;

pub use catalog::{Allowed, CatalogEntry, Constraint, Layer};
pub use config::{Model, Value};
pub use diff::{ClosureDiff, OptionChange, PackageDelta, SemanticDiff};
pub use error::{Error, Result};
pub use generations::{Generation, Generations, Kind, LogRef, NewGeneration, Outcome};
pub use protocol::{Challenge, Method, Payload, Request, Response, Solution};
pub use repo::{ConfigVcs, GitRepo};
