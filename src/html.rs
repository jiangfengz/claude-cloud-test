//! Self-contained interactive HTML reports.
//!
//! The run is serialised to JSON and embedded in a single HTML file together
//! with a small script that draws the cluster timeline and a Porcupine-style
//! chart of every key's operations. No network access, no dependencies: the
//! file can be attached to a bug report as-is.

use crate::checker::wgl::Verdict;
use crate::sim::{RunReport, SimConfig};
use crate::viz::Lane;
use std::collections::BTreeSet;
use std::fmt::Write;

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn lane_name(l: Lane) -> &'static str {
    match l {
        Lane::Follower => "follower",
        Lane::Candidate => "candidate",
        Lane::Leader => "leader",
        Lane::Down => "down",
    }
}

/// The run as a JSON document consumed by the page's script.
pub fn to_json(cfg: &SimConfig, report: &RunReport, repro: &str) -> String {
    let mut j = String::new();
    let st = &report.stats;
    let _ = write!(
        j,
        "{{\"seed\":{},\"nodes\":{},\"clients\":{},\"keys\":{},\"ops\":{},\"bugs\":{},\"end\":{},\"recover\":{},",
        cfg.seed,
        cfg.nodes,
        cfg.clients,
        cfg.keys,
        cfg.ops_per_client,
        json_str(&cfg.bugs.to_string()),
        report.timeline.end.max(1),
        report.timeline.recovery.map_or("null".to_string(), |t| t.to_string()),
    );
    let _ = write!(
        j,
        "\"plan\":{},\"swarm\":{},\"repro\":{},\"fingerprint\":\"{:016x}\",",
        json_str(&report.plan.to_string()),
        json_str(&if cfg.swarm { report.swarm.describe() } else { "off".into() }),
        json_str(repro),
        report.fingerprint
    );
    let _ = write!(
        j,
        "\"stats\":{{\"events\":{},\"sent\":{},\"delivered\":{},\"dropped\":{},\"duplicated\":{},\
         \"elections\":{},\"maxTerm\":{},\"crashes\":{},\"crashPoints\":{},\"restarts\":{},\"maxLog\":{},\
         \"completed\":{},\"indeterminate\":{}}},",
        st.events,
        st.sent,
        st.delivered,
        st.dropped,
        st.duplicated,
        st.elections,
        st.max_term,
        st.crashes,
        st.crash_points,
        st.restarts,
        st.max_log,
        report.history.completed(),
        report.history.indeterminate()
    );

    j.push_str("\"spans\":[");
    let mut first = true;
    for n in 0..cfg.nodes as u8 {
        for s in report.timeline.spans(n) {
            if !first {
                j.push(',');
            }
            first = false;
            let _ = write!(j, "[{n},{},{},\"{}\",{}]", s.from, s.to, lane_name(s.lane), s.term);
        }
    }
    j.push_str("],\"faults\":[");
    for (i, (t, label)) in report.timeline.faults.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        let _ = write!(j, "[{t},{}]", json_str(label));
    }

    j.push_str("],\"verdicts\":[");
    for (i, kv) in report.verdicts.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        let (status, order, culprit) = match &kv.verdict {
            Verdict::Linearizable(order) => ("ok", order.clone(), None),
            Verdict::NotLinearizable(cx) => ("fail", cx.longest_prefix.clone(), Some(cx.stuck_on)),
            Verdict::Unknown => ("unknown", vec![], None),
        };
        let order: Vec<String> = order.iter().map(ToString::to_string).collect();
        let _ = write!(
            j,
            "{{\"key\":{},\"status\":\"{status}\",\"order\":[{}],\"culprit\":{}}}",
            kv.key,
            order.join(","),
            culprit.map_or("null".to_string(), |c| c.to_string())
        );
    }

    j.push_str("],\"history\":[");
    for (i, o) in report.history.ops.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        let kind = match o.op.kind {
            crate::kv::OpKind::Read => "r",
            crate::kv::OpKind::Write(_) => "w",
            crate::kv::OpKind::Cas { .. } => "cas",
        };
        let result = match o.complete {
            Some((_, r)) if o.op.is_read() => format!("→ {r}"),
            Some((_, r)) => r.to_string(),
            None => "?".to_string(),
        };
        let _ = write!(
            j,
            "[{},{},{},\"{kind}\",{},{},{},{}]",
            o.id,
            o.client,
            o.key(),
            json_str(&o.op.to_string()),
            json_str(&result),
            o.invoke,
            o.complete.map_or("null".to_string(), |(t, _)| t.to_string())
        );
    }

    j.push_str("],\"violations\":[");
    for (i, v) in report.violations.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        let _ = write!(
            j,
            "{{\"kind\":{},\"at\":{},\"message\":{},\"culprit\":{}}}",
            json_str(v.kind.name()),
            v.at,
            json_str(&v.message),
            v.culprit.map_or("null".to_string(), |c| c.to_string())
        );
    }
    j.push_str("]}");
    j
}

