//! The nemesis: fault schedules.
//!
//! A [`FaultPlan`] is a list of timed faults. It is generated from the seed,
//! but it is also a plain value with a compact text form, so a shrunk failing
//! schedule can be printed, pasted back on the command line and replayed:
//!
//! ```text
//! 350:crash(L),900:part(01|234),1400:heal,2000:restart(*)
//! ```
//!
//! Generation uses *swarm testing* (Groce et al., ISSTA 2012): rather than
//! every seed drawing from the same fault mix, each seed first picks a random
//! [`Swarm`] profile that switches whole fault kinds off and re-weights the
//! rest. A run that only ever bounces nodes explores very different states
//! from one that only partitions them, and the diversity finds bugs that a
//! uniform mix dilutes away.

use crate::raft::NodeId;
use crate::rng::Rng;
use crate::time::{MILLIS, Time, ms};
use std::fmt;
use std::str::FromStr;

/// Which node(s) a fault applies to. `Leader` is resolved when the fault
/// fires, to whichever node currently believes it leads the highest term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Node(NodeId),
    Leader,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Split the cluster into mutually unreachable groups.
    Partition(Vec<Vec<NodeId>>),
    /// `left` and `right` cannot talk, but `hub` reaches everyone.
    Bridge {
        hub: NodeId,
        left: Vec<NodeId>,
        right: Vec<NodeId>,
    },
    /// Cut one node off from all its peers.
    Isolate(Target),
    Heal,
    Crash(Target),
    /// Crash, then restart after `down_ms` — a process bounce.
    Bounce(Target, u64),
    Restart(Target),
    /// Extra packet loss, in percent.
    Loss(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimedFault {
    pub at_ms: u64,
    pub fault: Fault,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FaultPlan {
    pub events: Vec<TimedFault>,
}

pub const FAULT_KINDS: [&str; 9] =
    ["partition", "bridge", "isolate", "heal", "crash-leader", "crash-node", "bounce", "restart", "loss"];

/// A per-seed configuration of the nemesis and the network.
#[derive(Clone, Debug, PartialEq)]
pub struct Swarm {
    /// Relative weight of each entry in [`FAULT_KINDS`].
    pub weights: [u32; 9],
    pub min_gap: Time,
    pub max_gap: Time,
    pub drop: f64,
    pub dup: f64,
    pub spike: f64,
    /// BUGGIFY-style crash points: probability of crashing a node right
    /// after it durably records a vote / a log append.
    pub crash_on_vote: f64,
    pub crash_on_append: f64,
    /// Raft's per-message entry limit. Small batches split replication into
    /// more steps, which some interleavings need.
    pub max_batch: usize,
}

impl Default for Swarm {
    fn default() -> Self {
        Swarm {
            weights: [20, 8, 10, 17, 10, 8, 10, 12, 5],
            min_gap: ms(100),
            max_gap: ms(700),
            drop: 0.005,
            dup: 0.01,
            spike: 0.02,
            crash_on_vote: 0.0,
            crash_on_append: 0.0,
            max_batch: 4,
        }
    }
}

impl Swarm {
    pub fn random(rng: &mut Rng) -> Self {
        let mut weights = [0u32; 9];
        for w in &mut weights {
            *w = if rng.chance(0.4) { 0 } else { rng.range(1, 11) as u32 };
        }
        if weights.iter().all(|&w| w == 0) {
            weights[rng.below(9) as usize] = 1;
        }
        let base = rng.range(40, 400);
        Swarm {
            weights,
            min_gap: ms(base),
            max_gap: ms(base * rng.range(2, 6)),
            drop: *rng.pick(&[0.0, 0.002, 0.01, 0.03]),
            dup: *rng.pick(&[0.0, 0.01, 0.05]),
            spike: *rng.pick(&[0.0, 0.02, 0.08]),
            crash_on_vote: *rng.pick(&[0.0, 0.0, 0.05, 0.25]),
            crash_on_append: *rng.pick(&[0.0, 0.0, 0.002, 0.01]),
            max_batch: *rng.pick(&[1, 2, 4, 16]),
        }
    }

    pub fn describe(&self) -> String {
        let kinds: Vec<String> = FAULT_KINDS
            .iter()
            .zip(self.weights)
            .filter(|(_, w)| *w > 0)
            .map(|(k, w)| format!("{k}:{w}"))
            .collect();
        let mut s = format!(
            "faults {{{}}} every {}–{}ms · drop {:.1}% dup {:.1}% delay-spike {:.1}% · batch {}",
            kinds.join(" "),
            self.min_gap / MILLIS,
            self.max_gap / MILLIS,
            self.drop * 100.0,
            self.dup * 100.0,
            self.spike * 100.0,
            self.max_batch
        );
        if self.crash_on_vote > 0.0 || self.crash_on_append > 0.0 {
            s += &format!(
                " · crash points: vote {:.1}% append {:.1}%",
                self.crash_on_vote * 100.0,
                self.crash_on_append * 100.0
            );
        }
        s
    }
}

fn sorted(mut v: Vec<NodeId>) -> Vec<NodeId> {
    v.sort_unstable();
    v
}

impl FaultPlan {
    pub fn generate(rng: &mut Rng, nodes: usize, duration: Time, swarm: &Swarm) -> FaultPlan {
        let total: u32 = swarm.weights.iter().sum();
        let mut events = Vec::new();
        if total == 0 || nodes == 0 {
            return FaultPlan { events };
        }
        let random_node = |rng: &mut Rng| rng.below(nodes as u64) as NodeId;
        let mut t = rng.range(swarm.min_gap, swarm.max_gap + 1);
        while t < duration {
            let mut roll = rng.below(u64::from(total)) as u32;
            let mut kind = 0;
            while roll >= swarm.weights[kind] {
                roll -= swarm.weights[kind];
                kind += 1;
            }
            let mut all: Vec<NodeId> = (0..nodes as NodeId).collect();
            rng.shuffle(&mut all);
            let fault = match kind {
                0 if nodes >= 2 => {
                    let cut = rng.range(1, nodes as u64) as usize;
                    Fault::Partition(vec![sorted(all[..cut].to_vec()), sorted(all[cut..].to_vec())])
                }
                1 if nodes >= 3 => {
                    let rest = &all[1..];
                    let cut = rest.len() / 2;
                    Fault::Bridge {
                        hub: all[0],
                        left: sorted(rest[..cut].to_vec()),
                        right: sorted(rest[cut..].to_vec()),
                    }
                }
                2 if rng.chance(0.7) => Fault::Isolate(Target::Leader),
                2 => Fault::Isolate(Target::Node(random_node(rng))),
                4 => Fault::Crash(Target::Leader),
                5 => Fault::Crash(Target::Node(random_node(rng))),
                6 => {
                    let target = match rng.below(5) {
                        0 | 1 => Target::Leader,
                        2 | 3 => Target::Node(random_node(rng)),
                        _ => Target::All,
                    };
                    Fault::Bounce(target, rng.range(5, 200))
                }
                7 if rng.chance(0.6) => Fault::Restart(Target::All),
                7 => Fault::Restart(Target::Node(random_node(rng))),
                8 => Fault::Loss(*rng.pick(&[0, 5, 20, 50])),
                _ => Fault::Heal,
            };
            events.push(TimedFault { at_ms: t / MILLIS, fault });
            t += rng.range(swarm.min_gap, swarm.max_gap + 1);
        }
        FaultPlan { events }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

// ------------------------------------------------------------- text format

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Node(n) => write!(f, "{n}"),
            Target::Leader => f.write_str("L"),
            Target::All => f.write_str("*"),
        }
    }
}

fn fmt_group(g: &[NodeId]) -> String {
    g.iter().map(|n| n.to_string()).collect()
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Partition(groups) => {
                let gs: Vec<String> = groups.iter().map(|g| fmt_group(g)).collect();
                write!(f, "part({})", gs.join("|"))
            }
            Fault::Bridge { hub, left, right } => {
                write!(f, "bridge({hub}:{}|{})", fmt_group(left), fmt_group(right))
            }
            Fault::Isolate(t) => write!(f, "isolate({t})"),
            Fault::Heal => f.write_str("heal"),
            Fault::Crash(t) => write!(f, "crash({t})"),
            Fault::Bounce(t, down) => write!(f, "bounce({t},{down})"),
            Fault::Restart(t) => write!(f, "restart({t})"),
            Fault::Loss(p) => write!(f, "loss({p})"),
        }
    }
}

