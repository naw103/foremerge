#![forbid(unsafe_code)]

pub mod api;
pub mod checks;
/// The command-line entry point, shared by the `foremerge` and `fmg` binaries.
///
/// This is an implementation detail of those binaries, not part of the library
/// API. It is public only because both binary targets must reach it, and it
/// carries no stability promise: the CLI is free to change shape without that
/// counting as a breaking change to the library.
#[doc(hidden)]
pub mod cli;
pub mod conflict;
pub mod db;
pub mod exclusions;
pub mod git;
pub mod integrations;
pub mod mcp;
pub mod model;
pub mod service;

pub use db::Store;
pub use service::Foremerge;
