//! The checking loop: enumerate crash states, dedupe them by content, run the
//! checker on each unique state in parallel, then minimize every failure down
//! to the few lost operations that cause it and explain them.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Serialize;

use crate::Error;
use crate::crash::Search;
use crate::model::{Micro, Profile, Program, compile, split};
use crate::trace::{Entry, Op, Trace, Tree};

/// Everything that controls a check.
#[derive(Debug, Clone)]
pub struct Options {
    pub profile: Profile,
    /// Writes tear at multiples of this many bytes.
    pub block_size: u64,
    pub search: Search,
    /// Stop enumerating after this many unique crash states.
    pub max_states: usize,
    /// Checker processes/threads to run at once.
    pub jobs: usize,
    /// Minimize and explain at most this many distinct vulnerabilities.
    pub explain: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            profile: Profile::Posix,
            block_size: 4096,
            search: Search::default(),
            max_states: 20_000,
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
            explain: 8,
        }
    }
}

/// What a checker is given: a directory holding one possible post-crash state.
#[derive(Debug)]
pub struct Crash<'a> {
    /// The recovered directory. The checker may modify it (e.g. run recovery).
    pub dir: &'a Path,
    /// How many trace ops had completed when the power went out.
    pub point: usize,
    /// Marks the workload emitted before the crash (what it had acknowledged).
    pub marks: &'a [String],
}

/// Outcome of a check.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub profile: Profile,
    pub ops: usize,
    pub crash_points: usize,
    /// Unique on-disk states the checker ran against.
    pub states: usize,
    /// True if `max_states` cut the enumeration short.
    pub truncated: bool,
    /// States the checker rejected.
    pub failures: usize,
    pub vulnerabilities: Vec<Vulnerability>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.failures == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Part of a single write persisted and part did not.
    TornWrite,
    /// A later operation persisted while an earlier one was lost.
    Reordering,
    /// Acknowledged state was lost because it was never made durable.
    Unsynced,
    /// The state is invalid even with every operation persisted: the workload
    /// passes through a state it cannot recover from (e.g. truncate-then-write).
    NonAtomic,
}

/// A trace op, by index, with a readable description.
#[derive(Debug, Clone, Serialize)]
pub struct OpRef {
    pub index: usize,
    pub op: String,
}

/// One minimized, explained crash-consistency bug.
#[derive(Debug, Clone, Serialize)]
pub struct Vulnerability {
    pub kind: Kind,
    /// The crash happened after this many ops completed.
    pub crash_point: usize,
    /// The op that completed just before the crash.
    pub crash_after: Option<OpRef>,
    /// Ops whose effect (in whole or in part) must be lost to trigger it.
    pub lost: Vec<OpRef>,
    /// Later, non-durable ops that nevertheless persisted.
    pub survived: Vec<OpRef>,
    /// The checker's complaint on the minimized state.
    pub message: String,
    pub hint: String,
    /// Failing states attributed to this vulnerability.
    pub occurrences: usize,
}

type Key = (u64, usize);

struct Engine<'a, F> {
    trace: &'a Trace,
    program: Program,
    checker: F,
    cache: Mutex<HashMap<Key, Result<(), String>>>,
    scratch: PathBuf,
}