/// Renders a complete, standalone HTML page for the run.
pub fn render(cfg: &SimConfig, report: &RunReport) -> String {
    let data = to_json(cfg, report, &cfg.repro_command());
    let kinds: BTreeSet<_> = report.kinds();
    let title = if kinds.is_empty() {
        format!("hourglass · seed {} · pass", cfg.seed)
    } else {
        format!(
            "hourglass · seed {} · {}",
            cfg.seed,
            kinds.iter().map(|k| k.name()).collect::<Vec<_>>().join(", ")
        )
    };
    TEMPLATE.replace("__TITLE__", &title).replace("__DATA__", &data)
}

const TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
<style>
:root {
  --bg: #fbfaf7; --panel: #ffffff; --ink: #1d1f23; --muted: #6b7078; --line: #e3e1db;
  --leader: #2f9e5b; --candidate: #d99a1c; --follower: #e9e7e1; --down: #d2453c;
  --read: #3b7dd8; --write: #2f9e5b; --cas: #8a5cd1; --bad: #d2453c; --fault: #9a6b00;
  --mono: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #141518; --panel: #1b1d21; --ink: #e8e6e1; --muted: #9a9ea6; --line: #2c2f35;
    --leader: #46c07a; --candidate: #e8b04a; --follower: #2a2d33; --down: #ef6a61;
    --read: #6aa2f0; --write: #46c07a; --cas: #ad8bec; --bad: #ef6a61; --fault: #e0b24f;
  }
}
:root[data-theme="dark"] {
  --bg: #141518; --panel: #1b1d21; --ink: #e8e6e1; --muted: #9a9ea6; --line: #2c2f35;
  --leader: #46c07a; --candidate: #e8b04a; --follower: #2a2d33; --down: #ef6a61;
  --read: #6aa2f0; --write: #46c07a; --cas: #ad8bec; --bad: #ef6a61; --fault: #e0b24f;
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink);
  font: 14px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 1280px; margin: 0 auto; padding: 24px 16px 64px; }
h1 { font-size: 20px; margin: 0 0 4px; font-weight: 650; letter-spacing: -0.01em; }
h2 { font-size: 13px; text-transform: uppercase; letter-spacing: .08em; color: var(--muted);
  margin: 32px 0 10px; font-weight: 600; }
.sub { color: var(--muted); }
.badge { display: inline-block; padding: 2px 10px; border-radius: 999px; font-weight: 600;
  font-size: 12px; margin-left: 8px; vertical-align: 2px; }
.pass { background: color-mix(in srgb, var(--leader) 18%, transparent); color: var(--leader); }
.fail { background: color-mix(in srgb, var(--bad) 18%, transparent); color: var(--bad); }
.panel { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 14px 16px; }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 10px; }
.stat b { display: block; font-size: 18px; font-variant-numeric: tabular-nums; }
.stat span { color: var(--muted); font-size: 12px; }
code, .mono { font-family: var(--mono); font-size: 12.5px; }
.repro { display: flex; gap: 8px; align-items: center; margin-top: 12px; }
.repro code { flex: 1; overflow-x: auto; white-space: nowrap; padding: 8px 10px; border-radius: 8px;
  background: var(--bg); border: 1px solid var(--line); }
button { font: inherit; font-size: 12px; padding: 6px 10px; border-radius: 7px; cursor: pointer;
  border: 1px solid var(--line); background: var(--panel); color: var(--ink); }
button:hover { border-color: var(--muted); }
.violation { border-left: 3px solid var(--bad); padding: 8px 12px; margin: 8px 0;
  background: color-mix(in srgb, var(--bad) 7%, transparent); border-radius: 0 8px 8px 0; }
