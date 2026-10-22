//! Throughput of the lock-free map versus `Mutex<HashMap>` and
//! `RwLock<HashMap>` from std, on a read-mostly mix (90% get, 5% insert,
//! 5% remove) over 65,536 keys.
//!
//! `cargo run --release --example bench`

use cortex9_epoch_gc::{Collector, HashMap};
use std::collections::HashMap as StdMap;
use std::sync::{Arc, Barrier, Mutex, RwLock};
use std::thread;
use std::time::Instant;

const KEYS: u64 = 1 << 16;
const OPS: usize = 2_000_000;

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

fn run<F: Fn(usize, &mut dyn FnMut() -> u64) + Sync + Send + 'static>(
    threads: usize,
    op: F,
) -> f64 {
    let op = Arc::new(op);
    let bar = Arc::new(Barrier::new(threads + 1));
    let hs: Vec<_> = (0..threads)
        .map(|t| {
            let (op, bar) = (op.clone(), bar.clone());
            thread::spawn(move || {
                let mut r = rng(0x9E37_79B9 + t as u64);
                bar.wait();
                op(OPS / threads, &mut r);
            })
        })
        .collect();
    bar.wait();
    let t0 = Instant::now();
    for h in hs {
        h.join().unwrap();
    }
    OPS as f64 / t0.elapsed().as_secs_f64() / 1e6
}

fn main() {
    let max = thread::available_parallelism().map_or(4, |n| n.get());
    println!("threads  epoch-map  Mutex<HashMap>  RwLock<HashMap>   (Mops/s)");
    let mut t = 1;
    while t <= max {
        let c = Collector::new();
        let m = Arc::new(HashMap::new(1 << 14, &c));
        {
            let h = c.register();
            let g = h.pin();
            for k in (0..KEYS).step_by(2) {
                m.insert(k, k, &g);
            }
        }
        let mm = m.clone();
        let lf = run(t, move |n, r| {
            let h = mm.collector().register();
            for _ in 0..n {
                let x = r();
                let k = x % KEYS;
                let g = h.pin();
                match (x >> 32) % 20 {
                    0 => {
                        mm.insert(k, k, &g);
                    }
                    1 => {
                        mm.remove(k, &g);
                    }
                    _ => {
                        std::hint::black_box(mm.get(k, &g));
                    }
                }
            }
        });

        let base: StdMap<u64, u64> = (0..KEYS).step_by(2).map(|k| (k, k)).collect();
        let mx = Arc::new(Mutex::new(base.clone()));
        let mtx = run(t, move |n, r| {
            for _ in 0..n {
                let x = r();
                let k = x % KEYS;
                let mut g = mx.lock().unwrap();
                match (x >> 32) % 20 {
                    0 => {
                        g.entry(k).or_insert(k);
                    }
                    1 => {
                        g.remove(&k);
                    }
                    _ => {
                        std::hint::black_box(g.get(&k));
                    }
                }
            }
        });

        let rw = Arc::new(RwLock::new(base));
        let rwl = run(t, move |n, r| {
            for _ in 0..n {
                let x = r();
                let k = x % KEYS;
                match (x >> 32) % 20 {
                    0 => {
                        rw.write().unwrap().entry(k).or_insert(k);
                    }
                    1 => {
                        rw.write().unwrap().remove(&k);
                    }
                    _ => {
                        std::hint::black_box(rw.read().unwrap().get(&k).copied());
                    }
                }
            }
        });
        println!("{t:>7}  {lf:>9.2}  {mtx:>14.2}  {rwl:>15.2}");
        t *= 2;
    }
}
