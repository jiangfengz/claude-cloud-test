//! Terminal visualisations: a cluster timeline and a per-key operation chart
//! for linearizability counterexamples.

use crate::history::{HistOp, History};
use crate::raft::{NodeId, Term};
use crate::time::{self, SECS, Time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    Follower,
    Candidate,
    Leader,
    Down,
}

impl Lane {
    /// Priority when several states fall into one column.
    fn weight(self) -> u8 {
        match self {
            Lane::Follower => 0,
            Lane::Candidate => 1,
            Lane::Down => 2,
            Lane::Leader => 3,
        }
    }
}

/// Role changes of every node over the run, plus the faults that caused them.
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    pub nodes: usize,
    pub changes: Vec<(Time, NodeId, Lane, Term)>,
    pub faults: Vec<(Time, String)>,
    pub recovery: Option<Time>,
    pub end: Time,
}

/// A maximal interval during which a node was in one state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub from: Time,
    pub to: Time,
    pub lane: Lane,
    pub term: Term,
}

impl Timeline {
    pub fn new(nodes: usize) -> Self {
        Timeline { nodes, ..Default::default() }
    }

    pub fn record(&mut self, at: Time, node: NodeId, lane: Lane, term: Term) {
        self.changes.push((at, node, lane, term));
    }

    pub fn spans(&self, node: NodeId) -> Vec<Span> {
        let mut spans: Vec<Span> = Vec::new();
        for &(at, _, lane, term) in self.changes.iter().filter(|c| c.1 == node) {
            if let Some(last) = spans.last_mut() {
                if last.lane == lane && last.term == term {
                    continue;
                }
                last.to = at;
            }
            spans.push(Span { from: at, to: self.end, lane, term });
        }
        spans.retain(|s| s.to > s.from || s.from == self.end);
        spans
    }

    /// Distinct leaders over the run, as `(node, term, from, to)`.
    pub fn leaderships(&self) -> Vec<(NodeId, Term, Time, Time)> {
        let mut out: Vec<_> = (0..self.nodes as NodeId)
            .flat_map(|n| {
                self.spans(n)
                    .into_iter()
                    .filter(|s| s.lane == Lane::Leader)
                    .map(move |s| (n, s.term, s.from, s.to))
            })
            .collect();
        out.sort_by_key(|l| (l.2, l.0));
        out
    }
}

/// ANSI styling that can be switched off for pipes and tests.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub color: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

fn fault_glyph(label: &str) -> char {
    match label.split('(').next().unwrap_or("") {
        "part" => 'P',
        "bridge" => 'B',
        "isolate" => 'I',
        "heal" => 'H',
        "crash" => 'K',
        "bounce" => 'b',
        "restart" => 'R',
        "loss" => '%',
        "crashpoint" => '!',
        _ => '?',
    }
}

/// Renders one row per node plus a fault row, e.g.
///
/// ```text
///         0s        1s        2s
/// n0  ····LLLLLLLL×××××·········
/// n1  ·········c··LLLLLLLLLLLLLL
/// ⚡       K    R
/// ```
pub fn render_timeline(tl: &Timeline, width: usize, style: Style) -> String {
    let end = tl.end.max(1);
    let width = width.max(10);
    let col_of = |t: Time| ((t as u128 * width as u128 / end as u128) as usize).min(width - 1);
    let mut out = String::new();

    // Axis with a tick per second (or coarser for long runs).
    let secs = end.div_ceil(SECS);
    let step = (secs as usize * 10).div_ceil(width).max(1) as u64;
    let mut axis = vec![' '; width + 8];
    let mut s = 0;
    while s * SECS <= end {
        let label = format!("{s}s");
        let c = col_of(s * SECS);
        for (i, ch) in label.chars().enumerate() {
            if c + i < axis.len() {
                axis[c + i] = ch;
            }
        }
        s += step;
    }
    out.push_str(&format!("      {}\n", style.dim(axis.iter().collect::<String>().trim_end())));

    for node in 0..tl.nodes as NodeId {
        let mut cells = vec![Lane::Follower; width];
        let mut weight = vec![0u8; width];
        for span in tl.spans(node) {
            let (a, b) = (col_of(span.from), col_of(span.to.max(span.from + 1) - 1));
            for c in a..=b {
                if span.lane.weight() >= weight[c] {
                    weight[c] = span.lane.weight();
                    cells[c] = span.lane;
                }
            }
        }
        let row: String = cells
            .iter()
            .map(|lane| match lane {
                Lane::Leader => style.green("L"),
                Lane::Candidate => style.yellow("c"),
                Lane::Down => style.red("×"),
                Lane::Follower => style.dim("·"),
            })
            .collect();
        out.push_str(&format!("  n{node}  {row}\n"));
    }

    let mut faults = vec![' '; width];
    for (t, label) in &tl.faults {
        let c = col_of(*t);
        faults[c] = if faults[c] == ' ' { fault_glyph(label) } else { '*' };
    }
    if let Some(r) = tl.recovery {
        faults[col_of(r)] = '|';
    }
    out.push_str(&format!("  ⚡  {}\n", style.cyan(faults.iter().collect::<String>().trim_end())));
    out.push_str(&style.dim(
        "      L leader · c candidate · × down · follower   \
         P part B bridge I isolate H heal K crash b bounce R restart % loss ! crash point | recovery\n",
    ));
    out
}

