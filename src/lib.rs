//! cortex9-epoch-gc: epoch-based memory reclamation and a lock-free hash
//! map built on it.
//!
//! - [`epoch`]: collector, per-thread handles, guards and deferred frees.
//! - [`map`]: Harris–Michael lists in a fixed bucket array.

#![warn(missing_docs)]

pub mod epoch;
pub mod map;
mod sync;

pub use epoch::{Bugs, Collector, Config, Guard, Handle};
pub use map::HashMap;
