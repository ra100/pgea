//! pg-rds-connector library crate.
//!
//! Public modules expose pure-logic units that are unit-tested in isolation.
//! Wire-protocol and AWS SDK glue lives in (future) `pg::*` and `rds::*` modules.

pub mod catalog;
pub mod config;
pub mod intercept;
pub mod pg;
pub mod rds;
pub mod rewriter;
pub mod types;
