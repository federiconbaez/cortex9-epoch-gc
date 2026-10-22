//! Lock-free hash map from `u64` keys: a fixed array of buckets, each a
//! Harris–Michael sorted linked list. Deletion first marks a node's
//! `next` pointer (logical delete) and then unlinks it with a CAS on the
//! predecessor; traversals that meet a marked node help unlink it. Every
//! unlinked node is handed to the epoch collector, which frees it once no
//! pinned reader can still reach it.
//!
//! The bucket count is fixed at construction (no resizing in v0.1).

use crate::epoch::{Collector, Guard};
use crate::sync::{AtomicPtr, AtomicUsize};
use std::marker::PhantomData;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};

struct Node<V> {
    key: u64,
    value: V,
    /// Next node; a set low bit means *this* node is deleted.
    next: AtomicPtr<Node<V>>,
}

// Tagging keeps pointer provenance (checked by Miri with
// `-Zmiri-strict-provenance`): the mark is added and removed with
// byte offsets instead of integer casts.
#[inline]
fn marked<V>(p: *mut Node<V>) -> bool {
    (p as usize) & 1 == 1
}
#[inline]
fn with_mark<V>(p: *mut Node<V>) -> *mut Node<V> {
    debug_assert!(!marked(p));
    p.cast::<u8>().wrapping_add(1).cast()
}
#[inline]
fn unmarked<V>(p: *mut Node<V>) -> *mut Node<V> {
    if marked(p) {
        p.cast::<u8>().wrapping_sub(1).cast()
    } else {
        p
    }
}

struct SendPtr<V>(*mut Node<V>);
// SAFETY: the pointer is only dereferenced by the deferred destructor,
// which owns the node exclusively by then; `V: Send` is required.
unsafe impl<V: Send> Send for SendPtr<V> {}

/// Concurrent map. `insert`, `remove` and `get` are lock-free.
pub struct HashMap<V> {
    buckets: Box<[AtomicPtr<Node<V>>]>,
    mask: u64,
    len: AtomicUsize,
    collector: Collector,
    _v: PhantomData<Box<Node<V>>>,
}

// SAFETY: nodes are shared between threads through atomics; values are
// read through `&V` from many threads (`V: Sync`) and dropped on whichever
// thread runs the deferred destructor (`V: Send`).
unsafe impl<V: Send + Sync> Send for HashMap<V> {}
unsafe impl<V: Send + Sync> Sync for HashMap<V> {}

