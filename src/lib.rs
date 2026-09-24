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

#![forbid(unsafe_code)]
#![allow(dead_code)]

mod trace;

pub use trace::{Entry, Op, Trace, Tree};

/// Errors from loading, compiling or checking a trace.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A malformed trace file.
    Trace(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Trace(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
