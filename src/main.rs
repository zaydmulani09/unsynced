use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use unsynced::{Crash, Options, Trace};

const USAGE: &str = "\
unsynced - find crash-consistency bugs by exploring every disk state a power loss can leave

USAGE:
  unsynced run    [--dir DIR] [--init DIR] --check CMD [--recover CMD] [OPTIONS] -- WORKLOAD...
  unsynced record [--dir DIR] [--init DIR] -o BUNDLE -- WORKLOAD...
  unsynced check  BUNDLE --check CMD [--recover CMD] [OPTIONS]
  unsynced show   BUNDLE

`run` and `record` trace WORKLOAD with strace (Linux). WORKLOAD operates on the
directory under test: --dir (default: a fresh temp dir), seeded from --init.
CMD runs once per crash state and must exit 0 iff that state is acceptable.
In both, {dir} expands to the directory and {marks} to a file holding what
WORKLOAD printed to stdout before the crash (also $UNSYNCED_DIR, $UNSYNCED_MARKS).

With --recover, CMD repairs each crash state before --check runs, and is itself
traced (Linux) and crashed at every point: recovery must survive a second crash.

OPTIONS:
  --profile posix|ext4  persistence model (default posix: the weakest legal)
  --block-size N        writes tear at N-byte boundaries (default 4096)
  --jobs N              checkers to run in parallel (default: CPU count)
  --timeout SECS        per-check time limit (default 30)
  --max-states N        cap on unique crash states (default 20000)
  --exhaustive N        try every subset when <= N micro-ops are in flight (default 10)
  --samples N           random subsets per crash point beyond that (default 32)
  --seed N              seed for those samples
  --explain N           minimize up to N failing states to find root causes (default 16)
  --json                print the report as JSON

EXIT STATUS: 0 no bugs found, 1 bugs found, 2 error";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

#[derive(Default)]
struct Args {
    positional: Vec<String>,
    workload: Vec<String>,
    dir: Option<PathBuf>,
    init: Option<PathBuf>,
    out: Option<PathBuf>,
    check: Option<String>,
    recover: Option<String>,
    timeout: u64,
    json: bool,
    opts: Options,
}