fn spread(k: u64) -> u64 {
    // SplitMix64 finalizer, so sequential keys spread over buckets.
    let mut z = k.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl<V: Send + Sync + 'static> HashMap<V> {
    /// Map with `buckets` buckets (rounded up to a power of two) whose
    /// nodes are reclaimed through `collector`.
    pub fn new(buckets: usize, collector: &Collector) -> Self {
        let n = buckets.max(1).next_power_of_two();
        HashMap {
            buckets: (0..n)
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
            mask: n as u64 - 1,
            len: AtomicUsize::new(0),
            collector: collector.clone(),
            _v: PhantomData,
        }
    }

    /// The collector guards must come from.
    pub fn collector(&self) -> &Collector {
        &self.collector
    }

    fn bucket(&self, k: u64) -> &AtomicPtr<Node<V>> {
        &self.buckets[(spread(k) & self.mask) as usize]
    }

    /// Find the first unmarked node with key >= `k`, unlinking marked
    /// nodes on the way. Returns the link that points to it, the node
    /// (unmarked, possibly null) and whether its key equals `k`.
    #[allow(clippy::type_complexity)]
    fn find<'g>(
        &'g self,
        k: u64,
        guard: &'g Guard,
    ) -> (&'g AtomicPtr<Node<V>>, *mut Node<V>, bool) {
        'retry: loop {
            let mut prev = self.bucket(k);
            let mut curr = prev.load(Acquire);
            loop {
                if marked(curr) {
                    // `prev` itself got deleted under us.
                    continue 'retry;
                }
                if curr.is_null() {
                    return (prev, curr, false);
                }
                // SAFETY: `curr` was reachable while we are pinned, so the
                // collector has not freed it.
                let node = unsafe { &*curr };
                let next = node.next.load(Acquire);
                if marked(next) {
                    let succ = unmarked(next);
                    if prev.compare_exchange(curr, succ, AcqRel, Acquire).is_err() {
                        continue 'retry;
                    }
                    let p = SendPtr(curr);
                    guard.defer(move || {
                        let p = p;
                        // SAFETY: unlinked by the CAS above (exactly one
                        // thread wins it) and no longer reachable.
                        drop(unsafe { Box::from_raw(p.0) });
                    });
                    curr = succ;
                    continue;
                }
                if node.key >= k {
                    return (prev, curr, node.key == k);
                }
                prev = &node.next;
                curr = next;
            }
        }
    }

    /// Insert if absent. Returns `false` (and drops `v`) if `k` is present.
    pub fn insert(&self, k: u64, v: V, guard: &Guard) -> bool {
        let node = Box::into_raw(Box::new(Node {
            key: k,
            value: v,
            next: AtomicPtr::new(std::ptr::null_mut()),
        }));
        loop {
            let (prev, curr, found) = self.find(k, guard);
            if found {
                // SAFETY: never published.
                drop(unsafe { Box::from_raw(node) });
                return false;
            }
            // SAFETY: not yet published; we own it.
            unsafe { (*node).next.store(curr, Relaxed) };
            if prev.compare_exchange(curr, node, Release, Relaxed).is_ok() {
                self.len.fetch_add(1, Relaxed);
                return true;
            }
        }
    }

    /// Remove. Returns whether `k` was present.
    pub fn remove(&self, k: u64, guard: &Guard) -> bool {
        loop {
            let (prev, curr, found) = self.find(k, guard);
            if !found {
                return false;
            }
            // SAFETY: reachable while pinned.
            let node = unsafe { &*curr };
            let next = node.next.load(Acquire);
            if marked(next) {
                continue;
            }
            if node
                .next
                .compare_exchange(next, with_mark(next), AcqRel, Acquire)
                .is_err()
            {
                continue;
            }
            // Logically deleted by us. Try to unlink; if that fails, a
            // traversal does it (and retires the node).
            self.len.fetch_sub(1, Relaxed);
            if prev.compare_exchange(curr, next, AcqRel, Acquire).is_ok() {
                let p = SendPtr(curr);
                guard.defer(move || {
                    let p = p;
                    // SAFETY: unlinked by our CAS, unreachable.
                    drop(unsafe { Box::from_raw(p.0) });
                });
            } else {
                self.find(k, guard);
            }
            return true;
        }
    }

    /// Look up. The reference lives as long as the guard.
    pub fn get<'g>(&'g self, k: u64, guard: &'g Guard) -> Option<&'g V> {
        let _ = guard;
        let mut curr = unmarked(self.bucket(k).load(Acquire));
        loop {
            if curr.is_null() {
                return None;
            }
            // SAFETY: reachable while pinned.
            let node = unsafe { &*curr };
            let next = node.next.load(Acquire);
            if node.key >= k {
                return (node.key == k && !marked(next)).then_some(&node.value);
            }
            curr = unmarked(next);
        }
    }

    /// Approximate number of entries (exact when quiescent).
    pub fn len(&self) -> usize {
        self.len.load(Relaxed)
    }

    /// Whether [`HashMap::len`] is zero.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<V> Drop for HashMap<V> {
    fn drop(&mut self) {
        // `&mut self`: no other thread can reach the nodes. Nodes already
        // retired are owned by the collector, not by the lists.
        for b in self.buckets.iter() {
            let mut curr = unmarked(b.load(Acquire));
            while !curr.is_null() {
                // SAFETY: exclusive access; every node still linked is
                // owned by the list.
                let node = unsafe { Box::from_raw(curr) };
                curr = unmarked(node.next.load(Relaxed));
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn single_thread_matches_btreemap() {
        let c = Collector::new();
        let h = c.register();
        let m = HashMap::new(8, &c);
        let mut model = BTreeMap::new();
        let mut x: u64 = 1;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = x % 300;
            let g = h.pin();
            match x % 3 {
                0 => assert_eq!(m.insert(k, k * 10, &g), model.insert(k, k * 10).is_none()),
                1 => assert_eq!(m.remove(k, &g), model.remove(&k).is_some()),
                _ => assert_eq!(m.get(k, &g).copied(), model.get(&k).copied()),
            }
        }
        assert_eq!(m.len(), model.len());
    }

    #[test]
    fn values_are_dropped_exactly_once() {
        use std::sync::atomic::AtomicUsize as Counter;
        use std::sync::Arc as StdArc;
        struct D(StdArc<Counter>);
        impl Drop for D {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let drops = StdArc::new(Counter::new(0));
        {
            let c = Collector::new();
            let h = c.register();
            let m = HashMap::new(4, &c);
            for k in 0..1000 {
                let g = h.pin();
                assert!(m.insert(k, D(drops.clone()), &g));
                assert!(!m.insert(k, D(drops.clone()), &g), "duplicate is dropped");
            }
            for k in (0..1000).step_by(2) {
                let g = h.pin();
                assert!(m.remove(k, &g));
            }
        }
        // 1000 rejected duplicates + 1000 inserted values.
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 2000);
    }
}
