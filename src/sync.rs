//! Synchronization primitives, swapped for loom's under `--cfg loom` so
//! the model checker can explore every interleaving.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{fence, AtomicPtr, AtomicUsize};
#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Mutex};

#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{fence, AtomicPtr, AtomicUsize};
#[cfg(not(loom))]
pub(crate) use std::sync::{Arc, Mutex};
