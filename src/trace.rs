//! The trace format: an initial directory snapshot plus the ordered list of
//! file-system operations a workload performed on it.
//!
//! Paths are relative to the traced root, `/`-separated; `""` is the root itself.
//! On disk a trace is a *bundle* directory: `initial/` (the snapshot) and
//! `trace.jsonl` (one [`Op`] per line). Any language can emit this format.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Error;

/// One file-system operation, as the workload issued it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Create a new, empty regular file.
    Create { path: String },
    /// Create a directory.
    Mkdir { path: String },
    /// Write `data` at `offset` (appends are writes at the old end of file).
    Write {
        path: String,
        offset: u64,
        #[serde(with = "hex")]
        data: Vec<u8>,
    },
    /// Set a file's length (`truncate`/`ftruncate`/`set_len`).
    Truncate { path: String, len: u64 },
    /// Atomically rename `from` to `to`, replacing `to` if it exists.
    Rename { from: String, to: String },
    /// Remove a file's directory entry.
    Unlink { path: String },
    /// Remove an empty directory.
    Rmdir { path: String },
    /// `fsync`/`fdatasync` a file or a directory.
    Fsync { path: String },
    /// `sync()`: flush everything.
    Sync,
    /// Not a disk operation: something the workload told the outside world
    /// (by default, its stdout). Checkers see the marks emitted before the crash,
    /// which is how they know what was acknowledged as durable.
    Mark { text: String },
}

impl Op {
    /// The path this op is about, if any (the source path for renames).
    pub fn path(&self) -> Option<&str> {
        match self {
            Op::Create { path }
            | Op::Mkdir { path }
            | Op::Write { path, .. }
            | Op::Truncate { path, .. }
            | Op::Unlink { path }
            | Op::Rmdir { path }
            | Op::Fsync { path } => Some(path),
            Op::Rename { from, .. } => Some(from),
            Op::Sync | Op::Mark { .. } => None,
        }
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let p = |s: &str| if s.is_empty() { ".".to_string() } else { s.to_string() };
        match self {
            Op::Create { path } => write!(f, "create {}", p(path)),
            Op::Mkdir { path } => write!(f, "mkdir {}", p(path)),
            Op::Write { path, offset, data } => {
                write!(f, "write {} [{}..{})", p(path), offset, offset + data.len() as u64)
            }
            Op::Truncate { path, len } => write!(f, "truncate {} to {len}", p(path)),
            Op::Rename { from, to } => write!(f, "rename {} -> {}", p(from), p(to)),
            Op::Unlink { path } => write!(f, "unlink {}", p(path)),
            Op::Rmdir { path } => write!(f, "rmdir {}", p(path)),
            Op::Fsync { path } => write!(f, "fsync {}", p(path)),
            Op::Sync => write!(f, "sync"),
            Op::Mark { text } => write!(f, "mark {:?}", text.trim_end()),
        }
    }
}

/// A directory tree held in memory: relative path -> entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    pub entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Dir,
    File(Vec<u8>),
}

impl Tree {
    /// Snapshot a directory (regular files and directories; symlinks are skipped).
    pub fn load(root: &Path) -> io::Result<Tree> {
        let mut tree = Tree::default();
        if root.exists() {
            load_into(root, "", &mut tree)?;
        }
        Ok(tree)
    }

    /// Materialize into `root`, which must be empty or absent.
    pub fn write_to(&self, root: &Path) -> io::Result<()> {
        fs::create_dir_all(root)?;
        for (path, entry) in &self.entries {
            let dst = root.join(path);
            match entry {
                Entry::Dir => fs::create_dir_all(&dst)?,
                Entry::File(data) => fs::write(&dst, data)?,
            }
        }
        Ok(())
    }
}

fn load_into(dir: &Path, prefix: &str, tree: &mut Tree) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = join(prefix, &name);
        let ty = entry.file_type()?;
        if ty.is_dir() {
            tree.entries.insert(rel.clone(), Entry::Dir);
            load_into(&entry.path(), &rel, tree)?;
        } else if ty.is_file() {
            tree.entries.insert(rel, Entry::File(fs::read(entry.path())?));
        }
    }
    Ok(())
}

pub(crate) fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") }
}

/// A recorded workload: where it started and what it did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trace {
    pub initial: Tree,
    pub ops: Vec<Op>,
}

impl Trace {
    /// Write as a bundle directory (`initial/` + `trace.jsonl`). Overwrites.
    pub fn save(&self, bundle: &Path) -> io::Result<()> {
        let initial = bundle.join("initial");
        if initial.exists() {
            fs::remove_dir_all(&initial)?;
        }
        self.initial.write_to(&initial)?;
        let mut out = io::BufWriter::new(fs::File::create(bundle.join("trace.jsonl"))?);
        for op in &self.ops {
            serde_json::to_writer(&mut out, op)?;
            out.write_all(b"\n")?;
        }
        out.flush()
    }

    /// Load a bundle written by [`Trace::save`] or `unsynced record`.
    pub fn load(bundle: &Path) -> Result<Trace, Error> {
        let initial = Tree::load(&bundle.join("initial"))?;
        let file = fs::File::open(bundle.join("trace.jsonl"))?;
        let mut ops = Vec::new();
        for (i, line) in io::BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let op = serde_json::from_str(&line)
                .map_err(|e| Error::Trace(format!("trace.jsonl line {}: {e}", i + 1)))?;
            ops.push(op);
        }
        Ok(Trace { initial, ops })
    }
}

/// Byte payloads are hex strings in JSON: compact enough, trivially portable.
mod hex {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(data.len() * 2);
        for b in data {
            out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            out.push(char::from_digit((b & 15) as u32, 16).unwrap());
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() % 2 != 0 {
            return Err(D::Error::custom("odd-length hex string"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(D::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_json_roundtrip() {
        let op = Op::Write { path: "a/b".into(), offset: 7, data: vec![0, 255, 16] };
        let s = serde_json::to_string(&op).unwrap();
        assert_eq!(s, r#"{"op":"write","path":"a/b","offset":7,"data":"00ff10"}"#);
        assert_eq!(serde_json::from_str::<Op>(&s).unwrap(), op);
        assert_eq!(serde_json::from_str::<Op>(r#"{"op":"sync"}"#).unwrap(), Op::Sync);
    }
}
