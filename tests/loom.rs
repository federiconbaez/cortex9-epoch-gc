//! Model checking with loom: `RUSTFLAGS="--cfg loom" cargo test --release --test loom`.
//! Every interleaving (within loom's bounds) of a reader and a writer is
//! explored; the reader must never observe a value whose destructor has
//! already run while it is still pinned.
#![cfg(loom)]

use cortex9_epoch_gc::{Bugs, Collector, Config, HashMap};
use loom::sync::atomic::{AtomicBool, AtomicUsize};
use loom::sync::Arc;
use loom::thread;
use std::sync::atomic::Ordering::SeqCst;

struct Tracked {
    freed: Arc<AtomicBool>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.freed.store(true, SeqCst);
    }
}

fn reader_vs_remover(bugs: Bugs) {
    loom::model(move || {
        let c = Collector::with_config(Config {
            collect_every: 1,
            bugs,
        });
        let m = Arc::new(HashMap::new(1, &c));
        let freed = Arc::new(AtomicBool::new(false));
        {
            let h = c.register();
            let g = h.pin();
            m.insert(
                1,
                Tracked {
                    freed: freed.clone(),
                },
                &g,
            );
        }
        let reader = {
            let (m, freed) = (m.clone(), freed.clone());
            thread::spawn(move || {
                let h = m.collector().register();
                let g = h.pin();
                if m.get(1, &g).is_some() {
                    // Still pinned: the value must not have been destroyed.
                    assert!(!freed.load(SeqCst), "value freed under a pinned reader");
                }
                drop(g);
            })
        };
        let h = c.register();
        {
            let g = h.pin();
            m.remove(1, &g);
        }
        for _ in 0..3 {
            h.collect();
        }
        reader.join().unwrap();
    });
}

#[test]
fn reader_never_sees_freed_value() {
    reader_vs_remover(Bugs::default());
}

#[test]
#[should_panic(expected = "value freed under a pinned reader")]
fn negative_control_ignore_pinned() {
    reader_vs_remover(Bugs {
        ignore_pinned: true,
        ..Bugs::default()
    });
}

#[test]
#[should_panic(expected = "value freed under a pinned reader")]
fn negative_control_free_early() {
    reader_vs_remover(Bugs {
        free_early: true,
        ..Bugs::default()
    });
}

/// Two threads race to insert and remove the same key; the per-key
/// balance of successful operations must match the final state.
#[test]
fn insert_remove_race_balances() {
    loom::model(|| {
        let c = Collector::with_config(Config {
            collect_every: 1,
            bugs: Bugs::default(),
        });
        let m = Arc::new(HashMap::new(1, &c));
        let bal = Arc::new(AtomicUsize::new(0));
        let t = {
            let (m, bal) = (m.clone(), bal.clone());
            thread::spawn(move || {
                let h = m.collector().register();
                let g = h.pin();
                if m.insert(7, 70u64, &g) {
                    bal.fetch_add(1, SeqCst);
                }
                if m.remove(7, &g) {
                    bal.fetch_sub(1, SeqCst);
                }
            })
        };
        let h = c.register();
        {
            let g = h.pin();
            if m.insert(7, 71u64, &g) {
                bal.fetch_add(1, SeqCst);
            }
        }
        t.join().unwrap();
        let g = h.pin();
        assert_eq!(bal.load(SeqCst) == 1, m.get(7, &g).is_some());
    });
}