.violation b { color: var(--bad); }
.toolbar { display: flex; gap: 14px; align-items: center; flex-wrap: wrap; margin-bottom: 8px; color: var(--muted); font-size: 12px; }
.toolbar input[type=range] { width: 160px; }
.scroll { overflow-x: auto; border: 1px solid var(--line); border-radius: 10px; background: var(--panel); }
svg text { font-family: var(--mono); font-size: 11px; fill: var(--ink); }
svg .muted { fill: var(--muted); }
.legend { display: flex; flex-wrap: wrap; gap: 14px; font-size: 12px; color: var(--muted); margin-top: 8px; }
.sw { display: inline-block; width: 12px; height: 12px; border-radius: 3px; vertical-align: -2px; margin-right: 5px; }
#tip { position: fixed; pointer-events: none; background: var(--ink); color: var(--bg); padding: 6px 9px;
  border-radius: 6px; font: 12px/1.4 var(--mono); white-space: pre; opacity: 0; transition: opacity .08s; z-index: 9; }
.keyhead { display: flex; align-items: baseline; gap: 10px; margin: 18px 0 6px; }
.keyhead b { font-family: var(--mono); }
details summary { cursor: pointer; color: var(--muted); }
</style>
</head>
<body>
<main>
  <h1 id="title"></h1>
  <div class="sub" id="subtitle"></div>
  <div class="repro"><code id="repro"></code><button id="copy">Copy</button></div>

  <div id="violations"></div>

  <h2>Run</h2>
  <div class="panel grid" id="stats"></div>

  <h2>Cluster timeline</h2>
  <div class="toolbar">
    <label>zoom <input type="range" id="zoom" min="1" max="40" step="1" value="1"></label>
    <span id="zoomval">1×</span>
    <span>hover for details · faults are marked on the top rail</span>
  </div>
  <div class="scroll" id="tlwrap"></div>
  <div class="legend">
    <span><i class="sw" style="background:var(--leader)"></i>leader (term shown)</span>
    <span><i class="sw" style="background:var(--candidate)"></i>candidate</span>
    <span><i class="sw" style="background:var(--follower)"></i>follower</span>
    <span><i class="sw" style="background:var(--down)"></i>down</span>
    <span><i class="sw" style="background:var(--fault)"></i>fault</span>
  </div>

  <h2>Client operations by key</h2>
  <div class="legend" style="margin:0 0 6px">
    <span><i class="sw" style="background:var(--read)"></i>read</span>
    <span><i class="sw" style="background:var(--write)"></i>write</span>
    <span><i class="sw" style="background:var(--cas)"></i>compare-and-set</span>
    <span><i class="sw" style="background:var(--bad)"></i>could not be linearized</span>
    <span>dashed = outcome unknown (client gave up) · faded = outside the longest linearizable prefix</span>
  </div>
  <div id="keys"></div>

  <h2>Fault plan</h2>
  <div class="panel"><code id="plan" style="word-break:break-all"></code>
    <div class="sub" style="margin-top:6px" id="swarm"></div></div>
</main>
<div id="tip"></div>
<script id="data" type="application/json">__DATA__</script>
<script>
const D = JSON.parse(document.getElementById('data').textContent);
const $ = (id) => document.getElementById(id);
const fmt = (us) => (us / 1e6).toFixed(3) + 's';
const num = (n) => n.toLocaleString('en-US');
const NS = 'http://www.w3.org/2000/svg';
const el = (tag, attrs, parent) => {
  const e = document.createElementNS(NS, tag);
  for (const k in attrs) e.setAttribute(k, attrs[k]);
  if (parent) parent.appendChild(e);
  return e;
};
const tip = $('tip');
const hover = (node, text) => {
  node.addEventListener('mousemove', (ev) => {
    tip.textContent = text; tip.style.opacity = 1;
    const x = Math.min(ev.clientX + 14, window.innerWidth - tip.offsetWidth - 8);
    tip.style.left = x + 'px'; tip.style.top = (ev.clientY + 14) + 'px';
  });
  node.addEventListener('mouseleave', () => { tip.style.opacity = 0; });
};

