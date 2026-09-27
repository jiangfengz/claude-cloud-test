//! # hourglass
//!
//! Deterministic simulation testing (DST) for a Raft-replicated key-value
//! store, in the spirit of FoundationDB's simulator and TigerBeetle's VOPR.
//!
//! * [`raft`] — a sans-IO Raft node: a pure state machine with no clocks,
//!   sockets or threads, and switchable [`bugs`].
//! * [`sim`] — a discrete-event simulator that runs a whole cluster, its
//!   clients, a lossy/reordering [`net`]work and a [`nemesis`] inside one
//!   thread, in virtual time, reproducibly from a single seed.
//! * [`checker`] — Raft's safety invariants checked online, plus a
//!   Wing–Gong–Lowe linearizability checker over the client history.
//! * [`shrink`] — delta debugging of failing runs down to a minimal
//!   reproduction; [`fuzz`] — parallel seed exploration.
//!
//! ```
//! use hourglass::sim::{run, SimConfig};
//!
//! let report = run(&SimConfig { seed: 7, ops_per_client: 20, ..SimConfig::default() });
//! assert!(!report.failed());
//! // Same seed, same universe.
//! assert_eq!(report.fingerprint, run(&SimConfig { seed: 7, ops_per_client: 20, ..SimConfig::default() }).fingerprint);
//! ```

pub mod bugs;
pub mod checker;
pub mod client;
pub mod fuzz;
pub mod history;
pub mod html;
pub mod kv;
pub mod nemesis;
pub mod net;
pub mod raft;
pub mod rng;
pub mod shrink;
pub mod sim;
pub mod time;
pub mod viz;
