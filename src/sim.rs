//! The deterministic discrete-event simulator.
//!
//! A single-threaded event loop owns every node, client, network link and
//! timer. Events are ordered by `(virtual time, insertion sequence)`, and all
//! randomness comes from streams derived from the run's seed — so a run is a
//! pure function of its [`SimConfig`]. [`RunReport::fingerprint`] hashes the
//! full event sequence so that property can itself be tested.
//!
//! A run has two phases:
//!
//! 1. **Chaos** — the nemesis executes its [`FaultPlan`] while clients run
//!    their workload.
//! 2. **Recovery** — at `recover_at` all partitions heal and every node is
//!    restarted. The cluster must then finish the workload within
//!    `recovery_limit`, or the run fails with a liveness violation.

use crate::bugs::BugSet;
use crate::checker::invariants::InvariantChecker;
use crate::checker::wgl::Verdict;
use crate::checker::{KeyVerdict, Violation, ViolationKind, check_history};
use crate::client::{Client, ClientConfig};
use crate::history::History;
use crate::nemesis::{Fault, FaultPlan, Swarm, Target};
use crate::net::{NetConfig, Network};
use crate::raft::{Addr, Durable, Msg, NodeId, Output, Persist, RaftConfig, RaftNode, Role, Term};
use crate::rng::{Fnv, Rng};
use crate::time::{self, Time, ms};
use crate::viz::{Lane, Timeline};
use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};

const STREAM_NET: u64 = 1;
const STREAM_NEMESIS: u64 = 2;
const STREAM_SWARM: u64 = 3;
const STREAM_BUGGIFY: u64 = 4;

fn client_stream(c: usize) -> u64 {
    100 + c as u64
}

fn node_stream(node: usize, incarnation: u64) -> u64 {
    10_000 + node as u64 * 1_000 + incarnation
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FaultMode {
    /// Derive a fault plan from the seed.
    Generated,
    /// No faults at all (the network is still lossy and reordering).
    Disabled,
    /// Replay exactly this plan.
    Explicit(FaultPlan),
}

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub seed: u64,
    pub nodes: usize,
    pub clients: usize,
    pub ops_per_client: usize,
    pub keys: u8,
    pub bugs: BugSet,
    pub raft: RaftConfig,
    pub client: ClientConfig,
    /// Length of the chaos phase.
    pub chaos: Time,
    pub faults: FaultMode,
    /// Pick a random nemesis/network profile per seed.
    pub swarm: bool,
    /// Allow the swarm profile's BUGGIFY-style crash points.
    pub crash_points: bool,
    pub recovery_limit: Time,
    pub trace: bool,
    /// Search-step budget for each key's linearizability check.
    pub lin_budget: u64,
    /// A run processing more events than this is stopped and reported as a
    /// liveness failure: something is amplifying traffic.
    pub max_events: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            seed: 0,
            nodes: 5,
            clients: 4,
            ops_per_client: 120,
            keys: 3,
            bugs: BugSet::NONE,
            raft: RaftConfig::default(),
            client: ClientConfig::default(),
            chaos: ms(8_000),
            faults: FaultMode::Generated,
            swarm: true,
            crash_points: true,
            recovery_limit: ms(30_000),
            trace: false,
            lin_budget: 2_000_000,
            max_events: 2_000_000,
        }
    }
}

