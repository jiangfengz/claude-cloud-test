//! Automatic minimisation of failing runs.
//!
//! A random failing schedule typically has dozens of faults and hundreds of
//! operations, almost all of them irrelevant. Because a run is a pure
//! function of its configuration, we can cheaply re-run variations and keep
//! any that still fail the same way:
//!
//! 1. **Faults** — Zeller & Hildebrandt's `ddmin` removes chunks of the fault
//!    plan, converging on a 1-minimal subset (removing any single remaining
//!    fault makes the failure disappear).
//! 2. **Clients** — drop whole clients.
//! 3. **Workload** — binary-search the number of operations per client.
//! 4. **Chaos window** — start recovery right after the last fault.
//! 5. **Crash points** — switch them off if the failure survives without.
//!
//! Every accepted candidate is a verified failing run, so the result is
//! always a genuine reproduction — just a much smaller one.

use crate::checker::ViolationKind;
use crate::nemesis::FaultPlan;
use crate::sim::{self, FaultMode, RunReport, SimConfig};
use crate::time::ms;

/// Delta debugging: a 1-minimal subset of `items` for which `test` holds,
/// assuming it holds for `items` itself.
pub fn ddmin<T: Clone>(items: Vec<T>, mut test: impl FnMut(&[T]) -> bool) -> Vec<T> {
    if test(&[]) {
        return Vec::new();
    }
    let mut items = items;
    let mut n = 2usize;
    while items.len() >= 2 {
        let size = items.len().div_ceil(n);
        let chunks: Vec<Vec<T>> = items.chunks(size).map(<[T]>::to_vec).collect();
        if let Some(c) = chunks.iter().find(|c| test(c)) {
            items = c.clone();
            n = 2;
            continue;
        }
        let complement = (0..chunks.len()).find_map(|skip| {
            let rest: Vec<T> = chunks
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != skip)
                .flat_map(|(_, c)| c.iter().cloned())
                .collect();
            test(&rest).then_some(rest)
        });
        if let Some(rest) = complement {
            items = rest;
            n = (n - 1).max(2);
            continue;
        }
        if n >= items.len() {
            break;
        }
        n = (n * 2).min(items.len());
    }
    items
}

pub struct Shrunk {
    pub config: SimConfig,
    pub report: RunReport,
    pub target: ViolationKind,
    pub runs: usize,
    pub original_faults: usize,
    pub original_ops: usize,
    pub original_clients: usize,
}

struct Search {
    target: ViolationKind,
    runs: usize,
    max_runs: usize,
    best: RunReport,
}

impl Search {
    fn fails(&mut self, cfg: &SimConfig) -> bool {
        if self.runs >= self.max_runs {
            return false;
        }
        self.runs += 1;
        let report = sim::run(cfg);
        if report.kinds().contains(&self.target) {
            self.best = report;
            true
        } else {
            false
        }
    }
}

/// Shrinks a failing run while preserving its earliest violation kind.
pub fn shrink(base: &SimConfig, failing: &RunReport, max_runs: usize) -> Shrunk {
    let target = failing.primary().expect("shrink requires a failing run");
    let mut cfg = base.clone();
    cfg.trace = false;
    cfg.faults = FaultMode::Explicit(failing.plan.clone());
    let mut s = Search { target, runs: 0, max_runs, best: failing.clone() };

    loop {
        let before = (plan_len(&cfg), cfg.clients, cfg.ops_per_client, cfg.chaos, cfg.crash_points);

        // 1. Faults.
        let events = explicit_plan(&cfg).events;
        let kept = ddmin(events, |subset| {
            let mut c = cfg.clone();
            c.faults = FaultMode::Explicit(FaultPlan { events: subset.to_vec() });
            s.fails(&c)
        });
        cfg.faults = FaultMode::Explicit(FaultPlan { events: kept });

        // 2. Clients (the highest-numbered ones; others keep their streams).
        while cfg.clients > 1 {
            let mut c = cfg.clone();
            c.clients -= 1;
            if !s.fails(&c) {
                break;
            }
            cfg = c;
        }

        // 3. Operations per client.
        let (mut lo, mut hi) = (1, cfg.ops_per_client);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mut c = cfg.clone();
            c.ops_per_client = mid;
            if s.fails(&c) { hi = mid } else { lo = mid + 1 }
        }
        cfg.ops_per_client = hi;

        // 4. Recover as soon as the last fault has happened.
        let last = explicit_plan(&cfg).events.iter().map(|e| ms(e.at_ms) + 1).max().unwrap_or(0);
        if last < cfg.chaos {
            let mut c = cfg.clone();
            c.chaos = last;
            if s.fails(&c) {
                cfg = c;
            }
        }

        // 5. Crash points are drawn at run time, so ddmin cannot see them;
        //    try without them altogether.
        if cfg.crash_points {
            let mut c = cfg.clone();
            c.crash_points = false;
            if s.fails(&c) {
                cfg = c;
            }
        }

        let after = (plan_len(&cfg), cfg.clients, cfg.ops_per_client, cfg.chaos, cfg.crash_points);
        if after == before || s.runs >= s.max_runs {
            break;
        }
    }

    // Re-run the final configuration so the report matches it exactly.
    let report = sim::run(&cfg);
    let report = if report.kinds().contains(&target) { report } else { s.best };
    Shrunk {
        config: cfg,
        report,
        target,
        runs: s.runs,
        original_faults: failing.plan.len(),
        original_ops: base.ops_per_client,
        original_clients: base.clients,
    }
}

fn explicit_plan(cfg: &SimConfig) -> FaultPlan {
    match &cfg.faults {
        FaultMode::Explicit(p) => p.clone(),
        _ => FaultPlan::default(),
    }
}

fn plan_len(cfg: &SimConfig) -> usize {
    explicit_plan(cfg).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddmin_finds_the_minimal_pair() {
        let items: Vec<u32> = (0..40).collect();
        let mut calls = 0;
        let out = ddmin(items, |s| {
            calls += 1;
            s.contains(&3) && s.contains(&31)
        });
        assert_eq!(out, vec![3, 31]);
        assert!(calls < 200, "{calls} tests");
    }

    #[test]
    fn ddmin_handles_empty_answer() {
        assert!(ddmin(vec![1, 2, 3], |_| true).is_empty());
    }

    #[test]
    fn ddmin_single_culprit() {
        assert_eq!(ddmin((0..17).collect(), |s: &[i32]| s.contains(&16)), vec![16]);
    }
}
