//! A generic linearizability checker.
//!
//! This is the Wing & Gong search with Lowe's memoisation ("Testing for
//! linearizability", 2017), the algorithm behind Porcupine and Knossos:
//!
//! * Every operation contributes a *call* and a *return* entry to a single
//!   list sorted by time.
//! * Walking the list, we try to linearize each call we meet: if the model
//!   accepts it from the current state, we remove the operation's call and
//!   return from the list (dancing-links style, so it can be put back in
//!   O(1)) and start again from the head.
//! * Meeting a *return* means the operation it belongs to had to take effect
//!   by now but could not, so we backtrack.
//! * The pair (set of linearized operations, model state) fully determines
//!   the rest of the search, so each pair is explored at most once. This
//!   cache is what turns an exponential search into a practical one.
//!
//! Operations that never returned get a return time of +∞: they may take
//! effect at any point after their call, or not at all.

use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;

pub trait Model {
    type State: Clone + Eq + Hash + Debug;
    type Input: Debug;
    type Output: Debug;

    fn init(&self) -> Self::State;

    /// The state after applying `input`, if that is consistent with
    /// `output` (`None` = output unknown). `None` means "impossible".
    fn step(
        &self,
        state: &Self::State,
        input: &Self::Input,
        output: Option<&Self::Output>,
    ) -> Option<Self::State>;
}

#[derive(Clone, Debug)]
pub struct Operation<I, O> {
    pub call: u64,
    /// `None` if the operation never returned.
    pub ret: Option<u64>,
    pub input: I,
    pub output: Option<O>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A valid order, as indices into the input slice.
    Linearizable(Vec<usize>),
    NotLinearizable(Counterexample),
    /// The search budget ran out before an answer was found.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counterexample {
    /// The longest prefix of the history the search managed to linearize.
    pub longest_prefix: Vec<usize>,
    /// The operation whose return could not be honoured at that frontier.
    pub stuck_on: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BitSet(Vec<u64>);

impl BitSet {
    fn new(n: usize) -> Self {
        BitSet(vec![0; n.div_ceil(64)])
    }
    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }
    fn clear(&mut self, i: usize) {
        self.0[i / 64] &= !(1 << (i % 64));
    }
}

/// Intrusive doubly-linked list over call/return entries. Node 0 is the head
/// sentinel, node `2n + 1` the tail; operation `i` owns nodes `2i + 1` (call)
/// and `2i + 2` (return).
struct Entries {
    prev: Vec<usize>,
    next: Vec<usize>,
}

impl Entries {
    fn call(op: usize) -> usize {
        2 * op + 1
    }
    fn ret(op: usize) -> usize {
        2 * op + 2
    }
    fn is_call(node: usize) -> bool {
        node % 2 == 1
    }
    fn op(node: usize) -> usize {
        (node - 1) / 2
    }

    fn unlink(&mut self, x: usize) {
        let (p, n) = (self.prev[x], self.next[x]);
        self.next[p] = n;
        self.prev[n] = p;
    }

    fn relink(&mut self, x: usize) {
        let (p, n) = (self.prev[x], self.next[x]);
        self.next[p] = x;
        self.prev[n] = x;
    }

    /// Removes an operation. Must be undone in LIFO order by [`Self::unlift`].
    fn lift(&mut self, op: usize) {
        self.unlink(Self::call(op));
        self.unlink(Self::ret(op));
    }

    fn unlift(&mut self, op: usize) {
        self.relink(Self::ret(op));
        self.relink(Self::call(op));
    }
}