impl SimConfig {
    /// The command line that reproduces this configuration exactly.
    pub fn repro_command(&self) -> String {
        let d = SimConfig::default();
        let mut cmd = format!("hourglass run --seed {}", self.seed);
        let mut opt = |name: &str, differs: bool, value: String| {
            if differs {
                cmd += &format!(" --{name} {value}");
            }
        };
        opt("nodes", self.nodes != d.nodes, self.nodes.to_string());
        opt("clients", self.clients != d.clients, self.clients.to_string());
        opt("ops", self.ops_per_client != d.ops_per_client, self.ops_per_client.to_string());
        opt("keys", self.keys != d.keys, self.keys.to_string());
        opt("chaos", self.chaos != d.chaos, (self.chaos / time::MILLIS).to_string());
        opt("bug", !self.bugs.is_empty(), self.bugs.to_string());
        if !self.swarm {
            cmd += " --no-swarm";
        }
        if !self.crash_points && self.faults != FaultMode::Disabled {
            cmd += " --no-crash-points";
        }
        match &self.faults {
            FaultMode::Generated => {}
            FaultMode::Disabled => cmd += " --no-faults",
            FaultMode::Explicit(p) => cmd += &format!(" --faults '{p}'"),
        }
        cmd
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub events: u64,
    pub sent: u64,
    pub delivered: u64,
    pub dropped: u64,
    pub duplicated: u64,
    pub elections: u64,
    pub crashes: u64,
    /// Crashes injected at crash points (subset of `crashes`).
    pub crash_points: u64,
    pub restarts: u64,
    pub faults: u64,
    pub max_term: Term,
    pub max_log: u64,
    pub virtual_time: Time,
    /// Keys whose linearizability check ran out of budget.
    pub lin_unknown: usize,
}

#[derive(Clone, Debug)]
pub struct RunReport {
    pub seed: u64,
    pub swarm: Swarm,
    pub plan: FaultPlan,
    pub recover_at: Time,
    pub violations: Vec<Violation>,
    pub history: History,
    pub verdicts: Vec<KeyVerdict>,
    pub stats: Stats,
    pub timeline: Timeline,
    pub trace: Vec<String>,
    /// Hash of every event processed; equal fingerprints mean identical runs.
    pub fingerprint: u64,
}

impl RunReport {
    pub fn failed(&self) -> bool {
        !self.violations.is_empty()
    }

    pub fn kinds(&self) -> BTreeSet<ViolationKind> {
        self.violations.iter().map(|v| v.kind).collect()
    }

    /// The earliest violation — usually the root cause of any later ones.
    pub fn primary(&self) -> Option<ViolationKind> {
        self.violations.first().map(|v| v.kind)
    }
}

pub fn run(cfg: &SimConfig) -> RunReport {
    Sim::new(cfg).run()
}

// ------------------------------------------------------------------- events

enum Event {
    Deliver { from: Addr, to: Addr, msg: Msg },
    Timer { slot: usize, epoch: u64 },
    Fault(usize),
    Restart(usize),
    Recover,
}

struct Queued {
    at: Time,
    seq: u64,
    event: Event,
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    // Reversed: `BinaryHeap` is a max-heap and we want the earliest event.
    fn cmp(&self, other: &Self) -> Ordering {
        (other.at, other.seq).cmp(&(self.at, self.seq))
    }
}

#[derive(Clone, Copy, Default)]
struct TimerSlot {
    epoch: u64,
    at: Option<Time>,
}

fn addr_code(a: Addr) -> u64 {
    match a {
        Addr::Node(n) => u64::from(n),
        Addr::Client(c) => 1_000 + u64::from(c),
    }
}

fn lane(role: Role) -> Lane {
    match role {
        Role::Follower => Lane::Follower,
        Role::Candidate => Lane::Candidate,
        Role::Leader => Lane::Leader,
    }
}

// ---------------------------------------------------------------- simulator

struct Sim<'a> {
    cfg: &'a SimConfig,
    now: Time,
    seq: u64,
    queue: BinaryHeap<Queued>,
    nodes: Vec<Option<RaftNode>>,
    /// Durable state of crashed nodes, waiting for a restart.
    stash: Vec<Durable>,
    incarnation: Vec<u64>,
    clients: Vec<Client>,
    /// One per node, then one per client.
    timers: Vec<TimerSlot>,
    net: Network,
    raft: RaftConfig,
    buggify: Rng,
    swarm: Swarm,
    plan: FaultPlan,
    recover_at: Time,
    deadline: Time,
    history: History,
    inv: InvariantChecker,
    timeline: Timeline,
    stats: Stats,
    trace: Vec<String>,
    fp: Fnv,
    storm: bool,
}

