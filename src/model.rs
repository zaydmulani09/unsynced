//! Compiles a [`Trace`] into *micro-ops*: the units a file system may persist
//! independently of each other (a directory-entry change, one block of a
//! write, a size change). Each micro-op records the barrier (an `fsync`/`sync`)
//! that made it durable, if any. Which subsets of the non-durable micro-ops may
//! survive a crash is decided by the [`Profile`] (see `crash.rs`).

use std::collections::{BTreeMap, HashMap};

use crate::Error;
use crate::trace::{Entry, Op, Trace, Tree, join};

pub(crate) type Ino = usize;
pub(crate) const NEVER: usize = usize::MAX;

/// How weak the file system's persistence guarantees are assumed to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// The weakest behavior POSIX permits, and what portable code must survive:
    /// any non-durable micro-op may be lost independently of any other, writes
    /// tear at block boundaries, a file's size can persist without its data,
    /// and `fsync` on a file does not make its directory entry durable.
    #[default]
    Posix,
    /// Linux ext4 with the default `data=ordered` journaling: metadata changes
    /// persist in program order, a size extension never persists before its
    /// data, `fsync` of anything commits all earlier metadata, and a rename that
    /// replaces a file first flushes the renamed file's data (`auto_da_alloc`).
    Ext4Ordered,
}

impl std::str::FromStr for Profile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "posix" => Ok(Profile::Posix),
            "ext4" | "ext4-ordered" => Ok(Profile::Ext4Ordered),
            _ => Err(format!("unknown profile `{s}` (expected `posix` or `ext4`)")),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Micro {
    Link { parent: Ino, name: String, ino: Ino },
    Unlink { parent: Ino, name: String },
    Rename { from: Ino, from_name: String, to: Ino, to_name: String, ino: Ino, replaces: bool },
    Write { ino: Ino, offset: u64, data: Vec<u8> },
    /// `keep_tail`: a size extension caused by a write (bytes already written
    /// past the old end become visible) as opposed to a truncate (zero-fill).
    SetLen { ino: Ino, len: u64, keep_tail: bool },
}

