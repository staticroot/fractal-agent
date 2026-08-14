//! Wire types only, kept out of `fractal-core` so a client can speak the protocol
//! without linking a git implementation and SQLite.

pub mod catalog;
pub mod config;
pub mod diff;
pub mod evidence;
pub mod generations;
pub mod messages;

pub use catalog::{
    Allowed, CatalogEntry, Constraint, Layer, OptionMeta, OptionRead, Scope, Source, Stamped,
};
pub use config::Value;
pub use diff::{ClosureDiff, OptionChange, PackageDelta, SemanticDiff};
pub use evidence::Evidence;
pub use generations::{Generation, Kind, LogRef, Outcome};
pub use messages::{
    Adoption, Challenge, Endpoint, Method, Payload, Request, Response, Solution, StagedChange,
};