impl<'a> Sim<'a> {
    fn new(cfg: &'a SimConfig) -> Self {
        assert!((1..=9).contains(&cfg.nodes), "1..=9 nodes supported");
        assert!((1..=9).contains(&cfg.clients), "1..=9 clients supported");
        assert!(cfg.keys >= 1, "need at least one key");
        let n = cfg.nodes;
        let mut swarm = if cfg.swarm {
            Swarm::random(&mut Rng::stream(cfg.seed, STREAM_SWARM))
        } else {
            Swarm::default()
        };
        if cfg.faults == FaultMode::Disabled || !cfg.crash_points {
            swarm.crash_on_vote = 0.0;
            swarm.crash_on_append = 0.0;
        }
        let plan = match &cfg.faults {
            FaultMode::Generated => {
                FaultPlan::generate(&mut Rng::stream(cfg.seed, STREAM_NEMESIS), n, cfg.chaos, &swarm)
            }
            FaultMode::Disabled => FaultPlan::default(),
            FaultMode::Explicit(p) => p.clone(),
        };
        let last_fault = plan.events.iter().map(|e| ms(e.at_ms) + 1).max().unwrap_or(0);
        let recover_at = cfg.chaos.max(last_fault);
        let net_cfg =
            NetConfig { drop: swarm.drop, dup: swarm.dup, spike: swarm.spike, ..NetConfig::default() };
        let raft = if cfg.swarm {
            RaftConfig { max_batch: swarm.max_batch, ..cfg.raft.clone() }
        } else {
            cfg.raft.clone()
        };

        let mut sim = Sim {
            cfg,
            now: 0,
            seq: 0,
            queue: BinaryHeap::new(),
            nodes: (0..n)
                .map(|x| {
                    let rng = Rng::stream(cfg.seed, node_stream(x, 0));
                    Some(RaftNode::new(x as NodeId, n, raft.clone(), cfg.bugs, Durable::default(), rng, 0))
                })
                .collect(),
            stash: vec![Durable::default(); n],
            incarnation: vec![0; n],
            clients: (0..cfg.clients)
                .map(|c| {
                    let rng = Rng::stream(cfg.seed, client_stream(c));
                    Client::new(c as u8, n, cfg.ops_per_client, cfg.keys, cfg.client.clone(), rng)
                })
                .collect(),
            timers: vec![TimerSlot::default(); n + cfg.clients],
            net: Network::new(net_cfg, Rng::stream(cfg.seed, STREAM_NET)),
            raft,
            buggify: Rng::stream(cfg.seed, STREAM_BUGGIFY),
            swarm,
            plan,
            recover_at,
            deadline: recover_at + cfg.recovery_limit,
            history: History::default(),
            inv: InvariantChecker::default(),
            timeline: Timeline::new(n),
            stats: Stats::default(),
            trace: Vec::new(),
            fp: Fnv::new(),
            storm: false,
        };
        for x in 0..n {
            sim.timeline.record(0, x as NodeId, Lane::Follower, 0);
        }
        for i in 0..sim.plan.events.len() {
            let at = ms(sim.plan.events[i].at_ms);
            sim.push(at, Event::Fault(i));
        }
        sim.push(recover_at, Event::Recover);
        for slot in 0..sim.timers.len() {
            sim.rearm(slot);
        }
        sim
    }

    fn push(&mut self, at: Time, event: Event) {
        self.seq += 1;
        self.queue.push(Queued { at, seq: self.seq, event });
    }

    fn log(&mut self, line: impl FnOnce() -> String) {
        if self.cfg.trace {
            let line = format!("{:>11}  {}", time::fmt_us(self.now), line());
            self.trace.push(line);
        }
    }

    fn run(mut self) -> RunReport {
        while let Some(Queued { at, event, .. }) = self.queue.pop() {
            if at > self.deadline {
                self.now = self.deadline;
                break;
            }
            self.now = at;
            self.stats.events += 1;
            if self.stats.events > self.cfg.max_events {
                self.storm = true;
                break;
            }
            self.fp.mix(at);
            match event {
                Event::Deliver { from, to, msg } => {
                    self.fp.mix(addr_code(from));
                    self.fp.mix(addr_code(to));
                    msg.fingerprint(&mut self.fp);
                    self.deliver(from, to, msg);
                }
                Event::Timer { slot, epoch } => {
                    self.fp.mix(10_000 + slot as u64);
                    self.fire_timer(slot, epoch);
                }
                Event::Fault(i) => {
                    self.fp.mix(20_000 + i as u64);
                    self.apply_fault(i);
                }
                Event::Restart(x) => {
                    self.fp.mix(30_000 + x as u64);
                    self.restart(x);
                }
                Event::Recover => {
                    self.fp.mix(40_000);
                    self.recover();
                }
            }
            if self.clients.iter().all(Client::is_done) {
                break;
            }
        }
        self.finish()
    }

    // ---------------------------------------------------------- messaging

