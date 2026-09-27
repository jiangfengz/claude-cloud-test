//! Parallel seed exploration.
//!
//! Each seed is an independent, deterministic universe, so fuzzing is
//! embarrassingly parallel: worker threads pull seeds from a shared counter
//! and report back. Results are sorted by seed, so the summary does not
//! depend on thread scheduling either.

use crate::checker::ViolationKind;
use crate::sim::{self, SimConfig};
use crate::time::Time;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Failure {
    pub seed: u64,
    pub primary: ViolationKind,
    pub kinds: Vec<ViolationKind>,
}

#[derive(Clone, Debug, Default)]
pub struct FuzzSummary {
    pub runs: u64,
    pub failures: Vec<Failure>,
    pub virtual_time: Time,
    pub events: u64,
    pub ops: u64,
    pub lin_unknown: u64,
    pub elapsed: Duration,
}

impl FuzzSummary {
    /// How often each kind of violation was the first one seen in a run.
    pub fn by_kind(&self) -> BTreeMap<ViolationKind, usize> {
        let mut m = BTreeMap::new();
        for f in &self.failures {
            *m.entry(f.primary).or_insert(0) += 1;
        }
        m
    }
}

/// Runs every seed in `seeds` (stopping early once `stop_after` failures have
/// been seen, if set). `progress` is called with (runs done, failures).
pub fn fuzz(
    base: &SimConfig,
    seeds: Range<u64>,
    jobs: usize,
    stop_after: Option<usize>,
    progress: &(dyn Fn(u64, usize) + Sync),
) -> FuzzSummary {
    let start = Instant::now();
    let next = AtomicU64::new(seeds.start);
    let done = AtomicU64::new(0);
    let failed = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let summary = Mutex::new(FuzzSummary::default());

    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| {
                let mut cfg = base.clone();
                cfg.trace = false;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= seeds.end {
                        break;
                    }
                    cfg.seed = seed;
                    let report = sim::run(&cfg);
                    {
                        let mut s = summary.lock().expect("poisoned");
                        s.runs += 1;
                        s.virtual_time += report.stats.virtual_time;
                        s.events += report.stats.events;
                        s.ops += report.history.ops.len() as u64;
                        s.lin_unknown += report.stats.lin_unknown as u64;
                        if let Some(primary) = report.primary() {
                            s.failures.push(Failure {
                                seed,
                                primary,
                                kinds: report.kinds().into_iter().collect(),
                            });
                        }
                    }
                    let f = if report.failed() {
                        failed.fetch_add(1, Ordering::Relaxed) + 1
                    } else {
                        failed.load(Ordering::Relaxed)
                    };
                    if stop_after.is_some_and(|limit| f >= limit) {
                        stop.store(true, Ordering::Relaxed);
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    progress(d, f);
                }
            });
        }
    });

    let mut summary = summary.into_inner().expect("poisoned");
    summary.failures.sort_by_key(|f| f.seed);
    summary.elapsed = start.elapsed();
    summary
}

/// Runs `cfg` twice and compares fingerprints.
pub fn check_determinism(cfg: &SimConfig) -> Result<u64, (u64, u64)> {
    let a = sim::run(cfg).fingerprint;
    let b = sim::run(cfg).fingerprint;
    if a == b { Ok(a) } else { Err((a, b)) }
}
