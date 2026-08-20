#![forbid(unsafe_code)]

pub mod api;
pub mod conflict;
pub mod db;
pub mod git;
pub mod mcp;
pub mod model;
pub mod service;

pub use db::Store;
pub use service::Foremerge;
