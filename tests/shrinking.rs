//! Shrinking must produce a smaller configuration that still fails the same
//! way — and that failure must reproduce from the printed command line alone.

use hourglass::bugs::{Bug, BugSet};
use hourglass::nemesis::FaultPlan;
use hourglass::shrink::shrink;
use hourglass::sim::{FaultMode, SimConfig, run};

#[test]
fn shrinks_a_stale_read_to_a_handful_of_faults() {
    let base = SimConfig { seed: 2, bugs: BugSet::NONE.with(Bug::StaleRead), ..SimConfig::default() };
    let failing = run(&base);
    assert!(failing.failed());
    let original_faults = failing.plan.len();

    let shrunk = shrink(&base, &failing, 400);
    let FaultMode::Explicit(plan) = &shrunk.config.faults else { panic!("expected an explicit plan") };

    assert!(plan.len() < original_faults, "{} → {}", original_faults, plan.len());
    assert!(plan.len() <= 3, "still {} faults: {plan}", plan.len());
    assert!(shrunk.config.ops_per_client < base.ops_per_client);
    assert!(shrunk.report.kinds().contains(&shrunk.target));

    // Round-trip through text, as a user pasting the repro command would.
    let replay = SimConfig {
        faults: FaultMode::Explicit(plan.to_string().parse::<FaultPlan>().unwrap()),
        ..shrunk.config.clone()
    };
    let again = run(&replay);
    assert!(again.kinds().contains(&shrunk.target));
    assert_eq!(again.fingerprint, shrunk.report.fingerprint);
}

#[test]
fn shrinking_respects_its_budget() {
    let base = SimConfig { seed: 0, bugs: BugSet::NONE.with(Bug::NoLogCheck), ..SimConfig::default() };
    let failing = run(&base);
    assert!(failing.failed());
    let shrunk = shrink(&base, &failing, 10);
    assert!(shrunk.runs <= 10);
    assert!(shrunk.report.kinds().contains(&shrunk.target));
}
