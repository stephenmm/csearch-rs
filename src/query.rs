// Copyright (c) 2011 The Go Authors. All rights reserved.
// Copyright (c) 2026 The csearch-rs Authors. All rights reserved.
//
// Derived from Russ Cox's Go implementation at
// https://github.com/google/codesearch. Use of this source code is governed
// by a BSD-style licence; see the LICENSE file at the repository root.

//! Boolean trigram queries — a port of `index/regexp.go`'s `Query` type.
//!
//! A query is `All`, `None`, or an `And`/`Or` node holding a sorted list of
//! trigrams plus a list of sub-queries. `And` means every trigram and every
//! sub-query must match; `Or` means any of them.

use crate::trigram;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    All,
    None,
    And,
    Or,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub op: Op,
    /// Sorted, deduplicated packed trigrams.
    pub trigrams: Vec<u32>,
    pub subs: Vec<Query>,
}

impl Default for Query {
    fn default() -> Self {
        Query::all()
    }
}

impl Query {
    pub fn all() -> Query {
        Query { op: Op::All, trigrams: Vec::new(), subs: Vec::new() }
    }
    pub fn none() -> Query {
        Query { op: Op::None, trigrams: Vec::new(), subs: Vec::new() }
    }
    pub fn and_trigrams(mut trigrams: Vec<u32>) -> Query {
        trigrams.sort_unstable();
        trigrams.dedup();
        Query { op: Op::And, trigrams, subs: Vec::new() }
    }

    pub fn and(self, r: Query) -> Query {
        and_or(self, r, Op::And)
    }
    pub fn or(self, r: Query) -> Query {
        and_or(self, r, Op::Or)
    }

    /// An `And`/`Or` doing no real work can be rewritten: empty → All/None,
    /// one sub → that sub, one trigram → either op.
    fn maybe_rewrite(mut self, op: Op) -> Query {
        if self.op != Op::And && self.op != Op::Or {
            return self;
        }
        let n = self.subs.len() + self.trigrams.len();
        if n > 1 {
            return self;
        }
        if n == 0 {
            return if self.op == Op::And { Query::all() } else { Query::none() };
        }
        if self.subs.len() == 1 {
            return self.subs.pop().unwrap();
        }
        self.op = op;
        self
    }

    fn is_atom(&self) -> bool {
        self.trigrams.len() == 1 && self.subs.is_empty()
    }
}

fn merge_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn and_or(q: Query, r: Query, op: Op) -> Query {
    let mut q = q.maybe_rewrite(op);
    let mut r = r.maybe_rewrite(op);

    // Boolean simplification.
    if q.op == Op::None || r.op == Op::None {
        if op == Op::And {
            return Query::none();
        }
        return if q.op == Op::None { r } else { q };
    }
    if q.op == Op::All || r.op == Op::All {
        if op == Op::Or {
            return Query::all();
        }
        return if q.op == Op::All { r } else { q };
    }

    // Both are And/Or now. If they match, or can be made to match, merge.
    let q_atom = q.is_atom();
    let r_atom = r.is_atom();
    if q.op == op && (r.op == op || r_atom) {
        q.trigrams = merge_sorted(&q.trigrams, &r.trigrams);
        q.subs.append(&mut r.subs);
        return q;
    }
    if r.op == op && q_atom {
        r.trigrams = merge_sorted(&r.trigrams, &q.trigrams);
        return r;
    }
    if q_atom && r_atom {
        q.op = op;
        q.trigrams = merge_sorted(&q.trigrams, &r.trigrams);
        return q;
    }

    // If one matches the op, add the other to it.
    if q.op == op {
        q.subs.push(r);
        return q;
    }
    if r.op == op {
        r.subs.push(q);
        return r;
    }

    // Creating an AND of ORs or an OR of ANDs: factor out common trigrams.
    //   (abc|def|ghi|jkl) AND (abc|def|mno|prs)
    //     => (abc|def) OR ((ghi|jkl) AND (mno|prs))
    let mut common = Vec::new();
    let mut qt = Vec::with_capacity(q.trigrams.len());
    let mut rt = Vec::with_capacity(r.trigrams.len());
    let (mut i, mut j) = (0, 0);
    while i < q.trigrams.len() && j < r.trigrams.len() {
        match q.trigrams[i].cmp(&r.trigrams[j]) {
            std::cmp::Ordering::Less => {
                qt.push(q.trigrams[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                rt.push(r.trigrams[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                common.push(q.trigrams[i]);
                i += 1;
                j += 1;
            }
        }
    }
    qt.extend_from_slice(&q.trigrams[i..]);
    rt.extend_from_slice(&r.trigrams[j..]);
    if !common.is_empty() {
        q.trigrams = qt;
        r.trigrams = rt;
        let inner = and_or(q, r, op);
        let outer = if op == Op::And { Op::Or } else { Op::And };
        return Query { op: outer, trigrams: common, subs: vec![inner] };
    }

    Query { op, trigrams: Vec::new(), subs: vec![q, r] }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.op {
            Op::None => return f.write_str("-"),
            Op::All => return f.write_str("+"),
            _ => {}
        }
        if self.subs.is_empty() && self.trigrams.len() == 1 {
            return f.write_str(&trigram::to_string(self.trigrams[0]));
        }
        let (start, sjoin, end, tjoin) = if self.op == Op::And {
            ("", " ", "", " ")
        } else {
            ("(", ")|(", ")", "|")
        };
        f.write_str(start)?;
        for (i, &t) in self.trigrams.iter().enumerate() {
            if i > 0 {
                f.write_str(tjoin)?;
            }
            f.write_str(&trigram::to_string(t))?;
        }
        if !self.subs.is_empty() {
            if !self.trigrams.is_empty() {
                f.write_str(sjoin)?;
            }
            for (i, s) in self.subs.iter().enumerate() {
                if i > 0 {
                    f.write_str(sjoin)?;
                }
                write!(f, "{s}")?;
            }
        }
        f.write_str(end)
    }
}
