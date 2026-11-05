//! ELO: a local context and memory engine for coding agents.
//!
//! This crate is the library half of the `elo` binary: it holds the
//! configuration, the local store and the self checks.

pub mod bench;
pub mod config;
pub mod context;
pub mod cue;
pub mod discover;
pub mod doctor;
pub mod events;
pub mod explain;
pub mod guard;
pub mod hook;
pub mod import;
pub mod init;
pub mod live;
pub mod maintain;
pub mod map;
pub mod mcp;
pub mod recall;
pub mod recipe;
pub mod stats;
pub mod store;
pub mod work;
pub mod write;

pub mod usage;

pub mod adapt;
pub mod export;
pub mod setup;
