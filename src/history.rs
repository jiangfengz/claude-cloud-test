//! The client-observed history: what each client asked for, when, and what it
//! was told. This — not any node's internal state — is what the
//! linearizability checker judges.

use crate::kv::{ClientId, Key, KvOp, KvResult};
use crate::time::{self, Time};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistOp {
    pub id: usize,
    pub client: ClientId,
    pub op: KvOp,
    pub invoke: Time,
    /// `None` if the client never learned the outcome (it timed out). Such an
    /// operation may or may not have taken effect — at any point after it was
    /// invoked.
    pub complete: Option<(Time, KvResult)>,
}

impl HistOp {
    pub fn key(&self) -> Key {
        self.op.key
    }

    pub fn ret_time(&self) -> Time {
        self.complete.map_or(Time::MAX, |(t, _)| t)
    }

    pub fn result(&self) -> Option<KvResult> {
        self.complete.map(|(_, r)| r)
    }
}

impl fmt::Display for HistOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{} c{} {}", self.id, self.client, self.op)?;
        match self.complete {
            Some((_, r)) if self.op.is_read() => write!(f, " → {r}"),
            Some((_, r)) => write!(f, " {r}"),
            None => write!(f, " ?"),
        }?;
        write!(f, "  [{} … {}]", time::fmt(self.invoke), time::fmt(self.ret_time()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    pub ops: Vec<HistOp>,
}

impl History {
    pub fn invoke(&mut self, client: ClientId, op: KvOp, at: Time) -> usize {
        let id = self.ops.len();
        self.ops.push(HistOp { id, client, op, invoke: at, complete: None });
        id
    }

    pub fn complete(&mut self, id: usize, at: Time, result: KvResult) {
        self.ops[id].complete = Some((at, result));
    }

    pub fn completed(&self) -> usize {
        self.ops.iter().filter(|o| o.complete.is_some()).count()
    }

    pub fn indeterminate(&self) -> usize {
        self.ops.len() - self.completed()
    }

    pub fn keys(&self) -> Vec<Key> {
        let mut keys: Vec<Key> = self.ops.iter().map(HistOp::key).collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}