/// Explore every crash state of `trace` and run `checker` on each.
///
/// `checker` returns `Err(reason)` when the recovered state is unacceptable.
pub fn check<F>(trace: &Trace, opts: &Options, checker: F) -> Result<Report, Error>
where
    F: Fn(&Crash) -> Result<(), String> + Sync,
{
    let program = compile(trace, opts.profile, opts.block_size)?;
    let scratch = std::env::temp_dir().join(format!(
        "unsynced-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let engine = Engine { trace, program, checker, cache: Mutex::new(HashMap::new()), scratch };
    let result = engine.run(opts);
    let _ = std::fs::remove_dir_all(&engine.scratch);
    result
}

struct Job {
    point: usize,
    persisted: Vec<bool>,
    key: Key,
}

impl<F> Engine<'_, F>
where
    F: Fn(&Crash) -> Result<(), String> + Sync,
{
    fn run(&self, opts: &Options) -> Result<Report, Error> {
        let p = &self.program;
        // 1. Enumerate and dedupe by (content, marks seen).
        let mut jobs: Vec<Job> = Vec::new();
        let mut seen: HashMap<Key, ()> = HashMap::new();
        let mut truncated = false;
        'enumerate: for point in 0..p.op_end.len() {
            for mut persisted in p.candidates(point, &opts.search) {
                p.close(point, &mut persisted);
                let tree = p.materialize(&persisted);
                let key = (hash(&tree), p.marks_before[point]);
                if seen.insert(key, ()).is_none() {
                    if jobs.len() == opts.max_states {
                        truncated = true;
                        break 'enumerate;
                    }
                    jobs.push(Job { point, persisted, key });
                }
            }
        }

        // 2. Check every unique state, in parallel.
        let next = AtomicUsize::new(0);
        let results: Vec<Mutex<Option<Result<(), String>>>> = jobs.iter().map(|_| Mutex::new(None)).collect();
        let io_error: Mutex<Option<std::io::Error>> = Mutex::new(None);
        std::thread::scope(|s| {
            for w in 0..opts.jobs.max(1) {
                let (next, results, jobs, io_error) = (&next, &results, &jobs, &io_error);
                s.spawn(move || {
                    let dir = self.scratch.join(format!("w{w}"));
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break };
                        match self.run_tree(&p.materialize(&job.persisted), job.point, &dir) {
                            Ok(r) => *results[i].lock().unwrap() = Some(r),
                            Err(e) => {
                                io_error.lock().unwrap().get_or_insert(e);
                                break;
                            }
                        }
                    }
                });
            }
        });
        if let Some(e) = io_error.into_inner().unwrap() {
            return Err(e.into());
        }
        let mut failing = Vec::new();
        for (job, r) in jobs.iter().zip(results) {
            let r = r.into_inner().unwrap().expect("every job ran");
            if r.is_err() {
                failing.push(job);
            }
            self.cache.lock().unwrap().insert(job.key, r);
        }

        // 3. Minimize failures into distinct vulnerabilities. Simplest first.
        failing.sort_by_key(|j| (j.point, j.persisted.iter().filter(|p| !**p).count()));
        let mut vulns: Vec<(Vec<usize>, Vulnerability)> = Vec::new();
        let dir = self.scratch.join("min");
        for job in &failing {
            let lost = self.lost_ops(&job.persisted);
            if let Some((_, v)) = vulns.iter_mut().find(|(sig, _)| sig.iter().all(|o| lost.contains(o))) {
                v.occurrences += 1;
                continue;
            }
            if vulns.len() >= opts.explain {
                continue;
            }
            let (persisted, message) = self.minimize(job.point, job.persisted.clone(), &dir)?;
            let sig = self.lost_ops(&persisted);
            match vulns.iter_mut().find(|(s, _)| *s == sig) {
                Some((_, v)) => v.occurrences += 1,
                None => {
                    let v = self.explain(job.point, &persisted, message);
                    vulns.push((sig, v));
                }
            }
        }

        Ok(Report {
            profile: p.profile,
            ops: self.trace.ops.len(),
            crash_points: p.op_end.len(),
            states: jobs.len(),
            truncated,
            failures: failing.len(),
            vulnerabilities: vulns.into_iter().map(|(_, v)| v).collect(),
        })
    }

    fn run_tree(&self, tree: &Tree, point: usize, dir: &Path) -> std::io::Result<Result<(), String>> {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        tree.write_to(dir)?;
        let p = &self.program;
        let marks = &p.marks[..p.marks_before[point]];
        Ok((self.checker)(&Crash { dir, point, marks }))
    }

    /// Check one persisted-set, through the cache.
    fn verdict(&self, point: usize, persisted: &[bool], dir: &Path) -> std::io::Result<Result<(), String>> {
        let tree = self.program.materialize(persisted);
        let key = (hash(&tree), self.program.marks_before[point]);
        if let Some(r) = self.cache.lock().unwrap().get(&key) {
            return Ok(r.clone());
        }
        let r = self.run_tree(&tree, point, dir)?;
        self.cache.lock().unwrap().insert(key, r.clone());
        Ok(r)
    }

    /// Greedily re-persist lost micro-ops (whole ops first, then single
    /// micro-ops) as long as the state still fails.
    fn minimize(
        &self,
        point: usize,
        mut persisted: Vec<bool>,
        dir: &Path,
    ) -> std::io::Result<(Vec<bool>, String)> {
        let p = &self.program;
        let mut message = self.verdict(point, &persisted, dir)?.err().unwrap_or_default();
        let mut try_add = |persisted: &mut Vec<bool>, add: &dyn Fn(usize) -> bool| -> std::io::Result<()> {
            let mut trial = persisted.clone();
            for (j, t) in trial.iter_mut().enumerate() {
                *t |= add(j);
            }
            p.close(point, &mut trial);
            if trial != *persisted
                && let Err(m) = self.verdict(point, &trial, dir)?
            {
                *persisted = trial;
                message = m;
            }
            Ok(())
        };
        for op in self.lost_ops(&persisted) {
            try_add(&mut persisted, &|j| p.micro[j].op == op)?;
        }
        for j in 0..persisted.len() {
            if !persisted[j] {
                try_add(&mut persisted, &|k| k == j)?;
            }
        }
        Ok((persisted, message))
    }

    fn lost_ops(&self, persisted: &[bool]) -> Vec<usize> {
        let mut ops: Vec<usize> =
            (0..persisted.len()).filter(|&j| !persisted[j]).map(|j| self.program.micro[j].op).collect();
        ops.dedup();
        ops
    }

    fn op_ref(&self, index: usize) -> OpRef {
        OpRef { index, op: self.trace.ops[index].to_string() }
    }

    fn explain(&self, point: usize, persisted: &[bool], message: String) -> Vulnerability {
        let p = &self.program;
        let lost_micro: Vec<usize> = (0..persisted.len()).filter(|&j| !persisted[j]).collect();
        let lost = self.lost_ops(persisted);
        let first_lost = lost_micro.first().copied().unwrap_or(0);
        let mut survived: Vec<usize> = (first_lost..persisted.len())
            .filter(|&j| persisted[j] && p.micro[j].durable_at >= point && !lost.contains(&p.micro[j].op))
            .map(|j| p.micro[j].op)
            .collect();
        survived.dedup();
        let torn = lost.iter().any(|&o| {
            matches!(self.trace.ops[o], Op::Write { .. })
                && (0..persisted.len()).any(|j| {
                    persisted[j] && p.micro[j].op == o && matches!(p.micro[j].m, Micro::Write { .. })
                })
        });
        let kind = if lost_micro.is_empty() {
            survived.clear();
            Kind::NonAtomic
        } else if torn {
            Kind::TornWrite
        } else if !survived.is_empty() {
            Kind::Reordering
        } else {
            Kind::Unsynced
        };
        let hint = self.hint(kind, lost_micro.first().map(|&j| &p.micro[j]), &survived);
        Vulnerability {
            kind,
            crash_point: point,
            crash_after: point.checked_sub(1).map(|i| self.op_ref(i)),
            lost: lost.iter().map(|&i| self.op_ref(i)).collect(),
            survived: survived.iter().take(5).map(|&i| self.op_ref(i)).collect(),
            message,
            hint,
            occurrences: 1,
        }
    }

    fn hint(&self, kind: Kind, first: Option<&crate::model::MicroOp>, survived: &[usize]) -> String {
        let Some(first) = first else {
            return "the checker rejects this state even with every operation persisted: the \
                    workload's own on-disk state is inconsistent here (or the checker is wrong)"
                .into();
        };
        let op = &self.trace.ops[first.op];
        let shown = |s: &str| {
            if s.is_empty() { ".".to_string() } else { s.to_string() }
        };
        if kind == Kind::TornWrite {
            return format!(
                "`{op}` spans several blocks and a crash can persist only some of them; \
                 detect torn records with a checksum, or write a temp file, fsync it, and rename it into place"
            );
        }
        match (&first.m, op) {
            (Micro::Link { .. } | Micro::Unlink { .. }, _) => {
                let dir = split(op.path().unwrap_or("")).0;
                format!("fsync the directory `{}` after `{op}` to make the entry durable", shown(dir))
            }
            (Micro::Rename { .. }, Op::Rename { to, .. }) => format!(
                "fsync the directory `{}` after `{op}`; until then the rename can be undone",
                shown(split(to).0)
            ),
            (Micro::Write { .. } | Micro::SetLen { .. }, _) => {
                let path = shown(op.path().unwrap_or(""));
                let renamed = survived.iter().find_map(|&i| match &self.trace.ops[i] {
                    Op::Rename { from, .. } if shown(from) == path => Some(i),
                    _ => None,
                });
                match (kind, renamed) {
                    (Kind::Reordering, Some(i)) => format!(
                        "fsync `{path}` before `{}` (op #{i}); the rename reached disk before the data it points to",
                        self.trace.ops[i]
                    ),
                    (Kind::Reordering, None) => format!(
                        "`{op}` can reach disk after later writes; fsync `{path}` before anything that depends on it"
                    ),
                    _ => {
                        format!("`{op}` was not durable at the crash; fsync `{path}` before acknowledging it")
                    }
                }
            }
            _ => format!("`{op}` was lost in the crash"),
        }
    }
}

