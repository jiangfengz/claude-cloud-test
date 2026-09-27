use hourglass::bugs::{Bug, BugSet};
use hourglass::checker::Violation;
use hourglass::fuzz;
use hourglass::nemesis::FaultPlan;
use hourglass::shrink;
use hourglass::sim::{self, FaultMode, RunReport, SimConfig};
use hourglass::time::{self, SECS, ms};
use hourglass::viz::{self, Style};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::str::FromStr;
use std::time::{Duration, Instant};

// Writes that ignore a closed stdout (e.g. when piped into `head`).
macro_rules! outln {
    ($($t:tt)*) => {{
        let _ = writeln!(std::io::stdout(), $($t)*);
    }};
}
macro_rules! out {
    ($($t:tt)*) => {{
        let _ = write!(std::io::stdout(), $($t)*);
    }};
}

const USAGE: &str = "\
hourglass — deterministic simulation testing for a Raft KV store

USAGE:
    hourglass run   [options]     simulate one seed and check it
    hourglass fuzz  [options]     explore many seeds in parallel; shrink the first failure
    hourglass determinism [--seeds N]   verify that every run replays bit-for-bit
    hourglass bugs                list the injectable bugs

CLUSTER & WORKLOAD:
    --seed N          seed for `run` (default 0)
    --nodes N         cluster size, 1–9 (default 5)
    --clients N       concurrent clients, 1–9 (default 4)
    --ops N           operations per client (default 120)
    --keys N          number of keys (default 3)
    --bug NAME        enable an injected bug (repeatable, or comma-separated)

FAULTS:
    --faults PLAN     replay an explicit fault plan, e.g. '300:crash(L),900:restart(*)'
    --no-faults       no nemesis (the network is still lossy and reorders)
    --no-swarm        use one fixed fault mix instead of a per-seed random profile
    --no-crash-points disable BUGGIFY-style crashes right after durable writes
    --chaos MS        length of the fault phase in ms (default 8000)

OUTPUT:
    --trace           print every delivered message and fault (run)
    --html FILE       write an interactive HTML report of the run (or shrunk failure)
    --width N         timeline width in columns (default 72)
    --no-timeline     do not draw the cluster timeline
    --no-color        disable ANSI colours

FUZZING:
    --seeds N         number of seeds to try (default 200)
    --start N         first seed (default 0)
    --jobs N          worker threads (default: all cores)
    --stop-after N    stop once N failing seeds have been found
    --no-shrink       do not minimise the first failure
    --shrink-runs N   simulation budget for shrinking (default 400)
";

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let code = match real_main(raw) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}\n\nrun `hourglass help` for usage");
            2
        }
    };
    std::process::exit(code);
}

fn real_main(raw: Vec<String>) -> Result<i32, String> {
    let args = Args::parse(raw)?;
    match args.cmd.as_str() {
        "run" => cmd_run(&args),
        "fuzz" => cmd_fuzz(&args),
        "determinism" => cmd_determinism(&args),
        "bugs" => {
            outln!("Injectable bugs (enable with --bug NAME):\n");
            for b in Bug::ALL {
                outln!("  {:<18} {}", b.name(), b.description());
            }
            Ok(0)
        }
        "help" | "" => {
            out!("{USAGE}");
            Ok(0)
        }
        other => Err(format!("unknown command `{other}`")),
    }
}

// ------------------------------------------------------------------ args

const SWITCHES: &[&str] =
    &["trace", "no-faults", "no-swarm", "no-crash-points", "no-shrink", "no-color", "no-timeline", "help"];

struct Args {
    cmd: String,
    opts: BTreeMap<String, Vec<String>>,
    switches: BTreeSet<String>,
}

