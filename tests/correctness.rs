//! The unmodified Raft implementation must survive every seed: any violation
//! here is either a real bug in the implementation or a false positive in a
//! checker, and both need fixing.

use hourglass::fuzz::fuzz;
use hourglass::sim::{FaultMode, SimConfig, run};

fn assert_clean(base: SimConfig, seeds: std::ops::Range<u64>) {
    let summary = fuzz(&base, seeds, 4, None, &|_, _| {});
    let failing: Vec<_> = summary.failures.iter().map(|f| (f.seed, f.primary)).collect();
    assert!(failing.is_empty(), "correct Raft failed: {failing:?}");
    assert_eq!(summary.lin_unknown, 0, "linearizability search exhausted its budget");
}

#[test]
fn five_nodes_survive_swarm_chaos() {
    assert_clean(SimConfig::default(), 0..400);
}

#[test]
fn three_nodes_survive_swarm_chaos() {
    assert_clean(SimConfig { nodes: 3, ..SimConfig::default() }, 1000..1300);
}

#[test]
fn seven_nodes_survive_swarm_chaos() {
    assert_clean(SimConfig { nodes: 7, ops_per_client: 60, ..SimConfig::default() }, 2000..2200);
}

#[test]
fn fixed_fault_mix_without_swarm() {
    assert_clean(SimConfig { swarm: false, ..SimConfig::default() }, 3000..3200);
}

#[test]
fn single_node_cluster_is_trivially_correct() {
    assert_clean(SimConfig { nodes: 1, ops_per_client: 40, ..SimConfig::default() }, 0..50);
}

#[test]
fn quiet_network_completes_every_operation() {
    let r = run(&SimConfig { seed: 11, faults: FaultMode::Disabled, ..SimConfig::default() });
    assert!(!r.failed());
    // No faults: every operation gets a definite answer.
    assert_eq!(r.history.indeterminate(), 0);
    assert_eq!(r.history.ops.len(), 4 * 120);
}

#[test]
fn linearizable_verdicts_come_with_a_witness_order() {
    use hourglass::checker::wgl::Verdict;
    let r = run(&SimConfig { seed: 5, ..SimConfig::default() });
    for kv in &r.verdicts {
        let Verdict::Linearizable(order) = &kv.verdict else { panic!("key {} not linearizable", kv.key) };
        let mut sorted = order.clone();
        sorted.sort_unstable();
        let mut expected = kv.ops.clone();
        expected.sort_unstable();
        assert_eq!(sorted, expected, "witness must be a permutation of the key's operations");
    }
}