impl Micro {
    pub(crate) fn is_meta(&self) -> bool {
        !matches!(self, Micro::Write { .. })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MicroOp {
    pub m: Micro,
    /// Index of the trace op that produced it.
    pub op: usize,
    /// Index of the trace op (a barrier) after which it is durable, or NEVER.
    pub durable_at: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum Node {
    Dir(BTreeMap<String, Ino>),
    /// `buf` may run past `len`: bytes written whose size update did not persist.
    File { buf: Vec<u8>, len: u64 },
}

/// An inode-level file system image. Inode 0 is the root directory.
#[derive(Debug, Clone)]
pub(crate) struct Fs {
    pub nodes: Vec<Node>,
}

impl Fs {
    pub(crate) fn from_tree(tree: &Tree) -> Fs {
        let mut fs = Fs { nodes: vec![Node::Dir(BTreeMap::new())] };
        // BTreeMap order puts every directory before its children.
        for (path, entry) in &tree.entries {
            let (parent, name) = split(path);
            let parent = fs.lookup(parent).expect("snapshot parent precedes child");
            let node = match entry {
                Entry::Dir => Node::Dir(BTreeMap::new()),
                Entry::File(d) => Node::File { buf: d.clone(), len: d.len() as u64 },
            };
            let ino = fs.nodes.len();
            fs.nodes.push(node);
            fs.dir_mut(parent).unwrap().insert(name.to_string(), ino);
        }
        fs
    }

    pub(crate) fn lookup(&self, path: &str) -> Option<Ino> {
        let mut ino = 0;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            match &self.nodes[ino] {
                Node::Dir(entries) => ino = *entries.get(part)?,
                Node::File { .. } => return None,
            }
        }
        Some(ino)
    }

    fn dir_mut(&mut self, ino: Ino) -> Option<&mut BTreeMap<String, Ino>> {
        match &mut self.nodes[ino] {
            Node::Dir(e) => Some(e),
            Node::File { .. } => None,
        }
    }

    fn file_len(&self, ino: Ino) -> Option<u64> {
        match &self.nodes[ino] {
            Node::File { len, .. } => Some(*len),
            Node::Dir(_) => None,
        }
    }

    pub(crate) fn apply(&mut self, m: &Micro) {
        match m {
            Micro::Link { parent, name, ino } => {
                if let Some(d) = self.dir_mut(*parent) {
                    d.insert(name.clone(), *ino);
                }
            }
            Micro::Unlink { parent, name } => {
                if let Some(d) = self.dir_mut(*parent) {
                    d.remove(name);
                }
            }
            Micro::Rename { from, from_name, to, to_name, ino, .. } => {
                if let Some(d) = self.dir_mut(*from)
                    && d.get(from_name) == Some(ino)
                {
                    d.remove(from_name);
                }
                if let Some(d) = self.dir_mut(*to) {
                    d.insert(to_name.clone(), *ino);
                }
            }
            Micro::Write { ino, offset, data } => {
                if let Node::File { buf, .. } = &mut self.nodes[*ino] {
                    let (start, end) = (*offset as usize, *offset as usize + data.len());
                    if buf.len() < end {
                        buf.resize(end, 0);
                    }
                    buf[start..end].copy_from_slice(data);
                }
            }
            Micro::SetLen { ino, len: new, keep_tail } => {
                if let Node::File { buf, len } = &mut self.nodes[*ino] {
                    // Shrinking discards data; a truncate-extension exposes zeros.
                    if *new < *len || !keep_tail {
                        buf.truncate((*new).min(*len) as usize);
                    }
                    *len = *new;
                }
            }
        }
    }

    /// The visible tree reachable from the root.
    pub(crate) fn to_tree(&self) -> Tree {
        let mut tree = Tree::default();
        let mut seen = vec![false; self.nodes.len()];
        self.walk(0, "", &mut tree, &mut seen);
        tree
    }

    fn walk(&self, ino: Ino, prefix: &str, tree: &mut Tree, seen: &mut [bool]) {
        if std::mem::replace(&mut seen[ino], true) {
            return; // a crash state can make a directory reachable twice
        }
        let Node::Dir(entries) = &self.nodes[ino] else { return };
        for (name, &child) in entries {
            let path = join(prefix, name);
            match &self.nodes[child] {
                Node::Dir(_) => {
                    tree.entries.insert(path.clone(), Entry::Dir);
                    self.walk(child, &path, tree, seen);
                }
                Node::File { buf, len } => {
                    let mut data = buf.clone();
                    data.resize(*len as usize, 0);
                    tree.entries.insert(path, Entry::File(data));
                }
            }
        }
    }
}

pub(crate) fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// A compiled trace, ready for crash-state enumeration.
pub(crate) struct Program {
    /// The initial image plus an empty node for every inode the trace creates.
    pub base: Fs,
    pub micro: Vec<MicroOp>,
    /// `op_end[k]` = number of micro-ops produced by the first `k` trace ops.
    pub op_end: Vec<usize>,
    pub profile: Profile,
    /// `marks_before[k]` = number of marks among the first `k` trace ops.
    pub marks_before: Vec<usize>,
    pub marks: Vec<String>,
}

pub(crate) fn compile(trace: &Trace, profile: Profile, block: u64) -> Result<Program, Error> {
    let block = block.max(1);
    let mut base = Fs::from_tree(&trace.initial);
    let mut v = base.clone(); // volatile view: every op applied
    let mut micro: Vec<MicroOp> = Vec::new();
    let mut op_end = vec![0];
    let mut marks_before = vec![0];
    let mut marks = Vec::new();
    // Not-yet-durable micro-ops, by what an fsync would flush.
    let mut pending_data: HashMap<Ino, Vec<usize>> = HashMap::new();
    let mut pending_dir: HashMap<Ino, Vec<usize>> = HashMap::new();
    let mut pending_meta: Vec<usize> = Vec::new();

    for (i, op) in trace.ops.iter().enumerate() {
        let err = |msg: String| Error::Model { op: i, msg: format!("{op}: {msg}") };
        let start = micro.len();
        let mut emit = |m: Micro, v: &mut Fs| {
            v.apply(&m);
            micro.push(MicroOp { m, op: i, durable_at: NEVER });
        };
        match op {
            Op::Create { path } | Op::Mkdir { path } => {
                let dir = matches!(op, Op::Mkdir { .. });
                if v.lookup(path).is_none() {
                    let (parent, name) = split(path);
                    let parent = v
                        .lookup(parent)
                        .filter(|&p| matches!(v.nodes[p], Node::Dir(_)))
                        .ok_or_else(|| err("parent directory does not exist".into()))?;
                    let node = if dir {
                        Node::Dir(BTreeMap::new())
                    } else {
                        Node::File { buf: Vec::new(), len: 0 }
                    };
                    let ino = v.nodes.len();
                    v.nodes.push(node.clone());
                    base.nodes.push(node);
                    emit(Micro::Link { parent, name: name.into(), ino }, &mut v);
                }
            }
            Op::Write { path, offset, data } => {
                let ino = v.lookup(path).ok_or_else(|| err("no such file".into()))?;
                let len = v.file_len(ino).ok_or_else(|| err("is a directory".into()))?;
                let end = offset + data.len() as u64;
                let mut at = *offset;
                while at < end {
                    let next = ((at / block + 1) * block).min(end);
                    let chunk = data[(at - offset) as usize..(next - offset) as usize].to_vec();
                    emit(Micro::Write { ino, offset: at, data: chunk }, &mut v);
                    at = next;
                }
                if end > len {
                    emit(Micro::SetLen { ino, len: end, keep_tail: true }, &mut v);
                }
            }
            Op::Truncate { path, len } => {
                let ino = v.lookup(path).ok_or_else(|| err("no such file".into()))?;
                v.file_len(ino).ok_or_else(|| err("is a directory".into()))?;
                emit(Micro::SetLen { ino, len: *len, keep_tail: false }, &mut v);
            }
            Op::Rename { from, to } => {
                let ino = v.lookup(from).ok_or_else(|| err("no such file".into()))?;
                if from == to || v.lookup(to) == Some(ino) {
                    // POSIX: renaming a file onto itself does nothing.
                } else {
                    if to.starts_with(&format!("{from}/")) {
                        return Err(err("cannot move a directory into itself".into()));
                    }
                    let (fp, fname) = split(from);
                    let (tp, tname) = split(to);
                    let fp = v.lookup(fp).unwrap();
                    let tp = v
                        .lookup(tp)
                        .filter(|&p| matches!(v.nodes[p], Node::Dir(_)))
                        .ok_or_else(|| err("target directory does not exist".into()))?;
                    let replaces = v.lookup(to).is_some();
                    let m = Micro::Rename {
                        from: fp,
                        from_name: fname.into(),
                        to: tp,
                        to_name: tname.into(),
                        ino,
                        replaces,
                    };
                    emit(m, &mut v);
                }
            }
            Op::Unlink { path } | Op::Rmdir { path } => {
                v.lookup(path).ok_or_else(|| err("no such file".into()))?;
                let (parent, name) = split(path);
                let parent = v.lookup(parent).unwrap();
                emit(Micro::Unlink { parent, name: name.into() }, &mut v);
            }
            Op::Fsync { .. } | Op::Sync => {}
            Op::Mark { text } => marks.push(text.clone()),
        }

        // Register the new micro-ops with what would flush them.
        for j in start..micro.len() {
            match &micro[j].m {
                Micro::Write { ino, .. } | Micro::SetLen { ino, .. } => {
                    pending_data.entry(*ino).or_default().push(j)
                }
                Micro::Link { parent, .. } | Micro::Unlink { parent, .. } => {
                    pending_dir.entry(*parent).or_default().push(j)
                }
                // POSIX only promises a rename is durable once the target directory is synced.
                Micro::Rename { to, .. } => pending_dir.entry(*to).or_default().push(j),
            }
            if micro[j].m.is_meta() {
                pending_meta.push(j);
            }
        }

        // Barriers.
        let flush = |list: Vec<usize>, micro: &mut Vec<MicroOp>| {
            for j in list {
                if micro[j].durable_at == NEVER {
                    micro[j].durable_at = i;
                }
            }
        };
        match op {
            Op::Fsync { path } => {
                let ino = v.lookup(path).ok_or_else(|| err("no such file".into()))?;
                flush(pending_data.remove(&ino).unwrap_or_default(), &mut micro);
                flush(pending_dir.remove(&ino).unwrap_or_default(), &mut micro);
                if profile == Profile::Ext4Ordered {
                    // One journal: committing it for any fsync commits all prior metadata.
                    flush(std::mem::take(&mut pending_meta), &mut micro);
                }
            }
            Op::Sync => {
                let all = (0..micro.len()).collect();
                flush(all, &mut micro);
                pending_data.clear();
                pending_dir.clear();
                pending_meta.clear();
            }
            _ => {}
        }
        op_end.push(micro.len());
        marks_before.push(marks.len());
    }
    Ok(Program { base, micro, op_end, profile, marks_before, marks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> Tree {
        let mut t = Tree::default();
        for (p, d) in files {
            let e = if d.ends_with('/') { Entry::Dir } else { Entry::File(d.as_bytes().to_vec()) };
            t.entries.insert(p.to_string(), e);
        }
        t
    }

    #[test]
    fn volatile_replay_matches_ops() {
        let trace = Trace {
            initial: tree(&[("d", "/"), ("d/old", "hello")]),
            ops: vec![
                Op::Create { path: "d/new".into() },
                Op::Write { path: "d/new".into(), offset: 0, data: b"abcdefghij".to_vec() },
                Op::Rename { from: "d/new".into(), to: "d/old".into() },
                Op::Truncate { path: "d/old".into(), len: 4 },
            ],
        };
        let p = compile(&trace, Profile::Posix, 4).unwrap();
        // 10 bytes over 4-byte blocks = 3 chunks + a size extension.
        assert_eq!(p.op_end, vec![0, 1, 5, 6, 7]);
        let mut fs = p.base.clone();
        p.micro.iter().for_each(|m| fs.apply(&m.m));
        assert_eq!(fs.to_tree(), tree(&[("d", "/"), ("d/old", "abcd")]));
    }

    #[test]
    fn size_without_data_reads_zeros() {
        let trace = Trace {
            initial: tree(&[("f", "")]),
            ops: vec![Op::Write { path: "f".into(), offset: 0, data: b"xy".to_vec() }],
        };
        let p = compile(&trace, Profile::Posix, 4096).unwrap();
        let mut fs = p.base.clone();
        fs.apply(&p.micro[1].m); // SetLen only
        assert_eq!(fs.to_tree(), tree(&[("f", "\0\0")]));
    }

    #[test]
    fn fsync_file_does_not_cover_its_dir_entry_on_posix() {
        let trace = Trace {
            initial: Tree::default(),
            ops: vec![
                Op::Create { path: "f".into() },
                Op::Write { path: "f".into(), offset: 0, data: b"x".to_vec() },
                Op::Fsync { path: "f".into() },
            ],
        };
        let posix = compile(&trace, Profile::Posix, 4096).unwrap();
        let d: Vec<_> = posix.micro.iter().map(|m| m.durable_at).collect();
        assert_eq!(d, vec![NEVER, 2, 2]);
        let ext4 = compile(&trace, Profile::Ext4Ordered, 4096).unwrap();
        let d: Vec<_> = ext4.micro.iter().map(|m| m.durable_at).collect();
        assert_eq!(d, vec![2, 2, 2]);
    }
}