impl fmt::Display for TimedFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.at_ms, self.fault)
    }
}

impl fmt::Display for FaultPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.events.iter().map(ToString::to_string).collect();
        f.write_str(&parts.join(","))
    }
}

fn parse_node(s: &str) -> Result<NodeId, String> {
    s.parse::<NodeId>().map_err(|_| format!("bad node id `{s}`"))
}

fn parse_target(s: &str) -> Result<Target, String> {
    match s {
        "L" | "leader" => Ok(Target::Leader),
        "*" | "all" => Ok(Target::All),
        n => parse_node(n).map(Target::Node),
    }
}

fn parse_group(s: &str) -> Result<Vec<NodeId>, String> {
    s.chars().map(|c| parse_node(&c.to_string())).collect()
}

fn parse_groups(s: &str) -> Result<Vec<Vec<NodeId>>, String> {
    s.split('|').map(parse_group).collect()
}

impl FromStr for Fault {
    type Err = String;

    fn from_str(s: &str) -> Result<Fault, String> {
        let s = s.trim();
        let (name, args) = match s.split_once('(') {
            Some((name, rest)) => {
                let args = rest.strip_suffix(')').ok_or_else(|| format!("missing `)` in `{s}`"))?;
                (name, args)
            }
            None => (s, ""),
        };
        match name {
            "heal" => Ok(Fault::Heal),
            "part" => Ok(Fault::Partition(parse_groups(args)?)),
            "bridge" => {
                let (hub, rest) = args.split_once(':').ok_or("bridge needs `hub:left|right`")?;
                let (left, right) = rest.split_once('|').ok_or("bridge needs `hub:left|right`")?;
                Ok(Fault::Bridge {
                    hub: parse_node(hub)?,
                    left: parse_group(left)?,
                    right: parse_group(right)?,
                })
            }
            "isolate" => Ok(Fault::Isolate(parse_target(args)?)),
            "crash" => Ok(Fault::Crash(parse_target(args)?)),
            "restart" => Ok(Fault::Restart(parse_target(args)?)),
            "bounce" => {
                let (t, down) = args.split_once(',').unwrap_or((args, "50"));
                let down = down.parse().map_err(|_| format!("bad bounce duration `{down}`"))?;
                Ok(Fault::Bounce(parse_target(t)?, down))
            }
            "loss" => {
                let p: u8 = args.parse().map_err(|_| format!("bad loss percentage `{args}`"))?;
                Ok(Fault::Loss(p.min(100)))
            }
            other => Err(format!("unknown fault `{other}`")),
        }
    }
}

