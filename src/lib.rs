//! cortex9-epoch-gc: epoch-based memory reclamation.
//!
//! - [`epoch`]: collector, per-thread handles, guards and deferred frees.

#![warn(missing_docs)]

pub mod epoch;
mod sync;

pub use epoch::{Bugs, Collector, Config, Guard, Handle};
