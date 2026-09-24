//! Crash-state enumeration.
//!
//! A crash after the first `k` trace ops leaves on disk every micro-op made
//! durable by a barrier among those `k` ops, plus *some* subset of the rest.
//! The profile constrains which subsets are legal; we generate candidate
//! subsets with a fixed battery of strategies (exhaustive when small) and then
//! close them under the profile's rules.

use std::collections::HashMap;

use crate::model::{Micro, Profile, Program};
use crate::trace::Tree;

/// Knobs for how hard to search.
#[derive(Debug, Clone)]
pub struct Search {
    /// Enumerate every subset when at most this many micro-ops are in flight.
    pub exhaustive_limit: usize,
    /// Random subsets to add at each crash point beyond the exhaustive limit.
    pub samples: usize,
    pub seed: u64,
}

impl Default for Search {
    fn default() -> Self {
        Search { exhaustive_limit: 10, samples: 32, seed: 0x5eed }
    }
}

impl Program {
    /// Micro-ops (of the first `point` ops) that may or may not have persisted.
    pub(crate) fn in_flight(&self, point: usize) -> Vec<usize> {
        (0..self.op_end[point]).filter(|&j| self.micro[j].durable_at >= point).collect()
    }

    /// Candidate persisted-sets for a crash after `point` ops (not yet closed).
    pub(crate) fn candidates(&self, point: usize, search: &Search) -> Vec<Vec<bool>> {
        let n = self.op_end[point];
        let flight = self.in_flight(point);
        let base: Vec<bool> = (0..n).map(|j| self.micro[j].durable_at < point).collect();
        let with = |keep: &dyn Fn(usize, usize) -> bool| {
            let mut s = base.clone();
            for (pos, &j) in flight.iter().enumerate() {
                s[j] = keep(pos, j);
            }
            s
        };
        let m = flight.len();
        if m <= search.exhaustive_limit {
            return (0..1u64 << m).map(|mask| with(&|pos, _| mask >> pos & 1 == 1)).collect();
        }
        let mut out = vec![with(&|_, _| true), with(&|_, _| false)];
        for skip in 0..m {
            out.push(with(&|pos, _| pos != skip)); // lose exactly one micro-op
            out.push(with(&|pos, _| pos < skip)); // in-order persistence, cut at `skip`
        }
        let mut ops: Vec<usize> = flight.iter().map(|&j| self.micro[j].op).collect();
        ops.dedup();
        for op in ops {
            out.push(with(&|_, j| self.micro[j].op != op)); // lose one whole op
        }
        let mut rng = search.seed ^ (point as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for _ in 0..search.samples {
            let bits: Vec<bool> = (0..m).map(|_| splitmix(&mut rng) & 1 == 1).collect();
            out.push(with(&|pos, _| bits[pos]));
        }
        out
    }

    /// Extend `persisted` (length `op_end[point]`) to the smallest legal state.
    pub(crate) fn close(&self, point: usize, persisted: &mut [bool]) {
        for (j, p) in persisted.iter_mut().enumerate() {
            *p |= self.micro[j].durable_at < point;
        }
        if self.profile != Profile::Ext4Ordered {
            return;
        }
        // Journaled metadata commits in order: persisted metadata is a prefix.
        if let Some(last) = (0..persisted.len()).rev().find(|&j| persisted[j] && self.micro[j].m.is_meta()) {
            for (j, p) in persisted.iter_mut().enumerate().take(last) {
                *p |= self.micro[j].m.is_meta();
            }
        }
        // data=ordered: a size extension commits only after its data; a
        // replacing rename flushes the renamed file first (auto_da_alloc).
        let mut flush_before: HashMap<usize, usize> = HashMap::new();
        for (j, _) in persisted.iter().enumerate().filter(|(_, p)| **p) {
            match self.micro[j].m {
                Micro::SetLen { ino, keep_tail: true, .. } | Micro::Rename { ino, replaces: true, .. } => {
                    flush_before.insert(ino, j);
                }
                _ => {}
            }
        }
        for (j, p) in persisted.iter_mut().enumerate() {
            if let Micro::Write { ino, .. } = self.micro[j].m
                && flush_before.get(&ino).is_some_and(|&until| j < until)
            {
                *p = true;
            }
        }
    }

    /// The directory tree a crash state leaves behind.
    pub(crate) fn materialize(&self, persisted: &[bool]) -> Tree {
        let mut fs = self.base.clone();
        for (j, _) in persisted.iter().enumerate().filter(|(_, p)| **p) {
            fs.apply(&self.micro[j].m);
        }
        fs.to_tree()
    }
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::compile;
    use crate::trace::{Entry, Op, Trace};

    fn file(t: &Tree, p: &str) -> Option<String> {
        match t.entries.get(p) {
            Some(Entry::File(d)) => Some(String::from_utf8_lossy(d).into_owned()),
            _ => None,
        }
    }

    /// Replace-via-rename without fsync: the classic zero-length-file bug.
    fn replace_trace() -> Trace {
        let mut initial = Tree::default();
        initial.entries.insert("cfg".into(), Entry::File(b"old".to_vec()));
        Trace {
            initial,
            ops: vec![
                Op::Create { path: "tmp".into() },
                Op::Write { path: "tmp".into(), offset: 0, data: b"new".to_vec() },
                Op::Rename { from: "tmp".into(), to: "cfg".into() },
            ],
        }
    }

    fn states(profile: Profile) -> Vec<Option<String>> {
        let p = compile(&replace_trace(), profile, 4096).unwrap();
        let mut seen: Vec<Option<String>> = Vec::new();
        for point in 0..p.op_end.len() {
            for mut c in p.candidates(point, &Search::default()) {
                p.close(point, &mut c);
                let s = file(&p.materialize(&c), "cfg");
                if !seen.contains(&s) {
                    seen.push(s);
                }
            }
        }
        seen.sort();
        seen
    }

    #[test]
    fn posix_exposes_empty_and_zeroed_file_after_rename() {
        let s = states(Profile::Posix);
        assert!(s.contains(&Some(String::new())), "{s:?}"); // rename persisted, data+size lost
        assert!(s.contains(&Some("\0\0\0".into())), "{s:?}"); // size persisted, data lost
        assert!(s.contains(&Some("new".into())));
        assert!(s.contains(&Some("old".into())));
    }

    #[test]
    fn ext4_auto_da_alloc_makes_replace_safe() {
        assert_eq!(states(Profile::Ext4Ordered), vec![Some("new".into()), Some("old".into())]);
    }
}
