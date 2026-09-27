//! The simulated network: latency, loss, duplication, reordering and
//! partitions, all driven by a seeded generator.

use crate::raft::{Addr, NodeId};
use crate::rng::Rng;
use crate::time::{Time, ms};
use std::collections::BTreeSet;

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub min_latency: Time,
    pub max_latency: Time,
    /// Baseline probability of silently losing a message.
    pub drop: f64,
    /// Probability that a message is delivered twice.
    pub dup: f64,
    /// Probability that a message is held back by up to `spike_max`, which
    /// reorders it relative to later traffic.
    pub spike: f64,
    pub spike_max: Time,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig {
            min_latency: ms(1),
            max_latency: ms(8),
            drop: 0.005,
            dup: 0.01,
            spike: 0.02,
            spike_max: ms(150),
        }
    }
}

pub struct Network {
    cfg: NetConfig,
    rng: Rng,
    /// Extra loss injected by the nemesis, on top of `cfg.drop`.
    extra_loss: f64,
    /// Directed node pairs that cannot currently talk.
    blocked: BTreeSet<(NodeId, NodeId)>,
}

impl Network {
    pub fn new(cfg: NetConfig, rng: Rng) -> Self {
        Network { cfg, rng, extra_loss: 0.0, blocked: BTreeSet::new() }
    }

    /// Partitions only ever separate servers; clients sit outside the
    /// cluster and can reach every node (so a client can keep talking to a
    /// deposed leader — exactly the situation that exposes stale reads).
    pub fn blocked(&self, from: Addr, to: Addr) -> bool {
        match (from, to) {
            (Addr::Node(a), Addr::Node(b)) => self.blocked.contains(&(a, b)),
            _ => false,
        }
    }

    /// Delays for each copy of a message about to be sent: empty if it is
    /// lost, two entries if it is duplicated.
    pub fn sample(&mut self) -> Vec<Time> {
        if self.rng.chance(self.cfg.drop + self.extra_loss) {
            return Vec::new();
        }
        let copies = if self.rng.chance(self.cfg.dup) { 2 } else { 1 };
        (0..copies)
            .map(|_| {
                let mut d = self.rng.range(self.cfg.min_latency, self.cfg.max_latency + 1);
                if self.rng.chance(self.cfg.spike) {
                    d += self.rng.range(0, self.cfg.spike_max + 1);
                }
                d
            })
            .collect()
    }

    /// Splits the cluster into groups that cannot talk to each other. Nodes
    /// not listed in any group form one extra group together.
    pub fn partition(&mut self, n: usize, groups: &[Vec<NodeId>]) {
        let group_of = |x: NodeId| groups.iter().position(|g| g.contains(&x)).unwrap_or(usize::MAX);
        self.blocked.clear();
        for a in 0..n as NodeId {
            for b in 0..n as NodeId {
                if a != b && group_of(a) != group_of(b) {
                    self.blocked.insert((a, b));
                }
            }
        }
    }

    /// A non-transitive partition: `left` and `right` cannot talk to each
    /// other, but `hub` can talk to everyone.
    pub fn bridge(&mut self, left: &[NodeId], right: &[NodeId]) {
        self.blocked.clear();
        for &a in left {
            for &b in right {
                self.blocked.insert((a, b));
                self.blocked.insert((b, a));
            }
        }
    }

    pub fn heal(&mut self) {
        self.blocked.clear();
    }

    pub fn set_loss(&mut self, p: f64) {
        self.extra_loss = p;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_blocks_across_groups_only() {
        let mut net = Network::new(NetConfig::default(), Rng::new(0));
        net.partition(5, &[vec![0, 1], vec![2, 3, 4]]);
        assert!(net.blocked(Addr::Node(0), Addr::Node(2)));
        assert!(net.blocked(Addr::Node(4), Addr::Node(1)));
        assert!(!net.blocked(Addr::Node(0), Addr::Node(1)));
        assert!(!net.blocked(Addr::Node(2), Addr::Node(4)));
        assert!(!net.blocked(Addr::Client(0), Addr::Node(0)));
        net.heal();
        assert!(!net.blocked(Addr::Node(0), Addr::Node(2)));
    }

    #[test]
    fn bridge_is_not_transitive() {
        let mut net = Network::new(NetConfig::default(), Rng::new(0));
        net.bridge(&[0, 1], &[3, 4]);
        assert!(net.blocked(Addr::Node(0), Addr::Node(3)));
        assert!(!net.blocked(Addr::Node(0), Addr::Node(2)));
        assert!(!net.blocked(Addr::Node(2), Addr::Node(4)));
    }

    #[test]
    fn full_loss_drops_everything() {
        let mut net = Network::new(NetConfig::default(), Rng::new(0));
        net.set_loss(1.0);
        assert!((0..100).all(|_| net.sample().is_empty()));
    }
}
