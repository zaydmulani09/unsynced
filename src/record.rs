//! In-process recording: perform file operations through a [`Recorder`] and it
//! both does them for real and logs them as a [`Trace`]. Works on every OS and
//! needs no tracer, which makes it the natural harness for Rust storage code.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use crate::trace::{Op, Trace, Tree};

/// Records file operations under a root directory.
///
/// ```no_run
/// # fn main() -> std::io::Result<()> {
/// use std::io::Write;
/// let rec = unsynced::Recorder::new("/tmp/db")?;
/// let mut f = rec.create("data.tmp")?;
/// f.write_all(b"hello")?;
/// f.sync_all()?;
/// rec.rename("data.tmp", "data")?;
/// rec.sync_dir("")?;
/// rec.mark("saved");
/// let trace = rec.finish();
/// # Ok(()) }
/// ```
pub struct Recorder {
    root: PathBuf,
    initial: Tree,
    ops: Mutex<Vec<Op>>,
}

impl Recorder {
    /// Start recording under `root` (created if missing), snapshotting its contents.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Recorder> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let initial = Tree::load(&root)?;
        Ok(Recorder { root, initial, ops: Mutex::new(Vec::new()) })
    }

    fn log(&self, op: Op) {
        self.ops.lock().unwrap().push(op);
    }

    fn rel(&self, path: impl AsRef<Path>) -> io::Result<String> {
        let mut parts = Vec::new();
        for c in path.as_ref().components() {
            match c {
                Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
                Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{}: paths must be relative to the recorder root", path.as_ref().display()),
                    ));
                }
            }
        }
        Ok(parts.join("/"))
    }

    /// Like [`File::create`]: create or truncate, open for writing.
    pub fn create(&self, path: impl AsRef<Path>) -> io::Result<RecFile<'_>> {
        let rel = self.rel(&path)?;
        let full = self.root.join(&path);
        match fs::metadata(&full) {
            Ok(m) if m.len() > 0 => self.log(Op::Truncate { path: rel.clone(), len: 0 }),
            Ok(_) => {}
            Err(_) => self.log(Op::Create { path: rel.clone() }),
        }
        let file = File::create(&full)?;
        Ok(RecFile { rec: self, path: rel, file, pos: 0, len: 0, append: false })
    }

    /// Open an existing file for reading and writing, positioned at the start.
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<RecFile<'_>> {
        let rel = self.rel(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(self.root.join(&path))?;
        let len = file.metadata()?.len();
        Ok(RecFile { rec: self, path: rel, file, pos: 0, len, append: false })
    }

    /// Open for appending, creating the file if needed.
    pub fn append(&self, path: impl AsRef<Path>) -> io::Result<RecFile<'_>> {
        let rel = self.rel(&path)?;
        let full = self.root.join(&path);
        if !full.exists() {
            self.log(Op::Create { path: rel.clone() });
        }
        let file = OpenOptions::new().read(true).append(true).create(true).open(&full)?;
        let len = file.metadata()?.len();
        Ok(RecFile { rec: self, path: rel, file, pos: len, len, append: true })
    }

    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
        let (f, t) = (self.rel(&from)?, self.rel(&to)?);
        fs::rename(self.root.join(from), self.root.join(to))?;
        self.log(Op::Rename { from: f, to: t });
        Ok(())
    }

    pub fn remove_file(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let rel = self.rel(&path)?;
        fs::remove_file(self.root.join(path))?;
        self.log(Op::Unlink { path: rel });
        Ok(())
    }

    pub fn create_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let rel = self.rel(&path)?;
        fs::create_dir(self.root.join(path))?;
        self.log(Op::Mkdir { path: rel });
        Ok(())
    }

    pub fn remove_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let rel = self.rel(&path)?;
        fs::remove_dir(self.root.join(path))?;
        self.log(Op::Rmdir { path: rel });
        Ok(())
    }

    /// fsync a directory (`""` is the root), making its entries durable.
    pub fn sync_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let rel = self.rel(&path)?;
        // Windows cannot open a directory for fsync; the trace is what matters.
        #[cfg(unix)]
        File::open(self.root.join(&path))?.sync_all()?;
        self.log(Op::Fsync { path: rel });
        Ok(())
    }

    /// Record that the workload acknowledged something (e.g. "committed 42").
    pub fn mark(&self, text: impl Into<String>) {
        self.log(Op::Mark { text: text.into() });
    }

    /// Stop recording.
    pub fn finish(self) -> Trace {
        Trace { initial: self.initial, ops: self.ops.into_inner().unwrap() }
    }
}

/// A file opened through a [`Recorder`]. Writes, syncs and resizes are logged.
///
/// The recorded path is the one it was opened under; renaming a file and then
/// writing through an old handle is not tracked.
pub struct RecFile<'r> {
    rec: &'r Recorder,
    path: String,
    file: File,
    pos: u64,
    len: u64,
    append: bool,
}

impl RecFile<'_> {
    pub fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()?;
        self.rec.log(Op::Fsync { path: self.path.clone() });
        Ok(())
    }

    /// Treated like `sync_all`: the model does not distinguish `fdatasync`.
    pub fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()?;
        self.rec.log(Op::Fsync { path: self.path.clone() });
        Ok(())
    }

    pub fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)?;
        self.len = len;
        self.rec.log(Op::Truncate { path: self.path.clone(), len });
        Ok(())
    }
}

impl Write for RecFile<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        let offset = if self.append { self.len } else { self.pos };
        self.rec.log(Op::Write { path: self.path.clone(), offset, data: buf[..n].to_vec() });
        self.pos = offset + n as u64;
        self.len = self.len.max(self.pos);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for RecFile<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.pos = self.file.seek(to)?;
        Ok(self.pos)
    }
}