    fn send(&mut self, from: Addr, to: Addr, msg: Msg) {
        self.stats.sent += 1;
        if self.net.blocked(from, to) {
            self.stats.dropped += 1;
            return;
        }
        let delays = self.net.sample();
        let Some((&last, rest)) = delays.split_last() else {
            self.stats.dropped += 1;
            return;
        };
        if !rest.is_empty() {
            self.stats.duplicated += 1;
        }
        for &d in rest {
            self.push(self.now + d, Event::Deliver { from, to, msg: msg.clone() });
        }
        self.push(self.now + last, Event::Deliver { from, to, msg });
    }

    fn deliver(&mut self, from: Addr, to: Addr, msg: Msg) {
        // Partitions are checked again on arrival: messages in flight when
        // a partition starts are lost too.
        if self.net.blocked(from, to) {
            self.stats.dropped += 1;
            self.log(|| format!("{from} ✂ {to}  {msg}  (partitioned)"));
            return;
        }
        match to {
            Addr::Node(x) => {
                let x = x as usize;
                if self.nodes[x].is_none() {
                    self.stats.dropped += 1;
                    self.log(|| format!("{from} ✂ {to}  {msg}  (node down)"));
                    return;
                }
                self.stats.delivered += 1;
                self.log(|| format!("{from} → {to}  {msg}"));
                let node = self.nodes[x].as_mut().expect("checked above");
                let before = (node.role(), node.term());
                let mut out = Vec::new();
                node.handle(self.now, from, msg, &mut out);
                self.after_node_step(x, before, out);
            }
            Addr::Client(c) => {
                self.stats.delivered += 1;
                self.log(|| format!("{from} → {to}  {msg}"));
                let c = c as usize;
                self.clients[c].on_msg(self.now, msg, &mut self.history);
                self.rearm(self.cfg.nodes + c);
            }
        }
    }

    /// Crash points: decide whether the node dies right after this step's
    /// durable writes, and if so whether its outgoing messages made it out.
    fn crash_point(&mut self, out: &[Output]) -> Option<bool> {
        if self.now >= self.recover_at {
            return None;
        }
        let mut p: f64 = 0.0;
        for o in out {
            match o {
                Output::Persisted(Persist::Vote) => p = p.max(self.swarm.crash_on_vote),
                Output::Persisted(Persist::Log) => p = p.max(self.swarm.crash_on_append),
                _ => {}
            }
        }
        if p > 0.0 && self.buggify.chance(p) { Some(self.buggify.chance(0.5)) } else { None }
    }

    fn after_node_step(&mut self, x: usize, before: (Role, Term), out: Vec<Output>) {
        let id = x as NodeId;
        let crash = self.crash_point(&out);
        for o in out {
            match o {
                Output::Send(..) if crash == Some(false) => {}
                Output::Send(to, msg) => self.send(Addr::Node(id), to, msg),
                Output::Persisted(_) => {}
                Output::BecameLeader(term) => {
                    self.stats.elections += 1;
                    if let Some(node) = &self.nodes[x] {
                        self.inv.on_leader(self.now, id, term, &node.durable);
                    }
                }
                Output::Committed { from, to, term } => {
                    if let Some(node) = &self.nodes[x] {
                        for i in from..=to {
                            if let Some(t) = node.durable.term_at(i) {
                                self.inv.on_commit(i, t, term);
                            }
                        }
                    }
                }
                Output::Applied { index, term, digest } => {
                    self.inv.on_apply(self.now, id, index, term, digest);
                }
            }
        }
        if let Some(node) = &self.nodes[x] {
            let after = (node.role(), node.term());
            self.stats.max_term = self.stats.max_term.max(after.1);
            self.stats.max_log = self.stats.max_log.max(node.durable.last_index());
            if after != before {
                self.timeline.record(self.now, id, lane(after.0), after.1);
                if after.0 != before.0 {
                    self.log(|| format!("n{x} becomes {:?} in term {}", after.0, after.1));
                }
            }
        }
        if let Some(sent) = crash {
            let down = self.buggify.range(1, 60);
            self.log(|| {
                let when = if sent { "after sending" } else { "before sending" };
                format!("⚡ crash point: n{x} dies right after a durable write, {when}")
            });
            self.stats.crash_points += 1;
            self.crash(x);
            self.push(self.now + ms(down), Event::Restart(x));
            self.timeline.faults.push((self.now, format!("crashpoint(n{x})")));
        } else {
            self.rearm(x);
        }
    }

    // ------------------------------------------------------------- timers

