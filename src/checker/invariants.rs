//! Raft's safety properties, checked online from a god's-eye view.
//!
//! The simulator can see every node at once — something no real deployment
//! can — so it reports the exact moment a protocol-level invariant breaks,
//! usually long before the damage becomes visible to clients.

use super::{Violation, ViolationKind};
use crate::raft::{Durable, Index, NodeId, Term};
use crate::time::{self, Time};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct InvariantChecker {
    /// Who won each term.
    leaders: BTreeMap<Term, NodeId>,
    /// index -> (entry term, term in which it was first committed)
    committed: BTreeMap<Index, (Term, Term)>,
    /// index -> (node, entry term, digest) of the first application.
    applied: BTreeMap<Index, (NodeId, Term, u64)>,
    reported: BTreeSet<ViolationKind>,
    pub violations: Vec<Violation>,
}

impl InvariantChecker {
    fn report(&mut self, kind: ViolationKind, at: Time, message: String) {
        // One report per kind keeps the output readable; the first occurrence
        // is the interesting one.
        if self.reported.insert(kind) {
            self.violations.push(Violation { kind, at, message, ops: vec![], culprit: None });
        }
    }

    /// Election Safety and Leader Completeness.
    pub fn on_leader(&mut self, now: Time, node: NodeId, term: Term, log: &Durable) {
        if let Some(&other) = self.leaders.get(&term)
            && other != node
        {
            self.report(
                ViolationKind::ElectionSafety,
                now,
                format!("n{node} and n{other} were both elected leader of term {term}"),
            );
        }
        self.leaders.insert(term, node);

        // Only entries committed in *earlier* terms are guaranteed: a node can
        // legitimately win an old term late (delayed votes) without entries
        // committed by a newer leader in the meantime.
        let missing = self.committed.iter().find(|&(&idx, &(entry_term, commit_term))| {
            commit_term < term && log.term_at(idx) != Some(entry_term)
        });
        if let Some((&idx, &(entry_term, commit_term))) = missing {
            let has = log.term_at(idx).map_or("nothing".to_string(), |t| format!("an entry from term {t}"));
            self.report(
                ViolationKind::LeaderCompleteness,
                now,
                format!(
                    "n{node} became leader of term {term} but has {has} at index {idx}, \
                     where an entry from term {entry_term} was committed in term {commit_term}"
                ),
            );
        }
    }

    pub fn on_commit(&mut self, index: Index, entry_term: Term, commit_term: Term) {
        self.committed.entry(index).or_insert((entry_term, commit_term));
    }

    /// State Machine Safety.
    pub fn on_apply(&mut self, now: Time, node: NodeId, index: Index, term: Term, digest: u64) {
        match self.applied.get(&index) {
            None => {
                self.applied.insert(index, (node, term, digest));
            }
            Some(&(first, first_term, first_digest)) if (first_term, first_digest) != (term, digest) => {
                self.report(
                    ViolationKind::StateMachineSafety,
                    now,
                    format!(
                        "n{node} applied an entry from term {term} at index {index}, \
                         but n{first} had applied a different one (term {first_term}) there"
                    ),
                );
            }
            Some(_) => {}
        }
    }

    /// Log Matching: if two logs contain an entry with the same index and
    /// term, the logs are identical up to that index.
    pub fn check_log_matching(&mut self, now: Time, logs: &[(NodeId, &Durable)]) {
        for (i, &(a, la)) in logs.iter().enumerate() {
            for &(b, lb) in &logs[i + 1..] {
                let common = la.last_index().min(lb.last_index());
                let Some(anchor) = (1..=common).rev().find(|&k| la.term_at(k) == lb.term_at(k)) else {
                    continue;
                };
                if let Some(k) = (1..=anchor).find(|&k| la.entry(k) != lb.entry(k)) {
                    self.report(
                        ViolationKind::LogMatching,
                        now,
                        format!(
                            "n{a} and n{b} agree on index {anchor} (term {}) but differ at index {k}",
                            la.term_at(anchor).unwrap_or(0)
                        ),
                    );
                    return;
                }
            }
        }
    }

    pub fn summary(&self) -> String {
        format!("{} terms with leaders, {} entries committed", self.leaders.len(), self.committed.len())
    }
}

pub fn describe(v: &Violation) -> String {
    format!("[{}] {} at {}: {}", v.kind, v.kind.name(), time::fmt(v.at), v.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::{Command, Entry};

    fn log(terms: &[Term]) -> Durable {
        Durable {
            term: terms.last().copied().unwrap_or(0),
            voted_for: None,
            log: terms.iter().map(|&term| Entry { term, cmd: Command::Noop }).collect(),
        }
    }

    #[test]
    fn detects_two_leaders_in_one_term() {
        let mut c = InvariantChecker::default();
        c.on_leader(0, 0, 3, &log(&[1, 3]));
        c.on_leader(1, 1, 3, &log(&[1, 3]));
        assert_eq!(c.violations[0].kind, ViolationKind::ElectionSafety);
    }

    #[test]
    fn leader_completeness_respects_commit_term() {
        let mut c = InvariantChecker::default();
        c.on_commit(2, 2, 4);
        // A late leader of term 3 may lack an entry committed in term 4...
        c.on_leader(0, 0, 3, &log(&[1]));
        assert!(c.violations.is_empty());
        // ...but a leader of term 5 may not.
        c.on_leader(0, 1, 5, &log(&[1]));
        assert_eq!(c.violations[0].kind, ViolationKind::LeaderCompleteness);
    }

    #[test]
    fn detects_divergent_application() {
        let mut c = InvariantChecker::default();
        c.on_apply(0, 0, 1, 1, 111);
        c.on_apply(0, 1, 1, 1, 111);
        assert!(c.violations.is_empty());
        c.on_apply(0, 2, 1, 2, 222);
        assert_eq!(c.violations[0].kind, ViolationKind::StateMachineSafety);
    }

    #[test]
    fn detects_log_mismatch() {
        let mut c = InvariantChecker::default();
        let a = log(&[1, 1, 2]);
        let mut b = log(&[1, 1, 2]);
        b.log[1].cmd = Command::Client {
            client: 0,
            seq: 1,
            op: crate::kv::KvOp { key: 0, kind: crate::kv::OpKind::Read },
        };
        c.check_log_matching(0, &[(0, &a), (1, &b)]);
        assert_eq!(c.violations[0].kind, ViolationKind::LogMatching);
    }
}
