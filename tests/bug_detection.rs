//! Every injected bug must be found from random seeds alone, and must be
//! reported as the *right kind* of violation. This is what gives a green run
//! of the correct implementation its meaning.

use hourglass::bugs::{Bug, BugSet};
use hourglass::checker::ViolationKind;
use hourglass::fuzz::fuzz;
use hourglass::sim::{SimConfig, run};

/// Searches seeds until `bug` produces a violation; returns (seed, kinds).
fn hunt(bug: Bug, nodes: usize, seeds: std::ops::Range<u64>) -> (u64, Vec<ViolationKind>) {
    let base = SimConfig { nodes, bugs: BugSet::NONE.with(bug), ..SimConfig::default() };
    let summary = fuzz(&base, seeds.clone(), 4, Some(1), &|_, _| {});
    let first =
        summary.failures.first().unwrap_or_else(|| panic!("{} not detected in seeds {seeds:?}", bug.name()));
    // Whatever seed was found must reproduce on its own.
    let again = run(&SimConfig { seed: first.seed, ..base });
    assert!(again.failed(), "seed {} did not reproduce", first.seed);
    (first.seed, first.kinds.clone())
}

#[test]
fn stale_reads_break_linearizability() {
    let (_, kinds) = hunt(Bug::StaleRead, 5, 0..200);
    assert!(kinds.contains(&ViolationKind::Linearizability), "{kinds:?}");
}

#[test]
fn acking_before_commit_loses_writes() {
    let (_, kinds) = hunt(Bug::AckBeforeCommit, 5, 0..200);
    assert!(kinds.contains(&ViolationKind::Linearizability), "{kinds:?}");
}

#[test]
fn skipping_the_log_check_breaks_leader_completeness() {
    let (_, kinds) = hunt(Bug::NoLogCheck, 5, 0..200);
    assert!(kinds.contains(&ViolationKind::LeaderCompleteness), "{kinds:?}");
}

#[test]
fn missing_dedup_applies_retries_twice() {
    let (_, kinds) = hunt(Bug::NoDedup, 5, 0..200);
    assert!(kinds.contains(&ViolationKind::Linearizability), "{kinds:?}");
}

#[test]
fn forgetting_votes_elects_two_leaders() {
    // Needs a crash right after a vote plus a split election: rare, but crash
    // points make it reachable within a few hundred seeds.
    let (_, kinds) = hunt(Bug::ForgetVote, 5, 0..3000);
    assert!(kinds.contains(&ViolationKind::ElectionSafety), "{kinds:?}");
}

#[test]
fn committing_old_terms_is_the_figure_8_bug() {
    // Raft paper, Figure 8. Smaller clusters and single-entry batches make
    // the required interleaving far more likely.
    let (_, kinds) = hunt(Bug::CommitOldTerm, 3, 0..3000);
    assert!(kinds.contains(&ViolationKind::LeaderCompleteness), "{kinds:?}");
}
