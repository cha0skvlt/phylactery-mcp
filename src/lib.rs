//! phylactery-mcp: one vector circuit over four memory tables.
//!
//! The library exists so that the integration tests exercise the same code the binary runs —
//! not a re-implementation of it.

pub mod ask;
pub mod backup;
pub mod browse;
pub mod circuit;
pub mod config;
pub mod db;
pub mod dedup;
pub mod embed;
pub mod guard;
pub mod kg;
pub mod math;
pub mod mutate;
pub mod ops;
pub mod surgical;
pub mod write;
