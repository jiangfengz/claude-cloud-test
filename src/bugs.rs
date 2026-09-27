//! Deliberately injectable Raft bugs.
//!
//! A checker that never fails proves nothing. Each [`Bug`] re-creates a
//! mistake that real Raft implementations have shipped; the test suite asserts
//! that the simulator finds every one of them from random seeds alone, which
//! is how we know the checkers have teeth.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bug {
    /// The leader answers reads from its local state machine without going
    /// through the log, so a deposed leader (or a fresh leader that has not
    /// yet applied earlier terms' entries) serves stale data.
    StaleRead,
    /// The leader acknowledges a write as soon as it is appended to its own
    /// log, before a majority has it. If the leader then dies, the
    /// acknowledged write can vanish.
    AckBeforeCommit,
    /// `votedFor` is not persisted: after a crash a node may vote twice in the
    /// same term, allowing two leaders in one term.
    ForgetVote,
    /// Votes are granted without the "candidate's log is at least as
    /// up-to-date" check, so a node missing committed entries can win and
    /// overwrite them.
    NoLogCheck,
    /// The state machine does not deduplicate client requests, so a delayed
    /// or retried request is applied twice.
    NoDedup,
    /// The leader commits entries from previous terms by counting replicas —
    /// the infamous "Figure 8" scenario from the Raft paper.
    CommitOldTerm,
}

impl Bug {
    pub const ALL: [Bug; 6] = [
        Bug::StaleRead,
        Bug::AckBeforeCommit,
        Bug::ForgetVote,
        Bug::NoLogCheck,
        Bug::NoDedup,
        Bug::CommitOldTerm,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Bug::StaleRead => "stale-read",
            Bug::AckBeforeCommit => "ack-before-commit",
            Bug::ForgetVote => "forget-vote",
            Bug::NoLogCheck => "no-log-check",
            Bug::NoDedup => "no-dedup",
            Bug::CommitOldTerm => "commit-old-term",
        }
    }

    pub fn from_name(s: &str) -> Option<Bug> {
        Bug::ALL.into_iter().find(|b| b.name() == s)
    }

    pub fn description(self) -> &'static str {
        match self {
            Bug::StaleRead => "leader serves reads from local state without a quorum round-trip",
            Bug::AckBeforeCommit => "leader acks writes once appended locally, before commit",
            Bug::ForgetVote => "votedFor is not persisted across crashes",
            Bug::NoLogCheck => "votes are granted without the log up-to-date check",
            Bug::NoDedup => "state machine applies retried/duplicated requests twice",
            Bug::CommitOldTerm => "leader commits previous-term entries by replica count (Figure 8)",
        }
    }

    fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// A set of enabled bugs. The empty set is a correct Raft.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct BugSet(u32);

impl BugSet {
    pub const NONE: BugSet = BugSet(0);

    pub fn with(self, bug: Bug) -> BugSet {
        BugSet(self.0 | bug.bit())
    }

    pub fn has(self, bug: Bug) -> bool {
        self.0 & bug.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Bug> {
        Bug::ALL.into_iter().filter(move |b| self.has(*b))
    }

    /// Parses a comma-separated list such as `stale-read,no-dedup`.
    pub fn parse(s: &str) -> Result<BugSet, String> {
        let mut set = BugSet::NONE;
        for name in s.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if name == "none" {
                continue;
            }
            let bug = Bug::from_name(name).ok_or_else(|| {
                let known: Vec<_> = Bug::ALL.iter().map(|b| b.name()).collect();
                format!("unknown bug `{name}` (known: {})", known.join(", "))
            })?;
            set = set.with(bug);
        }
        Ok(set)
    }
}

impl fmt::Display for BugSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let names: Vec<_> = self.iter().map(Bug::name).collect();
        f.write_str(&names.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_round_trip() {
        let set = BugSet::parse("no-dedup, stale-read").unwrap();
        assert!(set.has(Bug::StaleRead) && set.has(Bug::NoDedup));
        assert!(!set.has(Bug::ForgetVote));
        assert_eq!(BugSet::parse(&set.to_string()).unwrap(), set);
        assert_eq!(BugSet::NONE.to_string(), "none");
        assert!(BugSet::parse("typo").is_err());
    }
}
