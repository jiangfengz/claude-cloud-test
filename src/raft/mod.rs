//! A sans-IO Raft node.
//!
//! The node never touches a socket, clock or disk. It is a pure state machine:
//! the simulator feeds it messages and timer ticks along with the current
//! virtual time, and it answers with a list of [`Output`]s. "Persistence" is
//! modelled by [`Durable`], which is exactly the state that survives
//! [`RaftNode::crash`]; everything else is lost.
//!
//! This implementation covers leader election, log replication, the commit
//! rule, linearizable reads through the log, and client session
//! deduplication. It omits snapshots and membership changes.

mod types;

pub use types::*;

use crate::bugs::{Bug, BugSet};
use crate::kv::{Applied, ClientId, KvOp, KvResult, KvStore, OpKind};
use crate::rng::Rng;
use crate::time::{Time, ms};
use std::collections::BTreeSet;

#[derive(Clone, Debug)]
pub struct RaftConfig {
    pub election_min: Time,
    pub election_max: Time,
    /// Leader heartbeat interval; candidates also re-send vote requests at
    /// this cadence.
    pub heartbeat: Time,
    /// Maximum entries per `Append` message.
    pub max_batch: usize,
}

impl Default for RaftConfig {
    fn default() -> Self {
        RaftConfig { election_min: ms(150), election_max: ms(300), heartbeat: ms(40), max_batch: 4 }
    }
}

pub struct RaftNode {
    id: NodeId,
    n: usize,
    cfg: RaftConfig,
    bugs: BugSet,
    rng: Rng,

    pub durable: Durable,

    role: Role,
    leader_hint: Option<NodeId>,
    commit: Index,
    applied: Index,
    kv: KvStore,
    votes: BTreeSet<NodeId>,
    next_index: Vec<Index>,
    match_index: Vec<Index>,
    election_deadline: Time,
    /// Leader: next heartbeat. Candidate: next vote-request retry.
    heartbeat_deadline: Time,
}

impl RaftNode {
    /// Boots a node from its durable state (empty on first start).
    pub fn new(
        id: NodeId,
        n: usize,
        cfg: RaftConfig,
        bugs: BugSet,
        durable: Durable,
        rng: Rng,
        now: Time,
    ) -> Self {
        let mut node = RaftNode {
            id,
            n,
            cfg,
            bugs,
            rng,
            durable,
            role: Role::Follower,
            leader_hint: None,
            commit: 0,
            applied: 0,
            kv: KvStore::default(),
            votes: BTreeSet::new(),
            next_index: vec![1; n],
            match_index: vec![0; n],
            election_deadline: 0,
            heartbeat_deadline: Time::MAX,
        };
        node.reset_election_timer(now);
        node
    }