/// A Gantt-style chart of the operations around a linearizability failure.
pub fn render_ops(history: &History, ids: &[usize], culprit: Option<usize>, style: Style) -> String {
    const WIDTH: usize = 44;
    const MAX_ROWS: usize = 18;
    let mut ops: Vec<&HistOp> = ids.iter().map(|&i| &history.ops[i]).collect();
    ops.sort_by_key(|o| (o.invoke, o.id));

    // Window: the culprit and the operations just before and around it.
    if let Some(c) = culprit
        && let Some(pos) = ops.iter().position(|o| o.id == c)
    {
        let c_op = ops[pos];
        let mut window: Vec<&HistOp> = ops
            .iter()
            .copied()
            .filter(|o| o.invoke <= c_op.ret_time() && o.ret_time() >= c_op.invoke)
            .collect();
        let before: Vec<&HistOp> = ops[..pos]
            .iter()
            .rev()
            .filter(|o| o.ret_time() < c_op.invoke && o.complete.is_some())
            .take(6)
            .copied()
            .collect();
        window.extend(before);
        window.sort_by_key(|o| (o.invoke, o.id));
        window.dedup_by_key(|o| o.id);
        if window.len() > MAX_ROWS {
            let keep_from = window.len() - MAX_ROWS;
            window.drain(..keep_from);
        }
        ops = window;
    }
    if ops.is_empty() {
        return String::new();
    }
    let t0 = ops.iter().map(|o| o.invoke).min().unwrap_or(0);
    let t1 = ops.iter().map(|o| o.complete.map_or(o.invoke, |(t, _)| t)).max().unwrap_or(t0).max(t0 + 1);
    let col = |t: Time| {
        (((t.min(t1) - t0) as u128 * (WIDTH - 1) as u128 / (t1 - t0) as u128) as usize).min(WIDTH - 1)
    };

    let mut out = String::new();
    out.push_str(&style.dim(&format!(
        "        {:<34} {:<44} {}\n",
        "operation",
        format!("{} … {}", time::fmt(t0), time::fmt(t1)),
        "interval"
    )));
    for o in ops {
        let a = col(o.invoke);
        let b = o.complete.map(|(t, _)| col(t));
        let mut bar = vec![' '; WIDTH];
        match b {
            Some(b) => {
                for c in bar.iter_mut().take(b + 1).skip(a) {
                    *c = '═';
                }
                bar[a] = '╞';
                bar[b] = '╡';
            }
            None => {
                for c in bar.iter_mut().skip(a) {
                    *c = '┄';
                }
                bar[a] = '╞';
                bar[WIDTH - 1] = '›';
            }
        }
        let bar: String = bar.into_iter().collect();
        let result = match o.complete {
            Some((_, r)) if o.op.is_read() => format!("→ {r}"),
            Some((_, r)) => r.to_string(),
            None => "?".to_string(),
        };
        let label = format!("#{:<4} c{} {} {}", o.id, o.client, o.op, result);
        let span = format!("{} … {}", time::fmt(o.invoke), time::fmt(o.ret_time()));
        let is_culprit = Some(o.id) == culprit;
        let marker = if is_culprit { style.red("▶") } else { " ".to_string() };
        let (label, bar) = if is_culprit {
            (style.red(&format!("{label:<34}")), style.red(&bar))
        } else if o.complete.is_none() {
            (style.dim(&format!("{label:<34}")), style.dim(&bar))
        } else {
            (format!("{label:<34}"), style.cyan(&bar))
        };
        out.push_str(&format!("     {marker}  {label} {bar} {}\n", style.dim(&span)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_merge_and_close() {
        let mut tl = Timeline::new(1);
        tl.record(0, 0, Lane::Follower, 0);
        tl.record(10, 0, Lane::Candidate, 1);
        tl.record(12, 0, Lane::Leader, 1);
        tl.record(30, 0, Lane::Down, 1);
        tl.end = 40;
        let spans = tl.spans(0);
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[2], Span { from: 12, to: 30, lane: Lane::Leader, term: 1 });
        assert_eq!(tl.leaderships(), vec![(0, 1, 12, 30)]);
    }

    #[test]
    fn timeline_renders_every_node() {
        let mut tl = Timeline::new(3);
        for n in 0..3 {
            tl.record(0, n, Lane::Follower, 0);
        }
        tl.record(SECS / 2, 1, Lane::Leader, 1);
        tl.faults.push((SECS, "crash(L) → n1".into()));
        tl.record(SECS, 1, Lane::Down, 1);
        tl.end = 2 * SECS;
        let s = render_timeline(&tl, 40, Style { color: false });
        assert!(s.contains("n0") && s.contains("n2"));
        assert!(s.contains('L') && s.contains('×') && s.contains('K'));
    }
}
