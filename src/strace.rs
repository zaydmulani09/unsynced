//! Record any program's file-system operations with `strace` (Linux).
//!
//! We run `strace -f -y -xx` and reconstruct [`Op`]s from its output: `-y`
//! annotates every fd with its current path (so renames and `openat` dirfds
//! resolve for free), `-xx` hex-escapes all strings (so data is exact), and
//! we track open file descriptions ourselves to turn `write()` into
//! `write(offset)`, honoring `lseek`, `O_APPEND`, `dup` and `fork` sharing.
//!
//! Writes to fd 1 outside the root become [`Op::Mark`]s: what the program
//! printed is what it acknowledged.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::Error;
use crate::trace::{Entry, Op, Trace, Tree};

/// Largest single write we can capture; larger ones are reported as errors.
pub const MAX_WRITE: usize = 1 << 24;

/// The result of [`record`].
#[derive(Debug)]
pub struct Recording {
    pub trace: Trace,
    /// Operations that could not be modeled.
    pub warnings: Vec<String>,
    /// Whether the traced command exited successfully.
    pub success: bool,
}

/// Run `cmd` under strace with `root` as the directory under test.
///
/// The command's stdout is discarded; what it writes there is recorded as marks.
pub fn record(root: &Path, cmd: &[String]) -> Result<Recording, Error> {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    if cmd.is_empty() {
        return Err(Error::Strace("no workload command given".into()));
    }
    std::fs::create_dir_all(root)?;
    let initial = Tree::load(root)?;
    let root = std::fs::canonicalize(root)?;
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let log = std::env::temp_dir().join(format!(
        "unsynced-strace-{}-{}.log",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    ));
    let status = Command::new("strace")
        .args(["-f", "-qq", "-y", "-xx", "-s", &MAX_WRITE.to_string()])
        .args(["-e", "trace=%file,%desc,%process,%memory,sync"])
        .arg("-o")
        .arg(&log)
        .arg("--")
        .args(cmd)
        .stdout(Stdio::null())
        .status()
        .map_err(|e| {
            Error::Strace(format!("cannot run strace ({e}); install it (e.g. apt install strace)"))
        })?;
    let text = std::fs::read(&log).map(|b| String::from_utf8_lossy(&b).into_owned());
    let _ = std::fs::remove_file(&log);
    let text = text.map_err(|e| Error::Strace(format!("strace produced no log: {e}")))?;
    let (ops, warnings) = parse(&text, &root, &cwd, &initial)?;
    Ok(Recording { trace: Trace { initial, ops }, warnings, success: status.success() })
}

/// Parse an `strace -f -y -xx` log into ops on paths relative to `root`.
/// `root` and `cwd` must be absolute and canonical; `initial` is the state of
/// `root` when tracing began.
pub fn parse(log: &str, root: &Path, cwd: &Path, initial: &Tree) -> Result<(Vec<Op>, Vec<String>), Error> {
    let mut st = State::new(root, cwd, initial);
    let mut pending: HashMap<u32, String> = HashMap::new();
    for (n, raw) in log.lines().enumerate() {
        let Some((pid, rest)) = split_pid(raw) else {
            continue;
        };
        let line = if let Some(head) = rest.strip_suffix("<unfinished ...>") {
            pending.insert(pid, head.trim_end().to_string());
            continue;
        } else if let Some(tail) = rest.strip_prefix("<... ") {
            let Some(head) = pending.remove(&pid) else {
                continue;
            };
            let Some(i) = tail.find("resumed>") else {
                continue;
            };
            head + &tail[i + "resumed>".len()..]
        } else {
            rest.to_string()
        };
        let Some(call) = parse_call(&line) else {
            continue;
        };
        st.apply(pid, &call)
            .map_err(|e| Error::Strace(format!("strace log line {}: {e}\n  {raw}", n + 1)))?;
    }
    Ok((st.ops, st.warnings))
}

