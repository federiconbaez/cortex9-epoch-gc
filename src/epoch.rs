//! Epoch-based reclamation (EBR).
//!
//! A [`Collector`] keeps a global epoch that advances in steps of 2 (the
//! low bit of a participant's word means "pinned"). Each thread
//! registers a [`Handle`]; [`Handle::pin`] publishes "I am reading at
//! epoch e" and returns a [`Guard`]. Memory unlinked from a shared
//! structure is retired with [`Guard::defer`], tagged with the epoch at
//! retirement. The global epoch only advances from `e` to `e + 2` when
//! every pinned participant has observed `e`, so once it has advanced
//! twice past a tag, no thread can still hold a reference obtained before
//! the retirement, and the deferred function runs.
//!
//! Pinning and unpinning are lock-free (one atomic swap and a fence).
//! Advancing the epoch scans the participant list under `try_lock` and
//! simply gives up if the lock is busy, so no operation ever blocks on it.

use crate::sync::{fence, Arc, AtomicUsize, Mutex};
use std::cell::{Cell, RefCell};
use std::sync::atomic::Ordering::{Relaxed, Release, SeqCst};

/// Deliberate bugs for negative-control tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bugs {
    /// Advance the epoch without checking pinned participants.
    pub ignore_pinned: bool,
    /// Free garbage one epoch step too early.
    pub free_early: bool,
}

/// Tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Try to collect after this many local retirements.
    pub collect_every: usize,
    /// Injected bugs (off by default).
    pub bugs: Bugs,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            collect_every: 64,
            bugs: Bugs::default(),
        }
    }
}

type Deferred = Box<dyn FnOnce() + Send>;

struct Participant {
    /// 0 when not pinned, otherwise `epoch | 1`.
    epoch: AtomicUsize,
}

struct Global {
    epoch: AtomicUsize,
    participants: Mutex<Vec<Arc<Participant>>>,
    /// Garbage left behind by handles that were dropped.
    orphans: Mutex<Vec<(usize, Deferred)>>,
    config: Config,
    freed: AtomicUsize,
}

impl Drop for Global {
    fn drop(&mut self) {
        // No handle is left (each holds an `Arc<Global>`): nothing can be
        // pinned, so everything can go.
        let orphans = std::mem::take(&mut *self.orphans.lock().unwrap());
        for (_, f) in orphans {
            f();
        }
    }
}

/// Owner of the global epoch. Cheap to clone (shared).
#[derive(Clone)]
pub struct Collector {
    g: Arc<Global>,
}

impl Default for Collector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector {
    /// New collector with the default configuration.
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    /// New collector.
    pub fn with_config(config: Config) -> Self {
        Collector {
            g: Arc::new(Global {
                epoch: AtomicUsize::new(0),
                participants: Mutex::new(Vec::new()),
                orphans: Mutex::new(Vec::new()),
                config,
                freed: AtomicUsize::new(0),
            }),
        }
    }

    /// Register a participant. A handle is `Send` but not `Sync`: one
    /// thread uses it at a time, and its guards cannot leave that thread.
    pub fn register(&self) -> Handle {
        let me = Arc::new(Participant {
            epoch: AtomicUsize::new(0),
        });
        self.g.participants.lock().unwrap().push(me.clone());
        Handle {
            g: self.g.clone(),
            me,
            garbage: RefCell::new(Vec::new()),
            depth: Cell::new(0),
            since_collect: Cell::new(0),
        }
    }

    /// Current global epoch (always even).
    pub fn epoch(&self) -> usize {
        self.g.epoch.load(SeqCst)
    }

    /// Deferred functions that have run so far.
    pub fn freed(&self) -> usize {
        self.g.freed.load(SeqCst)
    }
}

/// A thread's registration with a [`Collector`].
pub struct Handle {
    g: Arc<Global>,
    me: Arc<Participant>,
    garbage: RefCell<Vec<(usize, Deferred)>>,
    depth: Cell<usize>,
    since_collect: Cell<usize>,
}

/// Proof that the owning thread is pinned. While it lives, memory that
/// was reachable when it was created is not freed.
pub struct Guard<'h> {
    h: &'h Handle,
}

