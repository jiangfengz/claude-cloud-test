//! The replicated state machine: a tiny key-value store of registers with
//! read / write / compare-and-set, plus a per-client session table that gives
//! exactly-once semantics to retried requests.

use crate::rng::Fnv;
use std::collections::BTreeMap;
use std::fmt;

pub type Key = u8;
pub type Value = u64;
pub type ClientId = u8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpKind {
    Read,
    Write(Value),
    /// Set to `new` iff the current value equals `expect` (`None` = absent).
    Cas {
        expect: Option<Value>,
        new: Value,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KvOp {
    pub key: Key,
    pub kind: OpKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KvResult {
    Read(Option<Value>),
    Written,
    Cas(bool),
}

impl KvOp {
    pub fn is_read(&self) -> bool {
        self.kind == OpKind::Read
    }

    pub fn mix_into(&self, h: &mut Fnv) {
        h.mix(u64::from(self.key));
        match self.kind {
            OpKind::Read => h.mix(0),
            OpKind::Write(v) => {
                h.mix(1);
                h.mix(v);
            }
            OpKind::Cas { expect, new } => {
                h.mix(2);
                h.mix(expect.map_or(u64::MAX, |e| e));
                h.mix(new);
            }
        }
    }
}

pub fn fmt_value(v: Option<Value>) -> String {
    v.map_or_else(|| "∅".to_string(), |v| v.to_string())
}

impl fmt::Display for KvOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            OpKind::Read => write!(f, "r(k{})", self.key),
            OpKind::Write(v) => write!(f, "w(k{},{v})", self.key),
            OpKind::Cas { expect, new } => {
                write!(f, "cas(k{},{}→{new})", self.key, fmt_value(expect))
            }
        }
    }
}

impl fmt::Display for KvResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KvResult::Read(v) => f.write_str(&fmt_value(*v)),
            KvResult::Written | KvResult::Cas(true) => f.write_str("ok"),
            KvResult::Cas(false) => f.write_str("fail"),
        }
    }
}

/// What happened when a command was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// First time this request was seen: it was executed.
    Fresh(KvResult),
    /// A retry of the latest request: the cached result is returned.
    Duplicate(KvResult),
    /// An older request than the latest one — a delayed duplicate. Ignored.
    Stale,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KvStore {
    data: BTreeMap<Key, Value>,
    /// client -> (latest sequence number applied, its result)
    sessions: BTreeMap<ClientId, (u64, KvResult)>,
}

impl KvStore {
    pub fn get(&self, key: Key) -> Option<Value> {
        self.data.get(&key).copied()
    }

    pub fn session(&self, client: ClientId) -> Option<(u64, KvResult)> {
        self.sessions.get(&client).copied()
    }

    pub fn apply(&mut self, client: ClientId, seq: u64, op: KvOp, dedup: bool) -> Applied {
        if dedup && let Some((last, result)) = self.session(client) {
            if seq == last {
                return Applied::Duplicate(result);
            }
            if seq < last {
                return Applied::Stale;
            }
        }
        let result = self.execute(op);
        self.sessions.insert(client, (seq, result));
        Applied::Fresh(result)
    }

    fn execute(&mut self, op: KvOp) -> KvResult {
        match op.kind {
            OpKind::Read => KvResult::Read(self.get(op.key)),
            OpKind::Write(v) => {
                self.data.insert(op.key, v);
                KvResult::Written
            }
            OpKind::Cas { expect, new } => {
                if self.get(op.key) == expect {
                    self.data.insert(op.key, new);
                    KvResult::Cas(true)
                } else {
                    KvResult::Cas(false)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(key: Key, kind: OpKind) -> KvOp {
        KvOp { key, kind }
    }

    #[test]
    fn register_semantics() {
        let mut kv = KvStore::default();
        assert_eq!(kv.apply(0, 1, op(1, OpKind::Read), true), Applied::Fresh(KvResult::Read(None)));
        let cas = OpKind::Cas { expect: None, new: 5 };
        assert_eq!(kv.apply(0, 2, op(1, cas), true), Applied::Fresh(KvResult::Cas(true)));
        let cas = OpKind::Cas { expect: Some(4), new: 6 };
        assert_eq!(kv.apply(0, 3, op(1, cas), true), Applied::Fresh(KvResult::Cas(false)));
        assert_eq!(kv.get(1), Some(5));
    }

    #[test]
    fn sessions_give_exactly_once() {
        let mut kv = KvStore::default();
        kv.apply(0, 1, op(0, OpKind::Write(10)), true);
        kv.apply(1, 1, op(0, OpKind::Write(20)), true);
        // A retry of client 0's request must not clobber client 1's write.
        assert_eq!(kv.apply(0, 1, op(0, OpKind::Write(10)), true), Applied::Duplicate(KvResult::Written));
        assert_eq!(kv.get(0), Some(20));
        kv.apply(0, 2, op(0, OpKind::Read), true);
        assert_eq!(kv.apply(0, 1, op(0, OpKind::Write(10)), true), Applied::Stale);
        // Without deduplication the stale request is re-executed.
        kv.apply(0, 1, op(0, OpKind::Write(10)), false);
        assert_eq!(kv.get(0), Some(10));
    }
}