fn parse(mut raw: Vec<String>) -> Result<Args, String> {
    let mut a = Args { timeout: 30, ..Default::default() };
    if let Some(i) = raw.iter().position(|s| s == "--") {
        a.workload = raw.split_off(i + 1);
        raw.pop();
    }
    let mut it = raw.into_iter();
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        fn num<T: std::str::FromStr>(name: &str, v: String) -> Result<T, String> {
            v.parse().map_err(|_| format!("{name}: `{v}` is not a valid number"))
        }
        match arg.as_str() {
            "--dir" => a.dir = Some(val("--dir")?.into()),
            "--init" => a.init = Some(val("--init")?.into()),
            "-o" | "--out" => a.out = Some(val("-o")?.into()),
            "--check" => a.check = Some(val("--check")?),
            "--recover" => a.recover = Some(val("--recover")?),
            "--profile" => a.opts.profile = val("--profile")?.parse()?,
            "--block-size" => a.opts.block_size = num("--block-size", val("--block-size")?)?,
            "--jobs" => a.opts.jobs = num("--jobs", val("--jobs")?)?,
            "--timeout" => a.timeout = num("--timeout", val("--timeout")?)?,
            "--max-states" => a.opts.max_states = num("--max-states", val("--max-states")?)?,
            "--exhaustive" => a.opts.search.exhaustive_limit = num("--exhaustive", val("--exhaustive")?)?,
            "--samples" => a.opts.search.samples = num("--samples", val("--samples")?)?,
            "--seed" => a.opts.search.seed = num("--seed", val("--seed")?)?,
            "--explain" => a.opts.explain = num("--explain", val("--explain")?)?,
            "--json" => a.json = true,
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`\n\n{USAGE}")),
            _ => a.positional.push(arg),
        }
    }
    Ok(a)
}

fn run(raw: Vec<String>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let Some(cmd) = raw.first().cloned() else {
        println!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let a = parse(raw[1..].to_vec())?;
    match cmd.as_str() {
        "run" => {
            let check = a.check.clone().ok_or("`run` needs --check CMD")?;
            let (trace, _dir) = record(&a)?;
            report(&trace, &check, &a)
        }
        "record" => {
            let out = a.out.clone().ok_or("`record` needs -o BUNDLE")?;
            let (trace, _dir) = record(&a)?;
            trace.save(&out)?;
            eprintln!("recorded {} ops into {}", trace.ops.len(), out.display());
            Ok(ExitCode::SUCCESS)
        }
        "check" => {
            let [bundle] = a.positional.as_slice() else {
                return Err("`check` needs one BUNDLE".into());
            };
            let check = a.check.clone().ok_or("`check` needs --check CMD")?;
            let trace = Trace::load(Path::new(bundle))?;
            report(&trace, &check, &a)
        }
        "show" => {
            let [bundle] = a.positional.as_slice() else {
                return Err("`show` needs one BUNDLE".into());
            };
            let trace = Trace::load(Path::new(bundle))?;
            println!("initial state: {} entries", trace.initial.entries.len());
            for (i, op) in trace.ops.iter().enumerate() {
                println!("#{i:<5} {op}");
            }
            Ok(ExitCode::SUCCESS)
        }
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}").into()),
    }
}

fn record(a: &Args) -> Result<(Trace, PathBuf), Box<dyn std::error::Error>> {
    if a.workload.is_empty() {
        return Err("give the workload after `--`".into());
    }
    let dir = match &a.dir {
        Some(d) => d.clone(),
        None => std::env::temp_dir().join(format!("unsynced-dir-{}", std::process::id())),
    };
    if a.dir.is_none() && dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    if let Some(init) = &a.init {
        unsynced::Tree::load(init)?.write_to(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let dir = std::fs::canonicalize(&dir)?;
    let shown = dir.to_string_lossy().into_owned();
    let workload: Vec<String> = a.workload.iter().map(|w| w.replace("{dir}", &shown)).collect();
    let rec = unsynced::strace::record(&dir, &workload)?;
    for w in rec.warnings {
        eprintln!("warning: {w}");
    }
    if !rec.success {
        eprintln!("warning: the workload failed; checking what it did anyway");
    }
    Ok((rec.trace, dir))
}

fn report(trace: &Trace, check: &str, a: &Args) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let timeout = Duration::from_secs(a.timeout);
    let checker = |crash: &Crash| run_checker(check, crash, timeout);
    let report = match &a.recover {
        None => unsynced::check(trace, &a.opts, checker)?,
        Some(cmd) => {
            let recover = |dir: &Path| {
                let cmd = cmd.replace("{dir}", &dir.to_string_lossy());
                let rec = unsynced::strace::record(dir, &["sh".into(), "-c".into(), cmd.clone()])
                    .map_err(|e| e.to_string())?;
                if rec.success { Ok(rec.trace) } else { Err(format!("`{cmd}` failed")) }
            };
            unsynced::check_with_recovery(trace, &a.opts, recover, checker)?
        }
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{report}");
    }
    Ok(if report.is_clean() { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

/// Run the user's check command against one crash state.
fn run_checker(cmd: &str, crash: &Crash, timeout: Duration) -> Result<(), String> {
    let sibling = |ext: &str| crash.dir.with_extension(ext);
    let (marks, out) = (sibling("marks"), sibling("out"));
    let io = |e: std::io::Error| format!("could not run checker: {e}");
    std::fs::write(&marks, crash.marks.concat()).map_err(io)?;
    let cmd = cmd.replace("{dir}", &crash.dir.to_string_lossy()).replace("{marks}", &marks.to_string_lossy());
    let log = std::fs::File::create(&out).map_err(io)?;
    let mut sh = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&cmd);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&cmd);
        c
    };
    let mut child = sh
        .env("UNSYNCED_DIR", crash.dir)
        .env("UNSYNCED_MARKS", &marks)
        .env("UNSYNCED_POINT", crash.point.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(io)?)
        .stderr(log)
        .spawn()
        .map_err(io)?;
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(io)? {
            break Some(s);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let output = String::from_utf8_lossy(&std::fs::read(&out).unwrap_or_default()).into_owned();
    match status {
        Some(s) if s.success() => Ok(()),
        Some(s) => Err(if output.trim().is_empty() { format!("checker exited with {s}") } else { output }),
        None => Err(format!("checker timed out after {}s", timeout.as_secs())),
    }
}