impl Args {
    fn parse(raw: Vec<String>) -> Result<Args, String> {
        let mut it = raw.into_iter();
        let mut cmd = String::new();
        let mut opts: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut switches = BTreeSet::new();
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                let (name, inline) = match name.split_once('=') {
                    Some((n, v)) => (n.to_string(), Some(v.to_string())),
                    None => (name.to_string(), None),
                };
                if SWITCHES.contains(&name.as_str()) {
                    switches.insert(name);
                } else {
                    let value = match inline {
                        Some(v) => v,
                        None => it.next().ok_or_else(|| format!("--{name} needs a value"))?,
                    };
                    opts.entry(name).or_default().push(value);
                }
            } else if a == "-h" {
                switches.insert("help".into());
            } else if cmd.is_empty() {
                cmd = a;
            } else {
                return Err(format!("unexpected argument `{a}`"));
            }
        }
        if switches.contains("help") {
            cmd = "help".into();
        }
        Ok(Args { cmd, opts, switches })
    }

    fn flag(&self, name: &str) -> bool {
        self.switches.contains(name)
    }

    fn str(&self, name: &str) -> Option<&str> {
        self.opts.get(name).and_then(|v| v.last()).map(String::as_str)
    }

    fn get<T: FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.str(name) {
            None => Ok(default),
            Some(s) => s.parse().map_err(|_| format!("invalid value for --{name}: `{s}`")),
        }
    }

    fn style(&self) -> Style {
        Style { color: !self.flag("no-color") && std::io::stdout().is_terminal() }
    }
}

fn sim_config(args: &Args) -> Result<SimConfig, String> {
    let d = SimConfig::default();
    let bugs = match args.opts.get("bug") {
        Some(list) => BugSet::parse(&list.join(","))?,
        None => BugSet::NONE,
    };
    let faults = match (args.str("faults"), args.flag("no-faults")) {
        (Some(_), true) => return Err("--faults and --no-faults are mutually exclusive".into()),
        (Some(p), false) => FaultMode::Explicit(p.parse::<FaultPlan>()?),
        (None, true) => FaultMode::Disabled,
        (None, false) => FaultMode::Generated,
    };
    let cfg = SimConfig {
        seed: args.get("seed", d.seed)?,
        nodes: args.get("nodes", d.nodes)?,
        clients: args.get("clients", d.clients)?,
        ops_per_client: args.get("ops", d.ops_per_client)?,
        keys: args.get("keys", d.keys)?,
        bugs,
        chaos: ms(args.get("chaos", d.chaos / 1000)?),
        faults,
        swarm: !args.flag("no-swarm"),
        crash_points: !args.flag("no-crash-points"),
        trace: args.flag("trace"),
        ..d
    };
    if !(1..=9).contains(&cfg.nodes) || !(1..=9).contains(&cfg.clients) || cfg.keys == 0 {
        return Err("--nodes and --clients must be 1–9, --keys at least 1".into());
    }
    Ok(cfg)
}