    /// Kills the node, returning only what it had persisted.
    pub fn crash(self) -> Durable {
        let mut durable = self.durable;
        if self.bugs.has(Bug::ForgetVote) {
            durable.voted_for = None;
        }
        durable
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn term(&self) -> Term {
        self.durable.term
    }

    pub fn commit_index(&self) -> Index {
        self.commit
    }

    pub fn kv(&self) -> &KvStore {
        &self.kv
    }

    /// When this node next needs [`RaftNode::tick`] to be called.
    pub fn next_deadline(&self) -> Time {
        match self.role {
            Role::Leader => self.heartbeat_deadline,
            Role::Candidate => self.election_deadline.min(self.heartbeat_deadline),
            Role::Follower => self.election_deadline,
        }
    }

    pub fn tick(&mut self, now: Time, out: &mut Vec<Output>) {
        match self.role {
            Role::Leader => {
                if now >= self.heartbeat_deadline {
                    self.broadcast_append(out);
                    self.heartbeat_deadline = now + self.cfg.heartbeat;
                }
            }
            Role::Candidate | Role::Follower => {
                if now >= self.election_deadline {
                    self.start_election(now, out);
                } else if self.role == Role::Candidate && now >= self.heartbeat_deadline {
                    // RPCs are retried until answered (Raft §5.1).
                    self.request_votes(out);
                    self.heartbeat_deadline = now + self.cfg.heartbeat;
                }
            }
        }
    }

    pub fn handle(&mut self, now: Time, from: Addr, msg: Msg, out: &mut Vec<Output>) {
        if let Msg::Request { client, seq, op } = msg {
            self.on_request(client, seq, op, out);
            return;
        }
        let (Addr::Node(peer), Some(term)) = (from, msg.term()) else {
            return;
        };
        if term > self.durable.term {
            self.become_follower(now, term, None);
        }
        match msg {
            Msg::RequestVote { term, last_index, last_term } => {
                self.on_request_vote(now, peer, term, last_index, last_term, out);
            }
            Msg::Vote { term, granted } => self.on_vote(now, peer, term, granted, out),
            Msg::Append { term, prev_index, prev_term, entries, commit } => {
                self.on_append(now, peer, term, prev_index, prev_term, entries, commit, out);
            }
            Msg::AppendAck { term, success, match_index, hint } => {
                self.on_append_ack(peer, term, success, match_index, hint, out);
            }
            Msg::Request { .. } | Msg::Response { .. } => {}
        }
    }

    // ----------------------------------------------------------------- roles

    fn majority(&self) -> usize {
        self.n / 2 + 1
    }

    fn peers(&self) -> impl Iterator<Item = NodeId> + use<> {
        let me = self.id;
        (0..self.n as NodeId).filter(move |&p| p != me)
    }

    fn reset_election_timer(&mut self, now: Time) {
        self.election_deadline = now + self.rng.range(self.cfg.election_min, self.cfg.election_max + 1);
    }

    fn become_follower(&mut self, now: Time, term: Term, leader: Option<NodeId>) {
        if term > self.durable.term {
            self.durable.term = term;
            self.durable.voted_for = None;
        }
        if self.role != Role::Follower {
            self.role = Role::Follower;
            self.heartbeat_deadline = Time::MAX;
            self.reset_election_timer(now);
        }
        self.votes.clear();
        self.leader_hint = leader;
    }

    fn start_election(&mut self, now: Time, out: &mut Vec<Output>) {
        self.durable.term += 1;
        self.durable.voted_for = Some(self.id);
        out.push(Output::Persisted(Persist::Vote));
        self.role = Role::Candidate;
        self.leader_hint = None;
        self.votes.clear();
        self.votes.insert(self.id);
        self.reset_election_timer(now);
        self.heartbeat_deadline = now + self.cfg.heartbeat;
        if self.votes.len() >= self.majority() {
            self.become_leader(now, out);
        } else {
            self.request_votes(out);
        }
    }

    fn request_votes(&mut self, out: &mut Vec<Output>) {
        let msg = Msg::RequestVote {
            term: self.durable.term,
            last_index: self.durable.last_index(),
            last_term: self.durable.last_term(),
        };
        for p in self.peers() {
            if !self.votes.contains(&p) {
                out.push(Output::Send(Addr::Node(p), msg.clone()));
            }
        }
    }

    fn become_leader(&mut self, now: Time, out: &mut Vec<Output>) {
        self.role = Role::Leader;
        self.leader_hint = Some(self.id);
        self.votes.clear();
        let term = self.durable.term;
        self.durable.log.push(Entry { term, cmd: Command::Noop });
        let last = self.durable.last_index();
        // Start by offering peers the no-op; rejections walk this back.
        self.next_index = vec![last; self.n];
        self.match_index = vec![0; self.n];
        out.push(Output::BecameLeader(term));
        self.broadcast_append(out);
        self.heartbeat_deadline = now + self.cfg.heartbeat;
        self.advance_commit(out);
    }

    // ------------------------------------------------------------- elections

    fn on_request_vote(
        &mut self,
        now: Time,
        candidate: NodeId,
        term: Term,
        last_index: Index,
        last_term: Term,
        out: &mut Vec<Output>,
    ) {
        let up_to_date = (last_term, last_index) >= (self.durable.last_term(), self.durable.last_index());
        let free = self.durable.voted_for.is_none_or(|v| v == candidate);
        let granted = term == self.durable.term && free && (up_to_date || self.bugs.has(Bug::NoLogCheck));
        if granted {
            self.durable.voted_for = Some(candidate);
            out.push(Output::Persisted(Persist::Vote));
            self.reset_election_timer(now);
        }
        let reply = Msg::Vote { term: self.durable.term, granted };
        out.push(Output::Send(Addr::Node(candidate), reply));
    }

    fn on_vote(&mut self, now: Time, peer: NodeId, term: Term, granted: bool, out: &mut Vec<Output>) {
        if self.role == Role::Candidate && term == self.durable.term && granted {
            self.votes.insert(peer);
            if self.votes.len() >= self.majority() {
                self.become_leader(now, out);
            }
        }
    }

    // ----------------------------------------------------------- replication

    fn broadcast_append(&mut self, out: &mut Vec<Output>) {
        for p in self.peers() {
            self.send_append(p, out);
        }
    }

    fn send_append(&mut self, peer: NodeId, out: &mut Vec<Output>) {
        let next = self.next_index[peer as usize].max(1);
        let prev_index = next - 1;
        let Some(prev_term) = self.durable.term_at(prev_index) else {
            return;
        };
        let end = self.durable.last_index().min(prev_index + self.cfg.max_batch as Index);
        let entries = self.durable.log[prev_index as usize..end as usize].to_vec();
        let msg =
            Msg::Append { term: self.durable.term, prev_index, prev_term, entries, commit: self.commit };
        out.push(Output::Send(Addr::Node(peer), msg));
    }

    #[allow(clippy::too_many_arguments)]
    fn on_append(
        &mut self,
        now: Time,
        leader: NodeId,
        term: Term,
        prev_index: Index,
        prev_term: Term,
        entries: Vec<Entry>,
        leader_commit: Index,
        out: &mut Vec<Output>,
    ) {
        let reject = |me: &Self, hint: Index| Msg::AppendAck {
            term: me.durable.term,
            success: false,
            match_index: 0,
            hint,
        };
        if term < self.durable.term {
            out.push(Output::Send(Addr::Node(leader), reject(self, 0)));
            return;
        }
        if self.role != Role::Follower {
            self.become_follower(now, term, Some(leader));
        }
        self.leader_hint = Some(leader);
        self.reset_election_timer(now);

        match self.durable.term_at(prev_index) {
            None => {
                let hint = self.durable.last_index() + 1;
                out.push(Output::Send(Addr::Node(leader), reject(self, hint)));
            }
            Some(t) if t != prev_term => {
                // Skip back over the whole conflicting term at once.
                let mut hint = prev_index;
                while hint > 1 && self.durable.term_at(hint - 1) == Some(t) {
                    hint -= 1;
                }
                out.push(Output::Send(Addr::Node(leader), reject(self, hint)));
            }
            Some(_) => {
                let mut index = prev_index;
                let mut changed = false;
                for entry in entries {
                    index += 1;
                    match self.durable.term_at(index) {
                        // Already have it: never truncate on a stale or
                        // duplicated message.
                        Some(t) if t == entry.term => {}
                        Some(_) => {
                            self.durable.truncate(index - 1);
                            self.durable.log.push(entry);
                            changed = true;
                        }
                        None => {
                            self.durable.log.push(entry);
                            changed = true;
                        }
                    }
                }
                if changed {
                    out.push(Output::Persisted(Persist::Log));
                }
                // Only `..=index` is known to match the leader; anything after
                // it may be a stale suffix from an older term.
                let new_commit = leader_commit.min(index);
                if new_commit > self.commit {
                    self.commit = new_commit;
                    self.apply(out);
                }
                let ack =
                    Msg::AppendAck { term: self.durable.term, success: true, match_index: index, hint: 0 };
                out.push(Output::Send(Addr::Node(leader), ack));
            }
        }
    }

    fn on_append_ack(
        &mut self,
        peer: NodeId,
        term: Term,
        success: bool,
        match_index: Index,
        hint: Index,
        out: &mut Vec<Output>,
    ) {
        if self.role != Role::Leader || term != self.durable.term {
            return;
        }
        let p = peer as usize;
        // Acks can arrive reordered or duplicated, so only an ack that makes
        // progress may trigger the next send. Reacting to every ack lets each
        // duplicate fork another replication chain, and the chains multiply
        // on every round-trip (the simulator found this as a message storm:
        // 27M events for 6s of cluster time with 5% duplication).
        if success {
            if match_index <= self.match_index[p] {
                return;
            }
            self.match_index[p] = match_index;
            self.next_index[p] = self.next_index[p].max(match_index + 1);
            self.advance_commit(out);
            if self.next_index[p] <= self.durable.last_index() {
                self.send_append(peer, out);
            }
        } else {
            let floor = self.match_index[p] + 1;
            let next = self.next_index[p].min(hint.max(floor)).max(1);
            if next < self.next_index[p] {
                self.next_index[p] = next;
                self.send_append(peer, out);
            }
        }
    }

    fn advance_commit(&mut self, out: &mut Vec<Output>) {
        let me = self.id as usize;
        self.match_index[me] = self.durable.last_index();
        let mut sorted = self.match_index.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        let candidate = sorted[self.majority() - 1];
        if candidate <= self.commit {
            return;
        }
        // Raft §5.4.2: only entries from the current term are committed by
        // counting replicas; earlier ones are committed indirectly.
        let current = self.durable.term_at(candidate) == Some(self.durable.term);
        if current || self.bugs.has(Bug::CommitOldTerm) {
            let from = self.commit + 1;
            self.commit = candidate;
            out.push(Output::Committed { from, to: candidate, term: self.durable.term });
            self.apply(out);
        }
    }

    fn apply(&mut self, out: &mut Vec<Output>) {
        let upto = self.commit.min(self.durable.last_index());
        while self.applied < upto {
            self.applied += 1;
            let entry = &self.durable.log[self.applied as usize - 1];
            out.push(Output::Applied { index: self.applied, term: entry.term, digest: entry.cmd.digest() });
            if let Command::Client { client, seq, op } = entry.cmd {
                let dedup = !self.bugs.has(Bug::NoDedup);
                let outcome = self.kv.apply(client, seq, op, dedup);
                if self.role == Role::Leader
                    && let Applied::Fresh(r) | Applied::Duplicate(r) = outcome
                {
                    reply(client, seq, Ok(r), out);
                }
            }
        }
    }

    // --------------------------------------------------------------- clients

    fn on_request(&mut self, client: ClientId, seq: u64, op: KvOp, out: &mut Vec<Output>) {
        if self.role != Role::Leader {
            reply(client, seq, Err(ClientError::NotLeader(self.leader_hint)), out);
            return;
        }
        if op.is_read() && self.bugs.has(Bug::StaleRead) {
            reply(client, seq, Ok(KvResult::Read(self.kv.get(op.key))), out);
            return;
        }
        if !self.bugs.has(Bug::NoDedup)
            && let Some((last, result)) = self.kv.session(client)
        {
            if seq == last {
                reply(client, seq, Ok(result), out);
                return;
            }
            if seq < last {
                return;
            }
        }
        let term = self.durable.term;
        self.durable.log.push(Entry { term, cmd: Command::Client { client, seq, op } });
        out.push(Output::Persisted(Persist::Log));
        if self.bugs.has(Bug::AckBeforeCommit) && matches!(op.kind, OpKind::Write(_)) {
            reply(client, seq, Ok(KvResult::Written), out);
        }
        self.broadcast_append(out);
        self.advance_commit(out);
    }
}

fn reply(client: ClientId, seq: u64, result: Result<KvResult, ClientError>, out: &mut Vec<Output>) {
    out.push(Output::Send(Addr::Client(client), Msg::Response { seq, result }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: NodeId, n: usize) -> RaftNode {
        RaftNode::new(id, n, RaftConfig::default(), BugSet::NONE, Durable::default(), Rng::new(id as u64), 0)
    }

    fn sends(out: &[Output]) -> Vec<(Addr, Msg)> {
        out.iter()
            .filter_map(|o| match o {
                Output::Send(a, m) => Some((*a, m.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn single_node_elects_itself_and_commits() {
        let mut n = node(0, 1);
        let mut out = vec![];
        n.tick(ms(1000), &mut out);
        assert_eq!(n.role(), Role::Leader);
        let op = KvOp { key: 0, kind: OpKind::Write(7) };
        n.handle(ms(1001), Addr::Client(0), Msg::Request { client: 0, seq: 1, op }, &mut out);
        assert_eq!(n.kv().get(0), Some(7));
        assert_eq!(n.commit_index(), 2);
    }

    #[test]
    fn follower_does_not_truncate_on_stale_append() {
        let mut f = node(1, 3);
        let e = |term| Entry { term, cmd: Command::Noop };
        let mut out = vec![];
        let append = |entries: Vec<Entry>, prev_index| Msg::Append {
            term: 1,
            prev_index,
            prev_term: if prev_index == 0 { 0 } else { 1 },
            entries,
            commit: 0,
        };
        f.handle(0, Addr::Node(0), append(vec![e(1), e(1), e(1)], 0), &mut out);
        assert_eq!(f.durable.last_index(), 3);
        // A delayed copy of an older, shorter append must not shorten the log.
        f.handle(0, Addr::Node(0), append(vec![e(1)], 0), &mut out);
        assert_eq!(f.durable.last_index(), 3);
    }

    #[test]
    fn vote_requires_up_to_date_log() {
        let mut v = node(1, 3);
        v.durable.log.push(Entry { term: 2, cmd: Command::Noop });
        v.durable.term = 2;
        let mut out = vec![];
        let req = Msg::RequestVote { term: 3, last_index: 5, last_term: 1 };
        v.handle(0, Addr::Node(0), req, &mut out);
        assert!(matches!(sends(&out)[0].1, Msg::Vote { granted: false, .. }));
        // The same request is granted when the bug is enabled.
        let mut buggy = RaftNode::new(
            1,
            3,
            RaftConfig::default(),
            BugSet::NONE.with(Bug::NoLogCheck),
            v.durable.clone(),
            Rng::new(1),
            0,
        );
        buggy.durable.voted_for = None;
        out.clear();
        buggy.handle(0, Addr::Node(0), Msg::RequestVote { term: 3, last_index: 5, last_term: 1 }, &mut out);
        assert!(matches!(sends(&out)[0].1, Msg::Vote { granted: true, .. }));
    }
}
