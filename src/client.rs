//! Simulated clients.
//!
//! Each client issues one operation at a time against a random key. It sends
//! the request to whichever node it believes is the leader, follows
//! `NotLeader` redirects, rotates to another node on timeout, and retries with
//! the *same* sequence number so the cluster can deduplicate. After
//! `give_up_after` it abandons the operation, which is then recorded as
//! indeterminate.

use crate::history::History;
use crate::kv::{ClientId, Key, KvOp, KvResult, OpKind, Value};
use crate::raft::{Addr, ClientError, Msg, NodeId};
use crate::rng::Rng;
use crate::time::{Time, ms};

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub request_timeout: Time,
    pub give_up_after: Time,
    pub think_min: Time,
    pub think_max: Time,
    /// Delay before following a redirect.
    pub backoff: Time,
    pub read_pct: u64,
    pub write_pct: u64,
    // The remainder are compare-and-set.
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            request_timeout: ms(200),
            give_up_after: ms(2500),
            think_min: ms(0),
            think_max: ms(30),
            backoff: ms(15),
            read_pct: 45,
            write_pct: 35,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    op: KvOp,
    seq: u64,
    hist: usize,
    give_up_at: Time,
    retry_at: Time,
    /// Whether the next retry should try a different node (timeout) or the
    /// current target (redirect).
    rotate: bool,
}

#[derive(Clone, Copy, Debug)]
enum State {
    Idle { next_at: Time },
    Waiting(Pending),
    Done,
}

pub struct Client {
    id: ClientId,
    nodes: usize,
    keys: u8,
    cfg: ClientConfig,
    rng: Rng,
    target: NodeId,
    seq: u64,
    remaining: usize,
    state: State,
    /// Last value observed per key, used to aim CAS operations.
    last_seen: Vec<Option<Value>>,
}

pub type Outbox = Vec<(Addr, Msg)>;

impl Client {
    pub fn new(id: ClientId, nodes: usize, ops: usize, keys: u8, cfg: ClientConfig, mut rng: Rng) -> Self {
        let target = rng.below(nodes as u64) as NodeId;
        let first = rng.range(cfg.think_min, cfg.think_max + 1);
        Client {
            id,
            nodes,
            keys,
            cfg,
            rng,
            target,
            seq: 0,
            remaining: ops,
            state: State::Idle { next_at: first },
            last_seen: vec![None; keys as usize],
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    pub fn deadline(&self) -> Option<Time> {
        match self.state {
            State::Idle { next_at } => Some(next_at),
            State::Waiting(p) => Some(p.retry_at),
            State::Done => None,
        }
    }

    /// Operations not yet finished (including an in-flight one).
    pub fn outstanding(&self) -> usize {
        self.remaining + usize::from(matches!(self.state, State::Waiting(_)))
    }

    pub fn wake(&mut self, now: Time, hist: &mut History, out: &mut Outbox) {
        match self.state {
            State::Idle { next_at } if now >= next_at => {
                if self.remaining == 0 {
                    self.state = State::Done;
                } else {
                    self.start(now, hist, out);
                }
            }
            State::Waiting(mut p) if now >= p.retry_at => {
                if now >= p.give_up_at {
                    // Outcome unknown: the history keeps it open forever.
                    self.state = State::Idle { next_at: now + self.think() };
                    return;
                }
                if p.rotate {
                    self.target = self.other_node();
                }
                p.rotate = true;
                p.retry_at = now + self.cfg.request_timeout;
                self.state = State::Waiting(p);
                self.send(p, out);
            }
            _ => {}
        }
    }

    pub fn on_msg(&mut self, now: Time, msg: Msg, hist: &mut History) {
        let Msg::Response { seq, result } = msg else { return };
        let State::Waiting(mut p) = self.state else { return };
        if p.seq != seq {
            return; // a late answer to an operation we already gave up on
        }
        match result {
            Ok(r) => {
                hist.complete(p.hist, now, r);
                let key = p.op.key as usize;
                match (p.op.kind, r) {
                    (OpKind::Read, KvResult::Read(v)) => self.last_seen[key] = v,
                    (OpKind::Write(v), _) => self.last_seen[key] = Some(v),
                    (OpKind::Cas { new, .. }, KvResult::Cas(true)) => self.last_seen[key] = Some(new),
                    _ => {}
                }
                self.state = State::Idle { next_at: now + self.think() };
            }
            Err(ClientError::NotLeader(hint)) => {
                self.target = match hint {
                    Some(h) if h != self.target => h,
                    _ => self.other_node(),
                };
                p.rotate = false;
                p.retry_at = now + self.cfg.backoff;
                self.state = State::Waiting(p);
            }
        }
    }

    fn start(&mut self, now: Time, hist: &mut History, out: &mut Outbox) {
        self.remaining -= 1;
        self.seq += 1;
        let key = self.rng.below(u64::from(self.keys)) as Key;
        // Values are unique per (client, seq), which keeps the checker's
        // search space small and makes histories easy to read.
        let value = self.seq * 10 + u64::from(self.id);
        let roll = self.rng.below(100);
        let kind = if roll < self.cfg.read_pct {
            OpKind::Read
        } else if roll < self.cfg.read_pct + self.cfg.write_pct {
            OpKind::Write(value)
        } else {
            OpKind::Cas { expect: self.last_seen[key as usize], new: value }
        };
        let op = KvOp { key, kind };
        let p = Pending {
            op,
            seq: self.seq,
            hist: hist.invoke(self.id, op, now),
            give_up_at: now + self.cfg.give_up_after,
            retry_at: now + self.cfg.request_timeout,
            rotate: true,
        };
        self.state = State::Waiting(p);
        self.send(p, out);
    }

    fn send(&self, p: Pending, out: &mut Outbox) {
        let msg = Msg::Request { client: self.id, seq: p.seq, op: p.op };
        out.push((Addr::Node(self.target), msg));
    }

    fn other_node(&mut self) -> NodeId {
        if self.nodes <= 1 {
            return 0;
        }
        let skip = self.rng.range(1, self.nodes as u64) as NodeId;
        (self.target + skip) % self.nodes as NodeId
    }

    fn think(&mut self) -> Time {
        self.rng.range(self.cfg.think_min, self.cfg.think_max + 1)
    }
}
