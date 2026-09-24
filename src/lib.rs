//! # unsynced
//!
//! Finds crash-consistency bugs: the ways a program's on-disk state can be
//! left broken by a power loss at the wrong moment.
//!
//! 1. **Record** what a workload does to a directory — in-process with
//!    [`Recorder`], or for any program on Linux via [`strace::record`].
//! 2. **Compile** the trace into micro-ops a file system may persist
//!    independently, each tagged with the `fsync` that made it durable.
//! 3. **Enumerate** the disk states a crash can leave behind under a
//!    persistence [`Profile`]: lost, reordered and torn writes, lost
//!    directory entries, sizes that outlive their data.
//! 4. **Check** each unique state with your checker (a closure or a command),
//!    then **minimize** each failure to the operations whose loss causes it
//!    and explain the fix.
//!
//! ```no_run
//! # fn main() -> Result<(), unsynced::Error> {
//! use std::io::Write;
//! let rec = unsynced::Recorder::new("/tmp/app")?;
//! let mut f = rec.create("config.tmp")?;
//! f.write_all(b"v2")?;
//! rec.rename("config.tmp", "config")?;
//! rec.mark("saved v2");
//! let trace = rec.finish();
//!
//! let report = unsynced::check(&trace, &unsynced::Options::default(), |crash| {
//!     let got = std::fs::read(crash.dir.join("config")).unwrap_or_default();
//!     let saved = crash.marks.iter().any(|m| m == "saved v2");
//!     match (saved, got.as_slice()) {
//!         (true, b"v2") | (false, b"v2" | b"") => Ok(()),
//!         _ => Err(format!("config = {got:?}")),
//!     }
//! })?;
//! println!("{report}");
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

mod check;
mod crash;
mod model;
mod record;
pub mod strace;
mod trace;

pub use check::{Crash, Kind, OpRef, Options, Report, Vulnerability, check};
pub use crash::Search;
pub use model::Profile;
pub use record::{RecFile, Recorder};
pub use trace::{Entry, Op, Trace, Tree};

/// Errors from loading, compiling or checking a trace.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A malformed trace file.
    Trace(String),
    /// A trace op that cannot apply to the state before it (e.g. writing a
    /// file that does not exist).
    Model {
        op: usize,
        msg: String,
    },
    /// Recording with strace failed or its output could not be understood.
    Strace(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Trace(m) | Error::Strace(m) => f.write_str(m),
            Error::Model { op, msg } => write!(f, "trace op #{op} ({msg})"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