impl FromStr for FaultPlan {
    type Err = String;

    fn from_str(s: &str) -> Result<FaultPlan, String> {
        let mut events = Vec::new();
        for part in s.split([',', ';']).map(str::trim).filter(|p| !p.is_empty()) {
            // Bounce uses a comma inside its parentheses; re-join it.
            if let Some(prev) = events.last_mut().filter(|_: &&mut String| !part.contains(':')) {
                prev.push(',');
                prev.push_str(part);
                continue;
            }
            events.push(part.to_string());
        }
        let events = events
            .iter()
            .map(|e| {
                let (at, fault) =
                    e.split_once(':').ok_or_else(|| format!("expected `ms:fault`, got `{e}`"))?;
                let at_ms = at.trim().parse().map_err(|_| format!("bad time `{at}`"))?;
                Ok(TimedFault { at_ms, fault: fault.parse()? })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut plan = FaultPlan { events };
        plan.events.sort_by_key(|e| e.at_ms);
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let text = "10:part(01|234),20:bridge(2:01|34),30:isolate(L),40:heal,50:crash(3),\
                    60:bounce(*,25),70:restart(*),80:loss(20)";
        let plan: FaultPlan = text.parse().unwrap();
        assert_eq!(plan.len(), 8);
        assert_eq!(plan.to_string(), text);
        assert_eq!(plan.events[5].fault, Fault::Bounce(Target::All, 25));
    }

    #[test]
    fn generated_plans_round_trip_and_are_deterministic() {
        for seed in 0..50 {
            let mut rng = Rng::new(seed);
            let swarm = Swarm::random(&mut rng);
            let plan = FaultPlan::generate(&mut rng, 5, ms(8000), &swarm);
            let reparsed: FaultPlan = plan.to_string().parse().unwrap();
            assert_eq!(plan, reparsed, "seed {seed}");
            let mut rng2 = Rng::new(seed);
            let swarm2 = Swarm::random(&mut rng2);
            assert_eq!(plan, FaultPlan::generate(&mut rng2, 5, ms(8000), &swarm2));
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!("10:explode(3)".parse::<FaultPlan>().is_err());
        assert!("crash(3)".parse::<FaultPlan>().is_err());
        assert!("10:crash(x)".parse::<FaultPlan>().is_err());
    }
}