// -------------------------------------------------------------- printing

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn human_duration(t: u64) -> String {
    let secs = t / SECS;
    match secs {
        0..=59 => time::fmt(t),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

fn speedup(virtual_us: u64, wall: Duration) -> String {
    let wall_us = wall.as_micros().max(1) as f64;
    format!("{:.0}×", virtual_us as f64 / wall_us)
}

fn header(cfg: &SimConfig, style: Style) -> String {
    format!(
        "{} seed {} · {} nodes · {} clients × {} ops · {} keys · bugs: {}",
        style.bold("hourglass"),
        style.bold(&cfg.seed.to_string()),
        cfg.nodes,
        cfg.clients,
        cfg.ops_per_client,
        cfg.keys,
        if cfg.bugs.is_empty() { style.green("none") } else { style.red(&cfg.bugs.to_string()) }
    )
}

fn print_violation(v: &Violation, report: &RunReport, style: Style) {
    outln!(
        "  {} {} {}",
        style.red("●"),
        style.bold(&style.red(v.kind.name())),
        style.dim(&format!("at {}", time::fmt(v.at)))
    );
    outln!("    {}", v.message);
    if !v.ops.is_empty() {
        outln!();
        out!("{}", viz::render_ops(&report.history, &v.ops, v.culprit, style));
    }
    outln!();
}

fn print_report(cfg: &SimConfig, report: &RunReport, wall: Duration, args: &Args, style: Style) {
    let st = &report.stats;
    outln!("{}", header(cfg, style));
    if cfg.swarm {
        outln!("{}  {}", style.dim("swarm "), report.swarm.describe());
    }
    let plan = if report.plan.is_empty() { style.dim("(none)") } else { report.plan.to_string() };
    outln!("{}  {} {}", style.dim("faults"), style.dim(&format!("{} events:", report.plan.len())), plan);
    outln!();
    if !args.flag("no-timeline") {
        let width = args.get("width", 72usize).unwrap_or(72);
        out!("{}", viz::render_timeline(&report.timeline, width, style));
        outln!();
    }
    outln!(
        "{}  simulated {} of cluster time in {} ({} real time) · {} events",
        style.dim("sim    "),
        time::fmt(st.virtual_time),
        format_args!("{:.1?}", wall),
        speedup(st.virtual_time, wall),
        thousands(st.events)
    );
    outln!(
        "{}  {} messages sent · {} delivered · {} dropped · {} duplicated",
        style.dim("network"),
        thousands(st.sent),
        thousands(st.delivered),
        thousands(st.dropped),
        thousands(st.duplicated)
    );
    outln!(
        "{}  {} leaders elected (max term {}) · {} crashes ({} at crash points) · {} restarts · longest log {}",
        style.dim("cluster"),
        st.elections,
        st.max_term,
        st.crashes,
        st.crash_points,
        st.restarts,
        st.max_log
    );
    outln!(
        "{}  {} operations: {} ok, {} indeterminate",
        style.dim("clients"),
        report.history.ops.len(),
        report.history.completed(),
        report.history.indeterminate()
    );
    let kinds = report.kinds();
    let checks: Vec<String> = [
        hourglass::checker::ViolationKind::ElectionSafety,
        hourglass::checker::ViolationKind::LeaderCompleteness,
        hourglass::checker::ViolationKind::LogMatching,
        hourglass::checker::ViolationKind::StateMachineSafety,
        hourglass::checker::ViolationKind::Linearizability,
        hourglass::checker::ViolationKind::Liveness,
    ]
    .iter()
    .map(|k| {
        if kinds.contains(k) {
            format!("{} {}", k.name(), style.red("✗"))
        } else if *k == hourglass::checker::ViolationKind::Linearizability && st.lin_unknown > 0 {
            format!("{} {}", k.name(), style.yellow("?"))
        } else {
            format!("{} {}", k.name(), style.green("✓"))
        }
    })
    .collect();
    outln!("{}  {}", style.dim("checks "), checks.join("  "));
    outln!();
    if report.failed() {
        outln!("{}", style.bold(&style.red(&format!("✗ FAIL — {} violation(s)", report.violations.len()))));
        outln!();
        for v in &report.violations {
            print_violation(v, report, style);
        }
    } else {
        outln!("{}", style.bold(&style.green("✓ PASS")));
    }
}

fn write_html(path: &str, cfg: &SimConfig, report: &RunReport) -> Result<(), String> {
    std::fs::write(path, hourglass::html::render(cfg, report)).map_err(|e| format!("writing {path}: {e}"))?;
    outln!("HTML report written to {path}");
    Ok(())
}

// ------------------------------------------------------------- commands

fn cmd_run(args: &Args) -> Result<i32, String> {
    let cfg = sim_config(args)?;
    let style = args.style();
    let start = Instant::now();
    let report = sim::run(&cfg);
    let wall = start.elapsed();
    if cfg.trace {
        for line in &report.trace {
            outln!("{line}");
        }
        outln!();
    }
    print_report(&cfg, &report, wall, args, style);
    if let Some(path) = args.str("html") {
        write_html(path, &cfg, &report)?;
    }
    Ok(i32::from(report.failed()))
}

fn cmd_fuzz(args: &Args) -> Result<i32, String> {
    let base = sim_config(args)?;
    let style = args.style();
    let count: u64 = args.get("seeds", 200)?;
    let first: u64 = args.get("start", 0)?;
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let jobs: usize = args.get("jobs", cores)?;
    let stop_after: Option<usize> =
        args.str("stop-after").map(str::parse).transpose().map_err(|_| "invalid --stop-after")?;
    let seeds = first..first + count;

    outln!(
        "{} fuzz · seeds {}..{} · {} threads · {} nodes · {} clients × {} ops · bugs: {}",
        style.bold("hourglass"),
        seeds.start,
        seeds.end,
        jobs,
        base.nodes,
        base.clients,
        base.ops_per_client,
        if base.bugs.is_empty() { style.green("none") } else { style.red(&base.bugs.to_string()) }
    );
    let interactive = std::io::stderr().is_terminal();
    let progress = |done: u64, failed: usize| {
        if interactive && (done.is_multiple_of(8) || done == count) {
            let bar_len = 30;
            let filled = (done * bar_len / count.max(1)) as usize;
            eprint!(
                "\r  [{}{}] {done}/{count} · {failed} failing ",
                "#".repeat(filled),
                ".".repeat(bar_len as usize - filled)
            );
            let _ = std::io::stderr().flush();
        }
    };
    let summary = fuzz::fuzz(&base, seeds, jobs, stop_after, &progress);
    if interactive {
        eprintln!();
    }

    outln!(
        "\n  {} runs · simulated {} of cluster time in {:.2?} ({}) · {} events · {} client ops",
        summary.runs,
        human_duration(summary.virtual_time),
        summary.elapsed,
        speedup(summary.virtual_time, summary.elapsed),
        thousands(summary.events),
        thousands(summary.ops)
    );
    if summary.lin_unknown > 0 {
        outln!("  {} key histories exceeded the linearizability search budget", summary.lin_unknown);
    }
    if summary.failures.is_empty() {
        outln!("\n{}", style.bold(&style.green("✓ no violations found")));
        return Ok(0);
    }
    let pct = 100.0 * summary.failures.len() as f64 / summary.runs.max(1) as f64;
    outln!(
        "\n{}",
        style.bold(&style.red(&format!("✗ {} failing seeds ({pct:.1}%)", summary.failures.len())))
    );
    for (kind, n) in summary.by_kind() {
        outln!("    {:<22} {n}", kind.name());
    }
    let listed: Vec<String> = summary.failures.iter().take(12).map(|f| f.seed.to_string()).collect();
    let more = if summary.failures.len() > 12 { " …" } else { "" };
    outln!("  seeds: {}{more}", listed.join(" "));

    let first_fail = &summary.failures[0];
    let mut cfg = base.clone();
    cfg.seed = first_fail.seed;
    let report = sim::run(&cfg);

    if args.flag("no-shrink") {
        outln!("\nreproduce with:\n  {}", cfg.repro_command());
        if let Some(path) = args.str("html") {
            write_html(path, &cfg, &report)?;
        }
        return Ok(1);
    }

    let budget: usize = args.get("shrink-runs", 400)?;
    outln!("\n{} seed {} ({})…", style.bold("shrinking"), cfg.seed, first_fail.primary.name());
    let start = Instant::now();
    let shrunk = shrink::shrink(&cfg, &report, budget);
    outln!(
        "  faults {} → {} · clients {} → {} · ops/client {} → {} · {} simulations in {:.2?}\n",
        shrunk.original_faults,
        match &shrunk.config.faults {
            FaultMode::Explicit(p) => p.len(),
            _ => 0,
        },
        shrunk.original_clients,
        shrunk.config.clients,
        shrunk.original_ops,
        shrunk.config.ops_per_client,
        shrunk.runs,
        start.elapsed()
    );
    let r = &shrunk.report;
    if !args.flag("no-timeline") {
        let width = args.get("width", 72usize).unwrap_or(72);
        out!("{}", viz::render_timeline(&r.timeline, width, style));
        outln!();
    }
    for v in r.violations.iter().filter(|v| v.kind == shrunk.target).take(1) {
        print_violation(v, r, style);
    }
    outln!("reproduce with:\n  {}", style.bold(&shrunk.config.repro_command()));
    if let Some(path) = args.str("html") {
        write_html(path, &shrunk.config, r)?;
    }
    Ok(1)
}

fn cmd_determinism(args: &Args) -> Result<i32, String> {
    let base = sim_config(args)?;
    let count: u64 = args.get("seeds", 20)?;
    let first: u64 = args.get("start", 0)?;
    let mut bad = 0;
    for seed in first..first + count {
        let cfg = SimConfig { seed, ..base.clone() };
        match fuzz::check_determinism(&cfg) {
            Ok(fp) => outln!("  seed {seed:>4}  fingerprint {fp:016x}  ✓"),
            Err((a, b)) => {
                bad += 1;
                outln!("  seed {seed:>4}  {a:016x} ≠ {b:016x}  ✗");
            }
        }
    }
    if bad == 0 {
        outln!("all {count} seeds replay identically");
        Ok(0)
    } else {
        outln!("{bad} seeds diverged between runs");
        Ok(1)
    }
}