impl Handle {
    /// Pin the thread. Re-entrant.
    pub fn pin(&self) -> Guard<'_> {
        let d = self.depth.get();
        if d == 0 {
            let e = self.g.epoch.load(Relaxed);
            self.me.epoch.swap(e | 1, SeqCst);
            // Order the publication above before every load the caller
            // makes from shared structures (store→load ordering).
            fence(SeqCst);
        }
        self.depth.set(d + 1);
        Guard { h: self }
    }

    /// Whether this thread is pinned.
    pub fn is_pinned(&self) -> bool {
        self.depth.get() > 0
    }

    fn unpin(&self) {
        let d = self.depth.get() - 1;
        self.depth.set(d);
        if d == 0 {
            self.me.epoch.store(0, Release);
        }
    }

    /// Try to move the global epoch forward by one step. Returns the
    /// epoch afterwards.
    fn try_advance(&self) -> usize {
        // Pairs with the fence in `pin`: our earlier unlinks (plain
        // AcqRel CAS) must be ordered before the loads of the participant
        // words below, or a reader could pin unseen and still find a node
        // we already retired. Found by the loom test
        // `reader_never_sees_freed_value`.
        fence(SeqCst);
        let e = self.g.epoch.load(SeqCst);
        if !self.g.config.bugs.ignore_pinned {
            let Ok(list) = self.g.participants.try_lock() else {
                return e;
            };
            for p in list.iter() {
                let pe = p.epoch.load(SeqCst);
                if pe & 1 == 1 && pe != (e | 1) {
                    return e;
                }
            }
        }
        match self.g.epoch.compare_exchange(e, e + 2, SeqCst, SeqCst) {
            Ok(_) => e + 2,
            Err(now) => now,
        }
    }

    fn safe(&self, tag: usize, now: usize) -> bool {
        let gap = if self.g.config.bugs.free_early { 2 } else { 4 };
        tag + gap <= now
    }

    /// Advance if possible and run every deferred function that is safe.
    pub fn collect(&self) {
        let now = self.try_advance();
        let ready: Vec<Deferred> = {
            let mut g = self.garbage.borrow_mut();
            let (ready, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut *g)
                .into_iter()
                .partition(|(t, _)| self.safe(*t, now));
            *g = keep;
            ready.into_iter().map(|(_, f)| f).collect()
        };
        let mut n = ready.len();
        for f in ready {
            f();
        }
        if let Ok(mut o) = self.g.orphans.try_lock() {
            let (ready, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut *o)
                .into_iter()
                .partition(|(t, _)| self.safe(*t, now));
            *o = keep;
            drop(o);
            n += ready.len();
            for (_, f) in ready {
                f();
            }
        }
        self.g.freed.fetch_add(n, SeqCst);
    }

    /// Retired items waiting in this handle.
    pub fn pending(&self) -> usize {
        self.garbage.borrow().len()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        assert_eq!(self.depth.get(), 0, "handle dropped while pinned");
        let garbage = std::mem::take(&mut *self.garbage.borrow_mut());
        self.g.orphans.lock().unwrap().extend(garbage);
        let mut list = self.g.participants.lock().unwrap();
        list.retain(|p| !Arc::ptr_eq(p, &self.me));
    }
}

impl Guard<'_> {
    /// Run `f` once no thread can still be reading what was unlinked
    /// before this call.
    pub fn defer(&self, f: impl FnOnce() + Send + 'static) {
        let h = self.h;
        let tag = h.g.epoch.load(SeqCst);
        h.garbage.borrow_mut().push((tag, Box::new(f)));
        let n = h.since_collect.get() + 1;
        if n >= h.g.config.collect_every {
            h.since_collect.set(0);
            h.collect();
        } else {
            h.since_collect.set(n);
        }
    }

    /// The handle this guard pins.
    pub fn handle(&self) -> &Handle {
        self.h
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.h.unpin();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn epoch_waits_for_pinned_threads() {
        let c = Collector::new();
        let a = c.register();
        let b = c.register();
        let ga = a.pin();
        let e0 = c.epoch();
        // `a` is pinned at e0, so the epoch can advance once (to e0 + 2)
        // but not twice.
        b.collect();
        b.collect();
        b.collect();
        assert_eq!(c.epoch(), e0 + 2);
        drop(ga);
        b.collect();
        assert_eq!(c.epoch(), e0 + 4);
    }

    #[test]
    fn deferred_runs_only_after_two_advances() {
        let c = Collector::with_config(Config {
            collect_every: 1_000,
            ..Config::default()
        });
        let reader = c.register();
        let writer = c.register();
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let r = reader.pin();
        {
            let g = writer.pin();
            let ran = ran.clone();
            g.defer(move || ran.store(true, SeqCst));
        }
        for _ in 0..10 {
            writer.collect();
        }
        assert!(!ran.load(SeqCst), "freed while a reader was pinned");
        drop(r);
        writer.collect();
        writer.collect();
        assert!(ran.load(SeqCst));
    }

    #[test]
    fn orphaned_garbage_is_freed() {
        let c = Collector::new();
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        {
            let h = c.register();
            let g = h.pin();
            for _ in 0..10 {
                let ran = ran.clone();
                g.defer(move || {
                    ran.fetch_add(1, SeqCst);
                });
            }
        }
        let h = c.register();
        for _ in 0..4 {
            h.collect();
        }
        assert_eq!(ran.load(SeqCst), 10);
    }

    #[test]
    fn collector_drop_frees_everything() {
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        {
            let c = Collector::new();
            let h = c.register();
            let g = h.pin();
            let ran = ran.clone();
            g.defer(move || {
                ran.fetch_add(1, SeqCst);
            });
        }
        assert_eq!(ran.load(SeqCst), 1);
    }

    #[test]
    fn pin_is_reentrant() {
        let c = Collector::new();
        let h = c.register();
        let a = h.pin();
        let b = h.pin();
        drop(a);
        assert!(h.is_pinned());
        drop(b);
        assert!(!h.is_pinned());
    }
}
