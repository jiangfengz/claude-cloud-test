use crate::kv::{ClientId, KvOp, KvResult};
use crate::rng::Fnv;
use std::fmt;

pub type NodeId = u8;
pub type Term = u64;
/// 1-based log index; 0 means "before the first entry".
pub type Index = u64;

/// Anything that can send or receive a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Addr {
    Node(NodeId),
    Client(ClientId),
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Addr::Node(n) => write!(f, "n{n}"),
            Addr::Client(c) => write!(f, "c{c}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    /// Appended by every new leader so it can commit entries from earlier
    /// terms (Raft §5.4.2) and serve linearizable reads.
    Noop,
    Client {
        client: ClientId,
        seq: u64,
        op: KvOp,
    },
}

impl Command {
    pub fn digest(&self) -> u64 {
        let mut h = Fnv::new();
        match self {
            Command::Noop => h.mix(0),
            Command::Client { client, seq, op } => {
                h.mix(1);
                h.mix(u64::from(*client));
                h.mix(*seq);
                op.mix_into(&mut h);
            }
        }
        h.finish()
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Noop => f.write_str("noop"),
            Command::Client { client, seq, op } => write!(f, "c{client}#{seq} {op}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub term: Term,
    pub cmd: Command,
}

/// State that survives a crash: Raft's `currentTerm`, `votedFor` and `log[]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Durable {
    pub term: Term,
    pub voted_for: Option<NodeId>,
    pub log: Vec<Entry>,
}

impl Durable {
    pub fn last_index(&self) -> Index {
        self.log.len() as Index
    }

    pub fn last_term(&self) -> Term {
        self.log.last().map_or(0, |e| e.term)
    }

    /// Term of the entry at `i`; `Some(0)` for index 0, `None` past the end.
    pub fn term_at(&self, i: Index) -> Option<Term> {
        if i == 0 { Some(0) } else { self.log.get(i as usize - 1).map(|e| e.term) }
    }

    pub fn entry(&self, i: Index) -> Option<&Entry> {
        if i == 0 { None } else { self.log.get(i as usize - 1) }
    }

    /// Keeps entries `1..=keep`.
    pub fn truncate(&mut self, keep: Index) {
        self.log.truncate(keep as usize);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// This node is not the leader; the payload is its best guess of who is.
    NotLeader(Option<NodeId>),
}

#[derive(Clone, Debug)]
pub enum Msg {
    RequestVote {
        term: Term,
        last_index: Index,
        last_term: Term,
    },
    Vote {
        term: Term,
        granted: bool,
    },
    Append {
        term: Term,
        prev_index: Index,
        prev_term: Term,
        entries: Vec<Entry>,
        commit: Index,
    },
    AppendAck {
        term: Term,
        success: bool,
        /// On success: the last index known to match the leader's log.
        match_index: Index,
        /// On failure: where the leader should retry from.
        hint: Index,
    },
    Request {
        client: ClientId,
        seq: u64,
        op: KvOp,
    },
    Response {
        seq: u64,
        result: Result<KvResult, ClientError>,
    },
}

impl Msg {
    /// The Raft term carried by protocol messages.
    pub fn term(&self) -> Option<Term> {
        match self {
            Msg::RequestVote { term, .. }
            | Msg::Vote { term, .. }
            | Msg::Append { term, .. }
            | Msg::AppendAck { term, .. } => Some(*term),
            Msg::Request { .. } | Msg::Response { .. } => None,
        }
    }

    /// A compact numeric summary of the message, used for run fingerprints.
    pub fn fingerprint(&self, h: &mut Fnv) {
        match self {
            Msg::RequestVote { term, last_index, last_term } => {
                h.mix(1);
                h.mix(*term);
                h.mix(*last_index);
                h.mix(*last_term);
            }
            Msg::Vote { term, granted } => {
                h.mix(2);
                h.mix(*term);
                h.mix(u64::from(*granted));
            }
            Msg::Append { term, prev_index, prev_term, entries, commit } => {
                h.mix(3);
                h.mix(*term);
                h.mix(*prev_index);
                h.mix(*prev_term);
                h.mix(entries.len() as u64);
                h.mix(*commit);
            }
            Msg::AppendAck { term, success, match_index, hint } => {
                h.mix(4);
                h.mix(*term);
                h.mix(u64::from(*success));
                h.mix(*match_index);
                h.mix(*hint);
            }
            Msg::Request { client, seq, op } => {
                h.mix(5);
                h.mix(u64::from(*client));
                h.mix(*seq);
                op.mix_into(h);
            }
            Msg::Response { seq, result } => {
                h.mix(6);
                h.mix(*seq);
                h.mix(u64::from(result.is_ok()));
            }
        }
    }
}

impl fmt::Display for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Msg::RequestVote { term, last_index, last_term } => {
                write!(f, "RequestVote t{term} last={last_index}@t{last_term}")
            }
            Msg::Vote { term, granted } => {
                write!(f, "Vote t{term} {}", if *granted { "granted" } else { "denied" })
            }
            Msg::Append { term, prev_index, prev_term, entries, commit } => {
                write!(f, "Append t{term} prev={prev_index}@t{prev_term} commit={commit}")?;
                if !entries.is_empty() {
                    write!(f, " +{}", entries.len())?;
                }
                Ok(())
            }
            Msg::AppendAck { term, success: true, match_index, .. } => {
                write!(f, "AppendOk t{term} match={match_index}")
            }
            Msg::AppendAck { term, success: false, hint, .. } => {
                write!(f, "AppendReject t{term} hint={hint}")
            }
            Msg::Request { client, seq, op } => write!(f, "Request c{client}#{seq} {op}"),
            Msg::Response { seq, result: Ok(r) } => write!(f, "Response #{seq} {r}"),
            Msg::Response { seq, result: Err(ClientError::NotLeader(h)) } => match h {
                Some(h) => write!(f, "Response #{seq} not-leader (try n{h})"),
                None => write!(f, "Response #{seq} not-leader"),
            },
        }
    }
}

/// Everything a node wants the outside world to do or know after a step.
#[derive(Clone, Debug)]
pub enum Output {
    Send(Addr, Msg),
    /// Observation for the checkers: this node just won an election.
    BecameLeader(Term),
    /// Observation: as leader of `term`, this node marked `from..=to`
    /// committed.
    Committed {
        from: Index,
        to: Index,
        term: Term,
    },
    /// Observation: this node applied the entry at `index`.
    Applied {
        index: Index,
        term: Term,
        digest: u64,
    },
    /// Observation: this node just made a durable change. The simulator may
    /// choose to crash it right here — a "crash point".
    Persisted(Persist),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Persist {
    Vote,
    Log,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}