// ---- header
const failed = D.violations.length > 0;
$('title').innerHTML = `hourglass · seed ${D.seed}<span class="badge ${failed ? 'fail' : 'pass'}">${failed ? 'FAIL' : 'PASS'}</span>`;
$('subtitle').textContent = `${D.nodes} nodes · ${D.clients} clients × ${D.ops} ops · ${D.keys} keys · bugs: ${D.bugs} · fingerprint ${D.fingerprint}`;
$('repro').textContent = D.repro;
$('copy').onclick = () => navigator.clipboard && navigator.clipboard.writeText(D.repro).then(() => { $('copy').textContent = 'Copied'; });
$('plan').textContent = D.plan || '(no faults)';
$('swarm').textContent = 'swarm profile: ' + D.swarm;

for (const v of D.violations) {
  const div = document.createElement('div');
  div.className = 'violation';
  div.innerHTML = `<b></b> <span class="sub"></span><div></div>`;
  div.querySelector('b').textContent = v.kind;
  div.querySelector('.sub').textContent = 'at ' + fmt(v.at);
  div.querySelector('div').textContent = v.message;
  $('violations').appendChild(div);
}

const S = D.stats;
const stats = [
  [fmt(D.end), 'virtual time'], [num(S.events), 'events'], [num(S.sent), 'messages sent'],
  [num(S.dropped), 'dropped'], [num(S.duplicated), 'duplicated'], [S.elections, 'leaders elected'],
  [S.maxTerm, 'max term'], [S.crashes, `crashes (${S.crashPoints} at crash points)`], [S.maxLog, 'longest log'],
  [num(S.completed), 'ops completed'], [num(S.indeterminate), 'ops indeterminate'],
];
$('stats').innerHTML = stats.map(([v, l]) => `<div class="stat"><b>${v}</b><span>${l}</span></div>`).join('');

// ---- drawing
const LEFT = 56, RIGHT = 24;
let zoom = 1;
const baseWidth = () => Math.max(600, $('tlwrap').clientWidth - 2);
const scale = () => (baseWidth() * zoom - LEFT - RIGHT) / D.end;

function axis(svg, y, height) {
  const s = scale();
  const steps = [1e3, 2e3, 5e3, 1e4, 2e4, 5e4, 1e5, 2e5, 5e5, 1e6, 2e6, 5e6, 1e7];
  const step = steps.find((st) => st * s >= 90) || 1e7;
  for (let t = 0; t <= D.end; t += step) {
    const x = LEFT + t * s;
    el('line', { x1: x, x2: x, y1: y, y2: y + height, stroke: 'var(--line)' }, svg);
    const label = step >= 1e6 ? (t / 1e6) + 's' : (t / 1e3) + 'ms';
    el('text', { x: x + 3, y: y + 11, class: 'muted' }, svg).textContent = label;
  }
}

// Row labels live in a group that follows horizontal scrolling.
function stickyLabels(svg, wrap, rows, height) {
  const g = el('g', {}, null);
  el('rect', { x: 0, y: 0, width: LEFT - 6, height, fill: 'var(--panel)' }, g);
  for (const [text, y] of rows) el('text', { x: 12, y }, g).textContent = text;
  svg.appendChild(g);
  const sync = () => g.setAttribute('transform', `translate(${wrap.scrollLeft},0)`);
  wrap.addEventListener('scroll', sync);
  sync();
}

function drawTimeline() {
  const wrap = $('tlwrap');
  wrap.innerHTML = '';
  const s = scale();
  const rowH = 22, top = 42, W = baseWidth() * zoom;
  const H = top + D.nodes * rowH + 12;
  const svg = el('svg', { width: W, height: H }, wrap);
  axis(svg, 0, H);
  const recover = D.recover;
  if (recover !== null) {
    const x = LEFT + recover * s;
    el('rect', { x, y: top - 4, width: Math.max(0, W - RIGHT - x), height: D.nodes * rowH + 4,
      fill: 'var(--leader)', opacity: 0.06 }, svg);
    el('text', { x: x + 4, y: top + D.nodes * rowH + 10, class: 'muted' }, svg).textContent = 'recovery →';
  }
  for (const [n, from, to, lane, term] of D.spans) {
    const x = LEFT + from * s, w = Math.max(1, (to - from) * s);
    const y = top + n * rowH + 3;
    const r = el('rect', { x, y, width: w, height: rowH - 6, rx: 3, fill: `var(--${lane})` }, svg);
    hover(r, `n${n} ${lane}${term ? ' · term ' + term : ''}\n${fmt(from)} → ${fmt(to)}`);
    if (lane === 'leader' && w > 26) {
      el('text', { x: x + 5, y: y + 12, fill: '#fff', style: 'fill:#fff;pointer-events:none' }, svg).textContent = 't' + term;
    }
  }
  for (const [t, label] of D.faults) {
    const x = LEFT + t * s;
    el('line', { x1: x, x2: x, y1: 18, y2: top + D.nodes * rowH, stroke: 'var(--fault)', 'stroke-dasharray': '2 3', opacity: 0.7 }, svg);
    const m = el('path', { d: `M${x - 5},16 L${x + 5},16 L${x},24 Z`, fill: 'var(--fault)' }, svg);
    hover(m, `${fmt(t)}  ${label}`);
  }
  stickyLabels(svg, wrap, [...Array(D.nodes).keys()].map((n) => ['n' + n, top + n * rowH + 15]), H);
}