fn split_pid(line: &str) -> Option<(u32, &str)> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let pid = line[..digits].parse().ok()?;
    let rest = line[digits..].trim_start();
    (!rest.starts_with("+++") && !rest.starts_with("---")).then_some((pid, rest))
}

#[derive(Debug, PartialEq)]
struct Call {
    name: String,
    args: Vec<String>,
    ret: Option<i64>,
    /// The `-y` annotation on the return value (the path of a returned fd).
    ret_path: Option<String>,
}

/// Split `name(arg, arg, ...) = ret` respecting quotes, brackets and `<path>` annotations.
fn parse_call(line: &str) -> Option<Call> {
    let open = line.find('(')?;
    let name = &line[..open];
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    let bytes = line.as_bytes();
    let (mut depth, mut i, mut start) = (0usize, open + 1, open + 1);
    let mut args = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = skip_delimited(bytes, i, b'"'),
            b'<' if i > 0 && bytes[i - 1].is_ascii_alphanumeric() => i = skip_delimited(bytes, i, b'>'),
            b'(' | b'[' | b'{' => depth += 1,
            b')' if depth == 0 => {
                let arg = line[start..i].trim();
                if !arg.is_empty() {
                    args.push(arg.to_string());
                }
                break;
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                args.push(line[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let tail = line.get(i + 1..)?.trim_start();
    let ret_tok = tail.strip_prefix('=')?.split_whitespace().next()?;
    let (num, ret_path) = match ret_tok.split_once('<') {
        Some((n, p)) => (n, Some(unescape(p.strip_suffix('>').unwrap_or(p)))),
        None => (ret_tok, None),
    };
    let ret = match num.strip_prefix("0x") {
        Some(h) => i64::from_str_radix(h, 16).ok(),
        None => num.parse().ok(),
    };
    Some(Call { name: name.to_string(), args, ret, ret_path: ret_path.map(lossy) })
}

/// Index of the closing delimiter, honoring backslash escapes.
fn skip_delimited(b: &[u8], mut i: usize, close: u8) -> usize {
    i += 1;
    while i < b.len() && b[i] != close {
        if b[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    i
}

fn lossy(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

/// Decode C-style escapes as strace prints them.
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = b[i];
        i += 1;
        match c {
            b'x' => {
                let hex = s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
                out.push(hex.unwrap_or(b'x'));
                i += 2;
            }
            b'0'..=b'7' => {
                let mut v = (c - b'0') as u32;
                for _ in 0..2 {
                    match b.get(i) {
                        Some(d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'v' => out.push(0x0b),
            b'f' => out.push(0x0c),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            other => out.push(other),
        }
    }
    out
}

/// Every quoted string in an argument, in order, with its truncation flag.
fn strings(arg: &str) -> Vec<(Vec<u8>, bool)> {
    let b = arg.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            let end = skip_delimited(b, i, b'"');
            let body = &arg[i + 1..end.min(arg.len())];
            let truncated = arg[end.min(arg.len())..].starts_with("\"...");
            out.push((unescape(body), truncated));
            i = end;
        }
        i += 1;
    }
    out
}

/// `3</a/b>` -> (Some(3), Some("/a/b")); `AT_FDCWD</x>` -> (None, Some("/x")).
fn fd_arg(arg: &str) -> (Option<i64>, Option<String>) {
    match arg.split_once('<') {
        Some((n, p)) => (n.parse().ok(), Some(lossy(unescape(p.strip_suffix('>').unwrap_or(p))))),
        None => (arg.parse().ok(), None),
    }
}

struct Ofd {
    offset: u64,
    append: bool,
}

struct State {
    root: String,
    launch_cwd: String,
    ops: Vec<Op>,
    warnings: Vec<String>,
    warned: HashSet<String>,
    // Open file descriptions and per-process fd tables (threads share one).
    ofds: Vec<Ofd>,
    tables: Vec<HashMap<i64, usize>>,
    table_of: HashMap<u32, usize>,
    cwd: HashMap<u32, String>,
    // What exists under the root right now, relative paths.
    sizes: HashMap<String, u64>,
    dirs: HashSet<String>,
}

impl State {
    fn new(root: &Path, cwd: &Path, initial: &Tree) -> State {
        let mut sizes = HashMap::new();
        let mut dirs = HashSet::from([String::new()]);
        for (p, e) in &initial.entries {
            match e {
                Entry::Dir => {
                    dirs.insert(p.clone());
                }
                Entry::File(d) => {
                    sizes.insert(p.clone(), d.len() as u64);
                }
            }
        }
        State {
            root: slash(root),
            launch_cwd: slash(cwd),
            ops: Vec::new(),
            warnings: Vec::new(),
            warned: HashSet::new(),
            ofds: Vec::new(),
            tables: Vec::new(),
            table_of: HashMap::new(),
            cwd: HashMap::new(),
            sizes,
            dirs,
        }
    }

    fn warn(&mut self, msg: String) {
        if self.warned.insert(msg.clone()) {
            self.warnings.push(msg);
        }
    }

    fn table(&mut self, pid: u32) -> usize {
        if let Some(&t) = self.table_of.get(&pid) {
            return t;
        }
        self.tables.push(HashMap::new());
        self.table_of.insert(pid, self.tables.len() - 1);
        self.tables.len() - 1
    }

    fn ofd(&mut self, pid: u32, fd: i64, rel: &str) -> usize {
        let t = self.table(pid);
        if let Some(&o) = self.tables[t].get(&fd) {
            return o;
        }
        // An fd we never saw opened (inherited): assume it sits at end of file.
        let offset = self.sizes.get(rel).copied().unwrap_or(0);
        self.ofds.push(Ofd { offset, append: false });
        let o = self.ofds.len() - 1;
        self.tables[t].insert(fd, o);
        o
    }

    fn cwd(&self, pid: u32) -> &str {
        self.cwd.get(&pid).map_or(&self.launch_cwd, |c| c)
    }

    /// Absolute path for `path` relative to a dirfd argument (or the cwd).
    fn resolve(&self, pid: u32, dirfd: Option<&str>, path: &str) -> String {
        if path.starts_with('/') {
            return normalize(path);
        }
        let base = match dirfd.map(fd_arg) {
            Some((_, Some(p))) => p,
            _ => self.cwd(pid).to_string(),
        };
        normalize(&format!("{base}/{path}"))
    }

    /// Path relative to the root, if inside it.
    fn rel(&self, abs: &str) -> Option<String> {
        if abs.ends_with(" (deleted)") {
            return None;
        }
        let abs = normalize(abs);
        if abs == self.root {
            Some(String::new())
        } else {
            abs.strip_prefix(&format!("{}/", self.root.trim_end_matches('/'))).map(str::to_string)
        }
    }

    fn fd_rel(&self, arg: &str) -> Option<String> {
        fd_arg(arg).1.and_then(|p| self.rel(&p))
    }

    fn exists(&self, rel: &str) -> bool {
        self.sizes.contains_key(rel) || self.dirs.contains(rel)
    }

    fn move_tree(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let moved = |p: &str| -> Option<String> {
            if p == from {
                Some(to.to_string())
            } else {
                p.strip_prefix(&prefix).map(|r| format!("{to}/{r}"))
            }
        };
        // A rename replaces its target.
        self.sizes.remove(to);
        self.dirs.remove(to);
        let files: Vec<_> = self.sizes.keys().filter_map(|p| moved(p).map(|n| (p.clone(), n))).collect();
        for (old, new) in files {
            let s = self.sizes.remove(&old).unwrap();
            self.sizes.insert(new, s);
        }
        let dirs: Vec<_> = self.dirs.iter().filter_map(|p| moved(p).map(|n| (p.clone(), n))).collect();
        for (old, new) in dirs {
            self.dirs.remove(&old);
            self.dirs.insert(new);
        }
    }

    fn apply(&mut self, pid: u32, c: &Call) -> Result<(), String> {
        let Some(ret) = c.ret else { return Ok(()) };
        if ret < 0 {
            return Ok(()); // failed syscalls change nothing
        }
        let arg = |i: usize| c.args.get(i).map(String::as_str).unwrap_or("");
        match c.name.as_str() {
            "open" | "openat" | "openat2" | "creat" => {
                let (dirfd, path, flags) = match c.name.as_str() {
                    "open" => (None, arg(0), arg(1)),
                    "creat" => (None, arg(0), "O_CREAT|O_WRONLY|O_TRUNC"),
                    _ => (Some(arg(0)), arg(1), arg(2)),
                };
                let abs = match &c.ret_path {
                    Some(p) => normalize(p),
                    None => {
                        let p = strings(path).into_iter().next().map(|s| lossy(s.0)).unwrap_or_default();
                        self.resolve(pid, dirfd, &p)
                    }
                };
                let Some(rel) = self.rel(&abs) else {
                    return Ok(());
                };
                if flags.contains("O_TMPFILE") {
                    self.warn(format!("O_TMPFILE in {rel} is not modeled"));
                    return Ok(());
                }
                if flags.contains("O_CREAT") && !self.exists(&rel) {
                    self.ops.push(Op::Create { path: rel.clone() });
                    self.sizes.insert(rel.clone(), 0);
                } else if flags.contains("O_TRUNC") && self.sizes.get(&rel).is_some_and(|&s| s > 0) {
                    self.ops.push(Op::Truncate { path: rel.clone(), len: 0 });
                    self.sizes.insert(rel.clone(), 0);
                }
                let t = self.table(pid);
                self.ofds.push(Ofd { offset: 0, append: flags.contains("O_APPEND") });
                let o = self.ofds.len() - 1;
                self.tables[t].insert(ret, o);
            }
            "write" | "writev" | "pwrite64" | "pwritev" | "pwritev2" => {
                let (fd, fd_path) = fd_arg(arg(0));
                let mut data = Vec::new();
                for (s, truncated) in strings(arg(1)) {
                    if truncated {
                        return Err(format!("a write was truncated by strace (limit {MAX_WRITE} bytes)"));
                    }
                    data.extend_from_slice(&s);
                }
                let n = ret as usize;
                if data.len() < n {
                    return Err(format!("{}: captured {} of {n} bytes", c.name, data.len()));
                }
                data.truncate(n);
                let rel = fd_path.as_deref().and_then(|p| self.rel(p));
                let Some(rel) = rel else {
                    if fd == Some(1) {
                        self.ops.push(Op::Mark { text: lossy(data) });
                    }
                    return Ok(());
                };
                let offset = if c.name.starts_with('p') && !(c.name == "pwritev2" && arg(3) == "-1") {
                    let off = arg(3);
                    off.parse().map_err(|_| format!("bad offset `{off}`"))?
                } else {
                    let o = self.ofd(pid, fd.unwrap_or(-1), &rel);
                    let size = self.sizes.get(&rel).copied().unwrap_or(0);
                    let ofd = &mut self.ofds[o];
                    let at = if ofd.append { size } else { ofd.offset };
                    ofd.offset = at + n as u64;
                    at
                };
                let size = self.sizes.entry(rel.clone()).or_insert(0);
                *size = (*size).max(offset + n as u64);
                if n > 0 {
                    self.ops.push(Op::Write { path: rel, offset, data });
                }
            }
            "read" | "readv" => {
                if let (Some(fd), Some(p)) = fd_arg(arg(0))
                    && let Some(rel) = self.rel(&p)
                {
                    let o = self.ofd(pid, fd, &rel);
                    self.ofds[o].offset += ret as u64;
                }
            }
            "lseek" => {
                if let (Some(fd), Some(p)) = fd_arg(arg(0))
                    && let Some(rel) = self.rel(&p)
                {
                    let o = self.ofd(pid, fd, &rel);
                    self.ofds[o].offset = ret as u64;
                }
            }
            "close" => {
                if let (Some(fd), _) = fd_arg(arg(0)) {
                    let t = self.table(pid);
                    self.tables[t].remove(&fd);
                }
            }
            "dup" | "dup2" | "dup3" | "fcntl" | "fcntl64" => {
                if c.name.starts_with("fcntl") && !arg(1).starts_with("F_DUPFD") {
                    return Ok(());
                }
                if let (Some(fd), _) = fd_arg(arg(0)) {
                    let t = self.table(pid);
                    if let Some(&o) = self.tables[t].get(&fd) {
                        self.tables[t].insert(ret, o);
                    }
                }
            }
            "ftruncate" | "truncate" => {
                let rel = if c.name == "ftruncate" {
                    self.fd_rel(arg(0))
                } else {
                    let p = strings(arg(0)).into_iter().next().map(|s| lossy(s.0)).unwrap_or_default();
                    self.rel(&self.resolve(pid, None, &p))
                };
                if let Some(rel) = rel {
                    let len = arg(1).parse().map_err(|_| format!("bad length `{}`", arg(1)))?;
                    self.sizes.insert(rel.clone(), len);
                    self.ops.push(Op::Truncate { path: rel, len });
                }
            }
            "fsync" | "fdatasync" => {
                if let Some(rel) = self.fd_rel(arg(0)) {
                    self.ops.push(Op::Fsync { path: rel });
                }
            }
            "sync" | "syncfs" => self.ops.push(Op::Sync),
            "sync_file_range" | "msync" => {
                self.warn(format!("{} is not a durability barrier and was ignored", c.name));
            }
            "rename" | "renameat" | "renameat2" => {
                let (from, to) = if c.name == "rename" {
                    (self.path_arg(pid, None, arg(0)), self.path_arg(pid, None, arg(1)))
                } else {
                    (self.path_arg(pid, Some(arg(0)), arg(1)), self.path_arg(pid, Some(arg(2)), arg(3)))
                };
                if arg(4).contains("RENAME_EXCHANGE") {
                    self.warn("renameat2(RENAME_EXCHANGE) is not modeled".into());
                    return Ok(());
                }
                match (self.rel(&from), self.rel(&to)) {
                    (Some(f), Some(t)) => {
                        self.move_tree(&f, &t);
                        self.ops.push(Op::Rename { from: f, to: t });
                    }
                    (Some(f), None) => self.remove(f),
                    (None, Some(t)) => self
                        .warn(format!("{t} was moved in from outside the root; its content is not modeled")),
                    (None, None) => {}
                }
            }
            "unlink" | "unlinkat" | "rmdir" => {
                let abs = match c.name.as_str() {
                    "unlinkat" => self.path_arg(pid, Some(arg(0)), arg(1)),
                    _ => self.path_arg(pid, None, arg(0)),
                };
                if let Some(rel) = self.rel(&abs) {
                    self.remove(rel);
                }
            }
            "mkdir" | "mkdirat" => {
                let abs = match c.name.as_str() {
                    "mkdirat" => self.path_arg(pid, Some(arg(0)), arg(1)),
                    _ => self.path_arg(pid, None, arg(0)),
                };
                if let Some(rel) = self.rel(&abs) {
                    self.dirs.insert(rel.clone());
                    self.ops.push(Op::Mkdir { path: rel });
                }
            }
            "link" | "linkat" | "symlink" | "symlinkat" | "fallocate" | "copy_file_range" | "sendfile"
            | "sendfile64" | "splice"
                if c.args.iter().any(|a| self.arg_touches_root(pid, a)) =>
            {
                self.warn(format!("{} under the root is not modeled", c.name));
            }
            "mmap" | "mmap2"
                if arg(2).contains("PROT_WRITE")
                    && arg(3).contains("MAP_SHARED")
                    && self.fd_rel(arg(4)).is_some() =>
            {
                self.warn("writes through a shared writable mmap are invisible to unsynced".into());
            }
            "chdir" => {
                let abs = self.path_arg(pid, None, arg(0));
                self.cwd.insert(pid, abs);
            }
            "fchdir" => {
                if let (_, Some(p)) = fd_arg(arg(0)) {
                    self.cwd.insert(pid, normalize(&p));
                }
            }
            "clone" | "clone3" | "fork" | "vfork" => {
                let child = ret as u32;
                if child == 0 {
                    return Ok(());
                }
                let parent = self.table(pid);
                let cwd = self.cwd(pid).to_string();
                self.cwd.entry(child).or_insert(cwd);
                if c.args.iter().any(|a| a.contains("CLONE_FILES")) && !self.table_of.contains_key(&child) {
                    self.table_of.insert(child, parent);
                } else {
                    // Copy the table; the child may already have started (and opened things).
                    let copy = self.tables[parent].clone();
                    let t = self.table(child);
                    for (fd, o) in copy {
                        self.tables[t].entry(fd).or_insert(o);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn path_arg(&self, pid: u32, dirfd: Option<&str>, arg: &str) -> String {
        let p = strings(arg).into_iter().next().map(|s| lossy(s.0)).unwrap_or_default();
        self.resolve(pid, dirfd, &p)
    }

    fn arg_touches_root(&self, pid: u32, arg: &str) -> bool {
        self.fd_rel(arg).is_some()
            || (arg.starts_with('"') && self.rel(&self.path_arg(pid, None, arg)).is_some())
    }

    fn remove(&mut self, rel: String) {
        let prefix = format!("{rel}/");
        let is_dir = self.dirs.contains(&rel);
        self.sizes.retain(|p, _| p != &rel && !p.starts_with(&prefix));
        self.dirs.retain(|p| p != &rel && !p.starts_with(&prefix));
        self.ops.push(if is_dir { Op::Rmdir { path: rel } } else { Op::Unlink { path: rel } });
    }
}

fn slash(p: &Path) -> String {
    normalize(&p.to_string_lossy().replace('\\', "/"))
}

/// Lexically resolve `.`, `..` and repeated slashes in an absolute path.
fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let prefix = if p.starts_with('/') { "/" } else { "" };
    format!("{prefix}{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(log: &str) -> (Vec<Op>, Vec<String>) {
        let mut initial = Tree::default();
        initial.entries.insert("old".into(), Entry::File(b"12345".to_vec()));
        parse(log, Path::new("/r"), Path::new("/r"), &initial).unwrap()
    }

    #[test]
    fn parses_calls_with_tricky_strings() {
        let c = parse_call(r#"write(3</r/a,b (x)>, "\x29\x2c\x22", 3) = 3"#).unwrap();
        assert_eq!(c.args, vec!["3</r/a,b (x)>", r#""\x29\x2c\x22""#, "3"]);
        assert_eq!(strings(&c.args[1])[0].0, b"),\"");
        assert_eq!(fd_arg(&c.args[0]).1.unwrap(), "/r/a,b (x)");
        let c = parse_call(r#"openat(AT_FDCWD</r>, "\x6e", O_WRONLY|O_CREAT, 0644) = 3</r/n>"#).unwrap();
        assert_eq!((c.ret, c.ret_path.as_deref()), (Some(3), Some("/r/n")));
        let c = parse_call("unlink(\"/nope\") = -1 ENOENT (No such file or directory)").unwrap();
        assert_eq!(c.ret, Some(-1));
    }

    #[test]
    fn reconstructs_offsets_appends_dups_and_marks() {
        let log = r#"100 openat(AT_FDCWD</r>, "\x74\x6d\x70", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 3</r/tmp>
100 write(3</r/tmp>, "\x61\x62", 2) = 2
100 dup(3</r/tmp>) = 4</r/tmp>
100 write(4</r/tmp>, "\x63", 1 <unfinished ...>
100 <... write resumed>) = 1
100 fsync(3</r/tmp>) = 0
100 rename("\x2f\x72\x2f\x74\x6d\x70", "\x2f\x72\x2f\x6f\x6c\x64") = 0
100 openat(AT_FDCWD</r>, "\x6f\x6c\x64", O_WRONLY|O_APPEND) = 5</r/old>
100 write(5</r/old>, "\x7a", 1) = 1
100 pwrite64(5</r/old>, "\x51", 1, 0) = 1
100 write(1</dev/pts/0>, "\x6f\x6b\x0a", 3) = 3
100 +++ exited with 0 +++
"#;
        let (ops, warnings) = run(log);
        assert!(warnings.is_empty(), "{warnings:?}");
        let w = |p: &str, offset, d: &[u8]| Op::Write { path: p.into(), offset, data: d.to_vec() };
        assert_eq!(
            ops,
            vec![
                Op::Create { path: "tmp".into() },
                w("tmp", 0, b"ab"),
                w("tmp", 2, b"c"), // dup shares the offset
                Op::Fsync { path: "tmp".into() },
                Op::Rename { from: "tmp".into(), to: "old".into() },
                w("old", 3, b"z"), // O_APPEND lands at the (replaced) file's end
                w("old", 0, b"Q"),
                Op::Mark { text: "ok\n".into() },
            ]
        );
    }

    #[test]
    fn pwritev2_offset_minus_one_uses_and_advances_file_offset() {
        let log = r#"1 openat(AT_FDCWD</r>, "\x6e", O_WRONLY|O_CREAT, 0600) = 3</r/n>
1 pwritev2(3</r/n>, [{iov_base="\x61\x62", iov_len=2}], 1, -1, 0) = 2
1 pwritev2(3</r/n>, [{iov_base="\x63", iov_len=1}], 1, -1, 0) = 1
"#;
        let (ops, warnings) = run(log);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            ops,
            vec![
                Op::Create { path: "n".into() },
                Op::Write { path: "n".into(), offset: 0, data: b"ab".to_vec() },
                Op::Write { path: "n".into(), offset: 2, data: b"c".to_vec() },
            ]
        );
    }

    #[test]
    fn pwritev2_offset_minus_one_honors_append() {
        let log = r#"1 openat(AT_FDCWD</r>, "\x6f\x6c\x64", O_WRONLY|O_APPEND) = 3</r/old>
1 pwritev2(3</r/old>, [{iov_base="\x61", iov_len=1}], 1, -1, 0) = 1
"#;
        let (ops, warnings) = run(log);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(ops, vec![Op::Write { path: "old".into(), offset: 5, data: b"a".to_vec() }]);
    }

    #[test]
    fn pwritev2_rejects_other_invalid_offsets() {
        let log = r#"1 openat(AT_FDCWD</r>, "\x6f\x6c\x64", O_WRONLY) = 3</r/old>
1 pwritev2(3</r/old>, [{iov_base="\x61", iov_len=1}], 1, -2, 0) = 1
"#;
        let error = parse(log, Path::new("/r"), Path::new("/r"), &Tree::default()).unwrap_err();
        assert!(error.to_string().contains("bad offset `-2`"), "{error}");
    }

    #[test]
    fn forked_child_shares_offsets_and_threads_share_tables() {
        let log = r#"1 openat(AT_FDCWD</r>, "\x6f\x6c\x64", O_WRONLY) = 3</r/old>
1 clone(child_stack=NULL, flags=CLONE_CHILD_CLEARTID|SIGCHLD) = 2
2 write(3</r/old>, "\x41", 1) = 1
1 write(3</r/old>, "\x42", 1) = 1
1 clone(child_stack=0x1, flags=CLONE_VM|CLONE_FILES|CLONE_THREAD) = 3
3 openat(AT_FDCWD</r>, "\x6e", O_WRONLY|O_CREAT, 0600) = 7</r/n>
1 write(7</r/n>, "\x43", 1) = 1
"#;
        let (ops, _) = run(log);
        let offsets: Vec<u64> = ops
            .iter()
            .filter_map(|o| if let Op::Write { offset, .. } = o { Some(*offset) } else { None })
            .collect();
        assert_eq!(offsets, vec![0, 1, 0]);
    }
}
