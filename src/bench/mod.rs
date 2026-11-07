//! Synthetic benchmark data, independent of scope resolution.
pub mod fixture;

pub use fixture::{load_fixture, Fixture, FixtureError};

pub mod baselines;
pub mod elo_selector;
pub mod metrics;
pub mod run;
pub mod selector;
mod unicode;

pub mod replay;
