//! Multi-threaded stress tests (also run under Miri and ThreadSanitizer
//! in CI, with smaller sizes under Miri).
#![cfg(not(loom))]

use cortex9_epoch_gc::{Collector, HashMap};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::thread;

fn scale(n: usize) -> usize {
    if cfg!(miri) {
        (n / 200).max(20)
    } else {
        n
    }
}

struct Xs(u64);
impl Xs {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Threads insert and remove overlapping keys. For every key, successful
/// inserts minus successful removes must be 0 or 1 and match presence at
/// the end; a lost update or a double remove breaks it.
#[test]
fn overlapping_inserts_and_removes_balance() {
    let threads = if cfg!(miri) { 3 } else { 8 };
    let keys = 64u64;
    let ops = scale(40_000);
    let c = Collector::new();
    let m = Arc::new(HashMap::new(16, &c));
    let balance: Arc<Vec<AtomicI64>> = Arc::new((0..keys).map(|_| AtomicI64::new(0)).collect());
    let hs: Vec<_> = (0..threads)
        .map(|t| {
            let (m, balance) = (m.clone(), balance.clone());
            thread::spawn(move || {
                let h = m.collector().register();
                let mut r = Xs(t as u64 * 7919 + 1);
                for _ in 0..ops {
                    let x = r.next();
                    let k = x % keys;
                    let g = h.pin();
                    match x % 3 {
                        0 => {
                            if m.insert(k, k, &g) {
                                balance[k as usize].fetch_add(1, SeqCst);
                            }
                        }
                        1 => {
                            if m.remove(k, &g) {
                                balance[k as usize].fetch_sub(1, SeqCst);
                            }
                        }
                        _ => {
                            if let Some(v) = m.get(k, &g) {
                                assert_eq!(*v, k, "read a value of another key or freed memory");
                            }
                        }
                    }
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let h = c.register();
    let g = h.pin();
    let mut present = 0;
    for k in 0..keys {
        let b = balance[k as usize].load(SeqCst);
        let here = m.get(k, &g).is_some();
        assert!(b == 0 || b == 1, "key {k}: balance {b}");
        assert_eq!(b == 1, here, "key {k}: balance {b}, present {here}");
        present += here as usize;
    }
    assert_eq!(m.len(), present);
}

/// Every value is dropped exactly once, and reclamation actually happens
/// while threads are still running (not only at the end).
#[test]
fn every_value_dropped_once_and_memory_is_reclaimed_concurrently() {
    struct D(Arc<AtomicUsize>);
    impl Drop for D {
        fn drop(&mut self) {
            self.0.fetch_add(1, SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let created = Arc::new(AtomicUsize::new(0));
    let threads = if cfg!(miri) { 2 } else { 6 };
    let ops = scale(30_000);
    let freed_midway;
    {
        let c = Collector::new();
        let m = Arc::new(HashMap::new(32, &c));
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let (m, drops, created) = (m.clone(), drops.clone(), created.clone());
                thread::spawn(move || {
                    let h = m.collector().register();
                    let mut r = Xs(t as u64 + 99);
                    for _ in 0..ops {
                        let k = r.next() % 128;
                        let g = h.pin();
                        created.fetch_add(1, SeqCst);
                        m.insert(k, D(drops.clone()), &g);
                        m.remove(r.next() % 128, &g);
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        freed_midway = c.freed();
        drop(m);
    }
    assert_eq!(drops.load(SeqCst), created.load(SeqCst));
    if !cfg!(miri) {
        assert!(freed_midway > 0, "nothing reclaimed before shutdown");
    }
}

/// Readers hold references across many writer operations; the referenced
/// values must stay intact while the guard lives.
#[test]
fn references_stay_valid_while_pinned() {
    let c = Collector::new();
    let m = Arc::new(HashMap::new(4, &c));
    {
        let h = c.register();
        let g = h.pin();
        for k in 0..32u64 {
            m.insert(k, vec![k; 16], &g);
        }
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (m, stop) = (m.clone(), stop.clone());
        thread::spawn(move || {
            let h = m.collector().register();
            let mut r = Xs(5);
            while !stop.load(SeqCst) {
                let k = r.next() % 32;
                let g = h.pin();
                if m.remove(k, &g) {
                    m.insert(k, vec![k; 16], &g);
                }
            }
        })
    };
    let h = c.register();
    for _ in 0..scale(2_000) {
        let g = h.pin();
        let held: Vec<&Vec<u64>> = (0..32).filter_map(|k| m.get(k, &g)).collect();
        thread::yield_now();
        for v in held {
            assert_eq!(v.len(), 16);
            assert!(v.iter().all(|x| *x == v[0]));
        }
    }
    stop.store(true, SeqCst);
    writer.join().unwrap();
}