function drawKeys() {
  const root = $('keys');
  root.innerHTML = '';
  const s = scale();
  const W = baseWidth() * zoom;
  const byId = new Map(D.history.map((o) => [o[0], o]));
  for (const v of D.verdicts) {
    const ops = D.history.filter((o) => o[2] === v.key);
    const rank = new Map(v.order.map((id, i) => [id, i + 1]));
    const head = document.createElement('div');
    head.className = 'keyhead';
    const verdict = v.status === 'ok' ? '<span class="badge pass">linearizable</span>'
      : v.status === 'fail' ? '<span class="badge fail">not linearizable</span>'
      : '<span class="badge">search budget exceeded</span>';
    head.innerHTML = `<b>k${v.key}</b>${verdict}<span class="sub">${ops.length} operations</span>`;
    root.appendChild(head);
    const wrap = document.createElement('div');
    wrap.className = 'scroll';
    root.appendChild(wrap);
    const rowH = 24, top = 20;
    const H = top + D.clients * rowH + 8;
    const svg = el('svg', { width: W, height: H }, wrap);
    axis(svg, 0, H);
    for (const [id, client, key, kind, label, result, inv, ret] of ops) {
      const end = ret === null ? D.end : ret;
      const x = LEFT + inv * s, w = Math.max(2, (end - inv) * s);
      const y = top + client * rowH + 4;
      const culprit = v.culprit === id;
      const faded = v.status === 'fail' && !rank.has(id) && !culprit;
      const color = culprit ? 'var(--bad)' : kind === 'r' ? 'var(--read)' : kind === 'w' ? 'var(--write)' : 'var(--cas)';
      const attrs = { x, y, width: w, height: rowH - 8, rx: 3, fill: color,
        'fill-opacity': ret === null ? 0.12 : 0.85, opacity: faded ? 0.3 : 1 };
      if (ret === null) Object.assign(attrs, { stroke: color, 'stroke-dasharray': '3 2' });
      const r = el('rect', attrs, svg);
      const text = `${label} ${result}`;
      const order = rank.has(id) ? `\nlinearization position ${rank.get(id)}` : '';
      hover(r, `#${id} c${client} ${text}\n${fmt(inv)} → ${ret === null ? 'never returned' : fmt(ret)}${order}${culprit ? '\n▶ could not be linearized' : ''}`);
      if (w > text.length * 6.8 + 8) {
        el('text', { x: x + 4, y: y + 12, style: `fill:${ret === null ? color : '#fff'};pointer-events:none` }, svg).textContent = text;
      }
    }
    stickyLabels(svg, wrap, [...Array(D.clients).keys()].map((c) => ['c' + c, top + c * rowH + 16]), H);
  }
}

function draw() { drawTimeline(); drawKeys(); }
$('zoom').addEventListener('input', (e) => { zoom = +e.target.value; $('zoomval').textContent = zoom + '×'; draw(); });
window.addEventListener('resize', draw);
draw();

// Start zoomed in on the first violation, if any.
const first = D.violations.find((v) => v.culprit !== null) || D.violations[0];
if (first) {
  zoom = Math.min(40, Math.max(1, Math.round(D.end / 1.5e6)));
  $('zoom').value = zoom; $('zoomval').textContent = zoom + '×';
  draw();
  const x = LEFT + first.at * scale() - baseWidth() / 2;
  document.querySelectorAll('.scroll').forEach((s) => { s.scrollLeft = x; });
}
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escaping_is_script_safe() {
        assert_eq!(json_str("a\"b\\c\n</script>"), "\"a\\\"b\\\\c\\n\\u003c/script\\u003e\"");
    }
}
