//! A run must be a pure function of its configuration. Everything else in
//! this project — replay, shrinking, sharing a seed in a bug report — depends
//! on that.

use hourglass::bugs::{Bug, BugSet};
use hourglass::sim::{FaultMode, SimConfig, run};

fn cfg(seed: u64) -> SimConfig {
    SimConfig { seed, ops_per_client: 60, ..SimConfig::default() }
}

#[test]
fn same_seed_same_universe() {
    for seed in 0..25 {
        let a = run(&cfg(seed));
        let b = run(&cfg(seed));
        assert_eq!(a.fingerprint, b.fingerprint, "seed {seed} diverged");
        assert_eq!(a.history, b.history);
        assert_eq!(a.plan, b.plan);
    }
}

#[test]
fn different_seeds_differ() {
    let prints: std::collections::BTreeSet<u64> = (0..25).map(|s| run(&cfg(s)).fingerprint).collect();
    assert_eq!(prints.len(), 25);
}

#[test]
fn printed_fault_plan_replays_the_generated_run() {
    // The text form of a plan must capture the schedule completely: replaying
    // it explicitly reproduces the generated run event for event.
    for seed in 0..25 {
        let generated = run(&cfg(seed));
        let text = generated.plan.to_string();
        let explicit = SimConfig { faults: FaultMode::Explicit(text.parse().unwrap()), ..cfg(seed) };
        assert_eq!(run(&explicit).fingerprint, generated.fingerprint, "seed {seed}: {text}");
    }
}

#[test]
fn tracing_does_not_perturb_the_run() {
    let quiet = run(&cfg(3));
    let traced = run(&SimConfig { trace: true, ..cfg(3) });
    assert_eq!(quiet.fingerprint, traced.fingerprint);
    assert!(!traced.trace.is_empty());
    assert!(quiet.trace.is_empty());
}

#[test]
fn buggy_runs_are_deterministic_too() {
    let bugs = BugSet::NONE.with(Bug::StaleRead).with(Bug::NoDedup);
    for seed in 0..10 {
        let c = SimConfig { bugs, ..cfg(seed) };
        let (a, b) = (run(&c), run(&c));
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.kinds(), b.kinds());
    }
}