    fn rearm(&mut self, slot: usize) {
        let n = self.cfg.nodes;
        let deadline = if slot < n {
            self.nodes[slot].as_ref().map(RaftNode::next_deadline)
        } else {
            self.clients[slot - n].deadline()
        };
        let now = self.now;
        let t = &mut self.timers[slot];
        match deadline.filter(|&d| d != Time::MAX) {
            None => {
                t.epoch += 1;
                t.at = None;
            }
            Some(d) => {
                let d = d.max(now);
                // An earlier pending timer is kept: when it fires the owner
                // finds nothing to do and we re-arm for the real deadline.
                if t.at.is_none_or(|a| d < a) {
                    t.epoch += 1;
                    t.at = Some(d);
                    let epoch = t.epoch;
                    self.push(d, Event::Timer { slot, epoch });
                }
            }
        }
    }

    fn fire_timer(&mut self, slot: usize, epoch: u64) {
        if self.timers[slot].epoch != epoch {
            return;
        }
        self.timers[slot].at = None;
        let n = self.cfg.nodes;
        if slot < n {
            if let Some(node) = self.nodes[slot].as_mut() {
                let before = (node.role(), node.term());
                let mut out = Vec::new();
                node.tick(self.now, &mut out);
                self.after_node_step(slot, before, out);
            }
        } else {
            let c = slot - n;
            let mut out = Vec::new();
            self.clients[c].wake(self.now, &mut self.history, &mut out);
            for (to, msg) in out {
                self.send(Addr::Client(c as u8), to, msg);
            }
            self.rearm(slot);
        }
    }

    // ------------------------------------------------------------- faults

    fn leader(&self) -> Option<NodeId> {
        self.nodes
            .iter()
            .flatten()
            .filter(|node| node.role() == Role::Leader)
            .max_by_key(|node| node.term())
            .map(RaftNode::id)
    }

    fn resolve(&self, target: Target) -> Vec<usize> {
        match target {
            Target::Node(x) if (x as usize) < self.cfg.nodes => vec![x as usize],
            Target::Node(_) => vec![],
            Target::Leader => self.leader().map(|x| x as usize).into_iter().collect(),
            Target::All => (0..self.cfg.nodes).collect(),
        }
    }

    fn names(xs: &[usize]) -> String {
        if xs.is_empty() {
            return "nobody".to_string();
        }
        xs.iter().map(|x| format!("n{x}")).collect::<Vec<_>>().join(",")
    }

    fn apply_fault(&mut self, i: usize) {
        let fault = self.plan.events[i].fault.clone();
        let n = self.cfg.nodes;
        self.stats.faults += 1;
        let effect = match &fault {
            Fault::Partition(groups) => {
                self.net.partition(n, groups);
                String::new()
            }
            Fault::Bridge { left, right, .. } => {
                self.net.bridge(left, right);
                String::new()
            }
            Fault::Isolate(t) => {
                let xs = self.resolve(*t);
                if let Some(&x) = xs.first() {
                    self.net.partition(n, &[vec![x as NodeId]]);
                }
                Self::names(&xs)
            }
            Fault::Heal => {
                self.net.heal();
                String::new()
            }
            Fault::Crash(t) => {
                let xs = self.resolve(*t);
                xs.iter().for_each(|&x| self.crash(x));
                Self::names(&xs)
            }
            Fault::Bounce(t, down) => {
                let xs = self.resolve(*t);
                for &x in &xs {
                    self.crash(x);
                    self.push(self.now + ms(*down), Event::Restart(x));
                }
                Self::names(&xs)
            }
            Fault::Restart(t) => {
                let xs: Vec<usize> =
                    self.resolve(*t).into_iter().filter(|&x| self.nodes[x].is_none()).collect();
                xs.iter().for_each(|&x| self.restart(x));
                Self::names(&xs)
            }
            Fault::Loss(p) => {
                self.net.set_loss(f64::from(*p) / 100.0);
                String::new()
            }
        };
        let label = if effect.is_empty() { fault.to_string() } else { format!("{fault} → {effect}") };
        self.log(|| format!("⚡ {label}"));
        self.timeline.faults.push((self.now, label));
    }

    fn crash(&mut self, x: usize) {
        if let Some(node) = self.nodes[x].take() {
            let term = node.term();
            self.stash[x] = node.crash();
            self.stats.crashes += 1;
            self.timeline.record(self.now, x as NodeId, Lane::Down, term);
            self.timers[x].epoch += 1;
            self.timers[x].at = None;
            self.log(|| format!("n{x} crashes"));
        }
    }