fn hash(tree: &Tree) -> u64 {
    let mut h = DefaultHasher::new();
    for (path, entry) in &tree.entries {
        path.hash(&mut h);
        match entry {
            Entry::Dir => 0u8.hash(&mut h),
            Entry::File(d) => d.hash(&mut h),
        }
    }
    h.finish()
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let profile = match self.profile {
            Profile::Posix => "posix",
            Profile::Ext4Ordered => "ext4-ordered",
        };
        writeln!(
            f,
            "{} ops, {} crash points, {} unique crash states checked ({profile} model){}",
            self.ops,
            self.crash_points,
            self.states,
            if self.truncated { " [TRUNCATED: raise --max-states]" } else { "" }
        )?;
        if self.is_clean() {
            return writeln!(f, "OK: the checker accepted every crash state");
        }
        writeln!(
            f,
            "FAIL: {} crash states rejected, {} distinct vulnerabilit{}:",
            self.failures,
            self.vulnerabilities.len(),
            if self.vulnerabilities.len() == 1 { "y" } else { "ies" }
        )?;
        for (n, v) in self.vulnerabilities.iter().enumerate() {
            let kind = match v.kind {
                Kind::TornWrite => "torn write",
                Kind::Reordering => "reordering",
                Kind::Unsynced => "unsynced",
                Kind::NonAtomic => "non-atomic update",
            };
            writeln!(f, "\n[{}] {kind} ({} failing states)", n + 1, v.occurrences)?;
            match &v.crash_after {
                Some(o) => writeln!(f, "  crash after   #{} {}", o.index, o.op)?,
                None => writeln!(f, "  crash before the first op")?,
            }
            for o in &v.lost {
                writeln!(f, "  lost          #{} {}", o.index, o.op)?;
            }
            for o in &v.survived {
                writeln!(f, "  but persisted #{} {}", o.index, o.op)?;
            }
            let msg = v.message.trim();
            if !msg.is_empty() {
                let msg: String = msg.lines().take(6).collect::<Vec<_>>().join("\n                ");
                writeln!(f, "  checker said  {msg}")?;
            }
            writeln!(f, "  fix           {}", v.hint)?;
        }
        Ok(())
    }
}
