//! Correctness oracles.
//!
//! Two independent layers judge every run:
//!
//! * [`invariants`] watches the cluster from a god's-eye view and checks
//!   Raft's safety properties (Figure 3 of the paper) as they happen.
//! * [`wgl`] checks, after the fact, that the history observed by clients is
//!   linearizable — the property users actually care about. It knows nothing
//!   about Raft.

pub mod invariants;
pub mod wgl;

use crate::history::{HistOp, History};
use crate::kv::{Key, KvResult, OpKind, Value};
use crate::time::Time;
use std::fmt;
use wgl::{Model, Operation, Verdict};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ViolationKind {
    /// Two leaders were elected in the same term.
    ElectionSafety,
    /// A new leader was missing an entry committed in an earlier term.
    LeaderCompleteness,
    /// Two logs agree on an entry's term at some index but differ before it.
    LogMatching,
    /// Two nodes applied different commands at the same index.
    StateMachineSafety,
    /// The client-observed history has no valid sequential explanation.
    Linearizability,
    /// The cluster failed to finish the workload after all faults healed.
    Liveness,
}

impl ViolationKind {
    pub fn name(self) -> &'static str {
        match self {
            ViolationKind::ElectionSafety => "election-safety",
            ViolationKind::LeaderCompleteness => "leader-completeness",
            ViolationKind::LogMatching => "log-matching",
            ViolationKind::StateMachineSafety => "state-machine-safety",
            ViolationKind::Linearizability => "linearizability",
            ViolationKind::Liveness => "liveness",
        }
    }
}

impl fmt::Display for ViolationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub kind: ViolationKind,
    pub at: Time,
    pub message: String,
    /// Operation ids involved, for linearizability violations.
    pub ops: Vec<usize>,
    /// The operation the checker could not place.
    pub culprit: Option<usize>,
}

/// A register per key: `None` until first written.
pub struct RegisterModel;

impl Model for RegisterModel {
    type State = Option<Value>;
    type Input = OpKind;
    type Output = KvResult;

    fn init(&self) -> Option<Value> {
        None
    }

    fn step(&self, s: &Option<Value>, input: &OpKind, output: Option<&KvResult>) -> Option<Option<Value>> {
        match (*input, output) {
            (OpKind::Read, Some(KvResult::Read(v))) => (s == v).then_some(*s),
            (OpKind::Read, None) => Some(*s),
            (OpKind::Write(v), None | Some(KvResult::Written)) => Some(Some(v)),
            (OpKind::Cas { expect, new }, Some(KvResult::Cas(true))) => (*s == expect).then_some(Some(new)),
            (OpKind::Cas { expect, .. }, Some(KvResult::Cas(false))) => (*s != expect).then_some(*s),
            // Unknown outcome: CAS is deterministic given the state.
            (OpKind::Cas { expect, new }, None) => Some(if *s == expect { Some(new) } else { *s }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct KeyVerdict {
    pub key: Key,
    /// History ids of the operations on this key that were checked.
    pub ops: Vec<usize>,
    pub verdict: Verdict,
}

/// Checks each key independently. Registers are independent objects, and
/// linearizability is *local* (Herlihy & Wing): a history is linearizable iff
/// its restriction to each object is, so this is sound and far cheaper than
/// checking the whole history at once.
pub fn check_history(history: &History, budget: u64) -> Vec<KeyVerdict> {
    history
        .keys()
        .into_iter()
        .map(|key| {
            // A read whose result we never saw constrains nothing.
            let relevant: Vec<&HistOp> = history
                .ops
                .iter()
                .filter(|o| o.key() == key && !(o.op.is_read() && o.complete.is_none()))
                .collect();
            let ops: Vec<Operation<OpKind, KvResult>> = relevant
                .iter()
                .map(|o| Operation {
                    call: o.invoke,
                    ret: o.complete.map(|(t, _)| t),
                    input: o.op.kind,
                    output: o.result(),
                })
                .collect();
            let verdict = match wgl::check(&RegisterModel, &ops, budget) {
                Verdict::Linearizable(order) => {
                    Verdict::Linearizable(order.into_iter().map(|i| relevant[i].id).collect())
                }
                Verdict::NotLinearizable(mut cx) => {
                    cx.longest_prefix = cx.longest_prefix.into_iter().map(|i| relevant[i].id).collect();
                    cx.stuck_on = relevant[cx.stuck_on].id;
                    Verdict::NotLinearizable(cx)
                }
                Verdict::Unknown => Verdict::Unknown,
            };
            KeyVerdict { key, ops: relevant.iter().map(|o| o.id).collect(), verdict }
        })
        .collect()
}