    fn restart(&mut self, x: usize) {
        if self.nodes[x].is_some() {
            return;
        }
        self.incarnation[x] += 1;
        let durable = std::mem::take(&mut self.stash[x]);
        let rng = Rng::stream(self.cfg.seed, node_stream(x, self.incarnation[x]));
        let node = RaftNode::new(
            x as NodeId,
            self.cfg.nodes,
            self.raft.clone(),
            self.cfg.bugs,
            durable,
            rng,
            self.now,
        );
        self.timeline.record(self.now, x as NodeId, Lane::Follower, node.term());
        self.nodes[x] = Some(node);
        self.stats.restarts += 1;
        self.log(|| format!("n{x} restarts"));
        self.rearm(x);
    }

    fn recover(&mut self) {
        self.net.heal();
        self.net.set_loss(0.0);
        for x in 0..self.cfg.nodes {
            self.restart(x);
        }
        self.timeline.recovery = Some(self.now);
        self.log(|| "⚡ recovery: network healed, all nodes up".to_string());
    }

    // ------------------------------------------------------------ verdict

    fn finish(mut self) -> RunReport {
        let n = self.cfg.nodes;
        self.stats.virtual_time = self.now;
        self.timeline.end = self.now;

        let mut violations = Vec::new();
        let stuck: Vec<String> = self
            .clients
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_done())
            .map(|(i, c)| format!("c{i} ({} ops left)", c.outstanding()))
            .collect();
        if self.storm {
            violations.push(Violation {
                kind: ViolationKind::Liveness,
                at: self.now,
                message: format!(
                    "event budget of {} exhausted after only {} of cluster time — \
                     something is amplifying message traffic",
                    self.cfg.max_events,
                    time::fmt(self.now)
                ),
                ops: vec![],
                culprit: None,
            });
        } else if !stuck.is_empty() {
            violations.push(Violation {
                kind: ViolationKind::Liveness,
                at: self.now,
                message: format!(
                    "workload still unfinished {} after the network healed at {}: {}",
                    time::fmt(self.cfg.recovery_limit),
                    time::fmt(self.recover_at),
                    stuck.join(", ")
                ),
                ops: vec![],
                culprit: None,
            });
        }

        let logs: Vec<(NodeId, &Durable)> = (0..n)
            .map(|x| (x as NodeId, self.nodes[x].as_ref().map_or(&self.stash[x], |node| &node.durable)))
            .collect();
        self.inv.check_log_matching(self.now, &logs);
        violations.append(&mut self.inv.violations);

        let verdicts = check_history(&self.history, self.cfg.lin_budget);
        for kv in &verdicts {
            match &kv.verdict {
                Verdict::NotLinearizable(cx) => {
                    let culprit = &self.history.ops[cx.stuck_on];
                    violations.push(Violation {
                        kind: ViolationKind::Linearizability,
                        at: culprit.ret_time().min(self.now),
                        message: format!(
                            "key k{}: no valid order exists for its {} operations; the longest \
                             linearizable prefix has {} ops, then `{}` cannot be placed",
                            kv.key,
                            kv.ops.len(),
                            cx.longest_prefix.len(),
                            culprit
                        ),
                        ops: kv.ops.clone(),
                        culprit: Some(cx.stuck_on),
                    });
                }
                Verdict::Unknown => self.stats.lin_unknown += 1,
                Verdict::Linearizable(_) => {}
            }
        }
        violations.sort_by_key(|v| v.at);

        for op in &self.history.ops {
            self.fp.mix(op.invoke);
            self.fp.mix(op.ret_time());
            if let Some(r) = op.result() {
                self.fp.mix(match r {
                    crate::kv::KvResult::Read(v) => v.map_or(1, |v| v + 2),
                    crate::kv::KvResult::Written => 0,
                    crate::kv::KvResult::Cas(ok) => u64::from(ok) + 7,
                });
            }
        }

        RunReport {
            seed: self.cfg.seed,
            swarm: self.swarm,
            plan: self.plan,
            recover_at: self.recover_at,
            violations,
            history: self.history,
            verdicts,
            stats: self.stats,
            timeline: self.timeline,
            trace: self.trace,
            fingerprint: self.fp.finish(),
        }
    }
}