pub fn check<M: Model>(model: &M, ops: &[Operation<M::Input, M::Output>], budget: u64) -> Verdict {
    let n = ops.len();
    let head = 0;
    let tail = 2 * n + 1;

    // Sort entries by time. On ties, calls go first: treating touching
    // intervals as concurrent can only make us more lenient, never report a
    // spurious violation.
    let mut order: Vec<usize> = (1..=2 * n).collect();
    let time_of = |node: usize| {
        let op = &ops[Entries::op(node)];
        if Entries::is_call(node) { op.call } else { op.ret.unwrap_or(u64::MAX) }
    };
    order.sort_by_key(|&node| (time_of(node), !Entries::is_call(node), node));

    let mut list = Entries { prev: vec![0; 2 * n + 2], next: vec![0; 2 * n + 2] };
    let mut last = head;
    for &node in &order {
        list.next[last] = node;
        list.prev[node] = last;
        last = node;
    }
    list.next[last] = tail;
    list.prev[tail] = last;

    let mut state = model.init();
    let mut linearized = BitSet::new(n);
    let mut cache: HashSet<(BitSet, M::State)> = HashSet::new();
    let mut stack: Vec<(usize, M::State)> = Vec::new();
    let mut best: Vec<usize> = Vec::new();
    let mut stuck_on = None;
    let mut steps = 0u64;
    let mut entry = list.next[head];

    while list.next[head] != tail {
        steps += 1;
        if steps > budget {
            return Verdict::Unknown;
        }
        if Entries::is_call(entry) {
            let i = Entries::op(entry);
            if let Some(next_state) = model.step(&state, &ops[i].input, ops[i].output.as_ref()) {
                let mut next_lin = linearized.clone();
                next_lin.set(i);
                if cache.insert((next_lin.clone(), next_state.clone())) {
                    stack.push((i, std::mem::replace(&mut state, next_state)));
                    linearized = next_lin;
                    list.lift(i);
                    if stack.len() > best.len() {
                        best = stack.iter().map(|(op, _)| *op).collect();
                        stuck_on = None;
                    }
                    entry = list.next[head];
                    continue;
                }
            }
            entry = list.next[entry];
        } else {
            if stuck_on.is_none() && stack.len() == best.len() {
                stuck_on = Some(Entries::op(entry));
            }
            let Some((i, prev_state)) = stack.pop() else {
                return Verdict::NotLinearizable(Counterexample {
                    longest_prefix: best,
                    stuck_on: stuck_on.unwrap_or(Entries::op(entry)),
                });
            };
            state = prev_state;
            linearized.clear(i);
            list.unlift(i);
            entry = list.next[Entries::call(i)];
        }
    }
    Verdict::Linearizable(stack.into_iter().map(|(op, _)| op).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register holding an integer, initially 0.
    struct Reg;

    #[derive(Debug)]
    enum In {
        R,
        W(u32),
    }

    impl Model for Reg {
        type State = u32;
        type Input = In;
        type Output = u32;
        fn init(&self) -> u32 {
            0
        }
        fn step(&self, s: &u32, i: &In, o: Option<&u32>) -> Option<u32> {
            match i {
                In::W(v) => Some(*v),
                In::R => match o {
                    Some(v) if v != s => None,
                    _ => Some(*s),
                },
            }
        }
    }

    fn op(call: u64, ret: u64, input: In, output: Option<u32>) -> Operation<In, u32> {
        Operation { call, ret: Some(ret), input, output }
    }

    #[test]
    fn concurrent_read_may_see_either_value() {
        // |--- w(1) ---|
        //    |-- r:0 --|        ← linearizes before the write
        //    |-- r:1 --|        ← linearizes after it
        let h = [op(0, 10, In::W(1), None), op(2, 8, In::R, Some(0)), op(2, 9, In::R, Some(1))];
        let Verdict::Linearizable(order) = check(&Reg, &h, 1_000) else { panic!() };
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn read_after_completed_write_must_see_it() {
        // |- w(1) -|  |- r:0 -|   ← the read starts after the write finished
        let h = [op(0, 5, In::W(1), None), op(6, 9, In::R, Some(0))];
        let Verdict::NotLinearizable(cx) = check(&Reg, &h, 1_000) else { panic!() };
        assert_eq!(cx.stuck_on, 1);
        assert_eq!(cx.longest_prefix, vec![0]);
    }

    #[test]
    fn values_cannot_flip_flop() {
        // Two sequential reads by concurrent observers that disagree on the
        // order of two writes.
        let h = [
            op(0, 100, In::W(1), None),
            op(0, 100, In::W(2), None),
            op(10, 20, In::R, Some(1)),
            op(30, 40, In::R, Some(2)),
            op(50, 60, In::R, Some(1)),
        ];
        assert!(matches!(check(&Reg, &h, 10_000), Verdict::NotLinearizable(_)));
    }

    #[test]
    fn pending_write_may_take_effect_late() {
        // A write that never returned is observed long after it was issued.
        let h = [
            Operation { call: 0, ret: None, input: In::W(7), output: None },
            op(10, 20, In::R, Some(0)),
            op(1000, 1010, In::R, Some(7)),
        ];
        assert!(matches!(check(&Reg, &h, 1_000), Verdict::Linearizable(_)));
    }

    #[test]
    fn budget_is_respected() {
        let h: Vec<_> =
            (0..40).map(|i| op(0, 1000, In::W(i), None)).chain([op(2000, 2001, In::R, Some(99))]).collect();
        assert_eq!(check(&Reg, &h, 500), Verdict::Unknown);
    }
}
