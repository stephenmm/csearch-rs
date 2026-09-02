// Copyright (c) 2011 The Go Authors. All rights reserved.
// Copyright (c) 2026 The csearch-rs Authors. All rights reserved.
//
// Derived from Russ Cox's Go implementation at
// https://github.com/google/codesearch. Use of this source code is governed
// by a BSD-style licence; see the LICENSE file at the repository root.

//! Regular-expression → trigram-query analysis, ported from Russ Cox's
//! `index/regexp.go` (see "Regular Expression Matching with a Trigram
//! Index", swtch.com/~rsc/regexp/regexp4.html).
//!
//! For every sub-expression we track: whether it can match the empty
//! string; if bounded, the exact set of strings it matches; otherwise the
//! sets of possible prefixes and suffixes; and a trigram `Query` that any
//! match must satisfy. Sets are capped at `MAX_SET` entries — when a set
//! grows past that, its trigrams are folded into the query and the set is
//! shortened, so the result is always conservative (never misses a file).

use crate::query::Query;
use crate::trigram;
use anyhow::{anyhow, Result};
use regex_syntax::hir::{Class, Hir, HirKind};

const MAX_SET: usize = 20;

/// A set of byte strings. Prefix sets are kept in lexical order, suffix sets
/// in reverse-lexical order (so redundant entries are adjacent).
type StrSet = Vec<Vec<u8>>;

fn suffix_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.iter().rev().cmp(b.iter().rev())
}

fn clean(s: &mut StrSet, is_suffix: bool) {
    if is_suffix {
        s.sort_unstable_by(|a, b| suffix_cmp(a, b));
    } else {
        s.sort_unstable();
    }
    s.dedup();
}

fn union(a: &StrSet, b: &StrSet, is_suffix: bool) -> StrSet {
    let mut out = a.clone();
    out.extend(b.iter().cloned());
    clean(&mut out, is_suffix);
    out
}

fn cross(a: &StrSet, b: &StrSet, is_suffix: bool) -> StrSet {
    let mut out = Vec::with_capacity(a.len() * b.len());
    for x in a {
        for y in b {
            let mut s = Vec::with_capacity(x.len() + y.len());
            s.extend_from_slice(x);
            s.extend_from_slice(y);
            out.push(s);
        }
    }
    clean(&mut out, is_suffix);
    out
}

fn min_len(s: &StrSet) -> usize {
    s.iter().map(Vec::len).min().unwrap_or(0)
}

/// OR over the set of (AND of each string's trigrams). Any string shorter
/// than three bytes contributes no constraint, making the whole thing `All`.
fn set_trigrams(s: &StrSet) -> Query {
    let mut q = Query::none();
    for str in s {
        if str.len() < 3 {
            return Query::all();
        }
        let t: Vec<u32> = str.windows(3).map(trigram::pack).collect();
        q = q.or(Query::and_trigrams(t));
    }
    q
}

#[derive(Clone, Debug)]
struct Info {
    can_empty: bool,
    exact: Option<StrSet>,
    prefix: StrSet,
    suffix: StrSet,
    m: Query,
}

fn no_match() -> Info {
    Info { can_empty: false, exact: None, prefix: vec![], suffix: vec![], m: Query::none() }
}
fn empty_string() -> Info {
    Info { can_empty: true, exact: Some(vec![vec![]]), prefix: vec![], suffix: vec![], m: Query::all() }
}
fn any_char() -> Info {
    Info { can_empty: false, exact: None, prefix: vec![vec![]], suffix: vec![vec![]], m: Query::all() }
}
fn any_match() -> Info {
    Info { can_empty: true, exact: None, prefix: vec![vec![]], suffix: vec![vec![]], m: Query::all() }
}
fn literal(bytes: Vec<u8>) -> Info {
    let can_empty = bytes.is_empty();
    Info { can_empty, exact: Some(vec![bytes]), prefix: vec![], suffix: vec![], m: Query::all() }
}

impl Info {
    fn add_exact(&mut self) {
        if let Some(e) = &self.exact {
            let t = set_trigrams(e);
            self.m = std::mem::take(&mut self.m).and(t);
        }
    }

    fn simplify(&mut self, force: bool) {
        let too_big = match &self.exact {
            Some(e) => e.len() > MAX_SET || (force && e.len() > 1),
            None => false,
        };
        if too_big {
            self.add_exact();
            let exact = self.exact.take().unwrap();
            for s in exact {
                let n = s.len();
                if n < 3 {
                    self.prefix.push(s.clone());
                    self.suffix.push(s);
                } else {
                    self.prefix.push(s[..2].to_vec());
                    self.suffix.push(s[n - 2..].to_vec());
                }
            }
            clean(&mut self.prefix, false);
            clean(&mut self.suffix, true);
        }
        if self.exact.is_none() {
            self.simplify_set(true);
            self.simplify_set(false);
        }
    }

    /// Fold the set's trigrams into the query, then shrink the set to at
    /// most two-byte strings (fewer if it is still too large) and drop
    /// entries made redundant by a shorter prefix/suffix already present.
    fn simplify_set(&mut self, is_prefix: bool) {
        let mut t = std::mem::take(if is_prefix { &mut self.prefix } else { &mut self.suffix });
        clean(&mut t, !is_prefix);
        self.m = std::mem::take(&mut self.m).and(set_trigrams(&t));

        let mut n = 3usize;
        while n == 3 || t.len() > MAX_SET {
            let keep = n - 1;
            for s in t.iter_mut() {
                if s.len() >= n {
                    if is_prefix {
                        s.truncate(keep);
                    } else {
                        let start = s.len() - keep;
                        s.drain(..start);
                    }
                }
            }
            clean(&mut t, !is_prefix);
            if n == 1 {
                break;
            }
            n -= 1;
        }

        let mut out: StrSet = Vec::with_capacity(t.len());
        for s in t {
            if let Some(last) = out.last() {
                let redundant = if is_prefix { s.starts_with(last) } else { s.ends_with(last) };
                if redundant {
                    continue;
                }
            }
            out.push(s);
        }
        if is_prefix {
            self.prefix = out;
        } else {
            self.suffix = out;
        }
    }
}

fn concat(x: Info, y: Info) -> Info {
    let mut xy = Info {
        can_empty: x.can_empty && y.can_empty,
        exact: None,
        prefix: vec![],
        suffix: vec![],
        m: x.m.clone().and(y.m.clone()),
    };
    match (&x.exact, &y.exact) {
        (Some(xe), Some(ye)) => xy.exact = Some(cross(xe, ye, false)),
        _ => {
            if let Some(xe) = &x.exact {
                xy.prefix = cross(xe, &y.prefix, false);
            } else {
                xy.prefix = x.prefix.clone();
                if x.can_empty {
                    xy.prefix = union(&xy.prefix, &y.prefix, false);
                }
            }
            if let Some(ye) = &y.exact {
                xy.suffix = cross(&x.suffix, ye, true);
            } else {
                xy.suffix = y.suffix.clone();
                if y.can_empty {
                    xy.suffix = union(&xy.suffix, &x.suffix, true);
                }
            }
        }
    }

    // If every string in x.suffix × y.prefix is at least three bytes, one of
    // those trigrams must appear, and it may not be captured by xy.prefix or
    // xy.suffix.
    if x.exact.is_none()
        && y.exact.is_none()
        && x.suffix.len() <= MAX_SET
        && y.prefix.len() <= MAX_SET
        && min_len(&x.suffix) + min_len(&y.prefix) >= 3
    {
        let t = set_trigrams(&cross(&x.suffix, &y.prefix, false));
        xy.m = xy.m.and(t);
    }

    xy.simplify(false);
    xy
}

fn alternate(mut x: Info, mut y: Info) -> Info {
    let mut xy = Info { can_empty: false, exact: None, prefix: vec![], suffix: vec![], m: Query::all() };
    match (&x.exact, &y.exact) {
        (Some(xe), Some(ye)) => xy.exact = Some(union(xe, ye, false)),
        (Some(xe), None) => {
            xy.prefix = union(xe, &y.prefix, false);
            xy.suffix = union(xe, &y.suffix, true);
            x.add_exact();
        }
        (None, Some(ye)) => {
            xy.prefix = union(&x.prefix, ye, false);
            xy.suffix = union(&x.suffix, ye, true);
            y.add_exact();
        }
        (None, None) => {
            xy.prefix = union(&x.prefix, &y.prefix, false);
            xy.suffix = union(&x.suffix, &y.suffix, true);
        }
    }
    xy.can_empty = x.can_empty || y.can_empty;
    xy.m = x.m.or(y.m);
    xy.simplify(false);
    xy
}

/// x+ : at least one x, so prefixes/suffixes stay; exactness is lost.
fn plus(mut x: Info) -> Info {
    if let Some(e) = x.exact.take() {
        x.prefix = e.clone();
        x.suffix = e;
        clean(&mut x.suffix, true);
    }
    x
}

fn class_strings(class: &Class) -> Option<StrSet> {
    let mut out: StrSet = Vec::new();
    match class {
        Class::Unicode(c) => {
            for r in c.iter() {
                let (lo, hi) = (r.start() as u32, r.end() as u32);
                if (hi - lo) as usize >= MAX_SET {
                    return None;
                }
                for cp in lo..=hi {
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch.to_string().into_bytes());
                        if out.len() > MAX_SET {
                            return None;
                        }
                    }
                }
            }
        }
        Class::Bytes(c) => {
            for r in c.iter() {
                for b in r.start()..=r.end() {
                    out.push(vec![b]);
                    if out.len() > MAX_SET {
                        return None;
                    }
                }
            }
        }
    }
    Some(out)
}

fn analyze(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty => empty_string(),
        HirKind::Literal(lit) => literal(lit.0.to_vec()),
        HirKind::Class(class) => match class_strings(class) {
            Some(set) if set.is_empty() => no_match(),
            Some(mut set) => {
                clean(&mut set, false);
                Info { can_empty: false, exact: Some(set), prefix: vec![], suffix: vec![], m: Query::all() }
            }
            None => any_char(),
        },
        HirKind::Look(_) => empty_string(),
        HirKind::Capture(c) => analyze(&c.sub),
        HirKind::Repetition(rep) => {
            let (min, max) = (rep.min, rep.max);
            if min == 0 {
                if max == Some(1) {
                    return alternate(analyze(&rep.sub), empty_string());
                }
                return any_match();
            }
            let x = analyze(&rep.sub);
            let power = |k: u32| -> Info {
                let mut r = x.clone();
                for _ in 1..k {
                    r = concat(r, x.clone());
                }
                r
            };
            match max {
                Some(m) if m == min => power(min),
                None => {
                    if min == 1 {
                        plus(x.clone())
                    } else {
                        concat(power(min - 1), plus(x.clone()))
                    }
                }
                Some(_) => concat(power(min), any_match()),
            }
        }
        HirKind::Concat(subs) => {
            let mut info = empty_string();
            for s in subs {
                info = concat(info, analyze(s));
            }
            info
        }
        HirKind::Alternation(subs) => {
            let mut info = no_match();
            for s in subs {
                info = alternate(info, analyze(s));
            }
            info
        }
    }
}

/// Parse a pattern with the same syntax the `regex` crate uses.
pub fn parse(pattern: &str, case_insensitive: bool) -> Result<Hir> {
    regex_syntax::ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .build()
        .parse(pattern)
        .map_err(|e| anyhow!("bad regexp: {e}"))
}

/// Compute the trigram query for a parsed regular expression.
pub fn query_for(hir: &Hir) -> Query {
    let mut info = analyze(hir);
    info.simplify(true);
    info.add_exact();
    info.m
}

/// Convenience: parse and analyze.
pub fn regexp_query(pattern: &str, case_insensitive: bool) -> Result<Query> {
    Ok(query_for(&parse(pattern, case_insensitive)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(p: &str) -> String {
        regexp_query(p, false).unwrap().to_string()
    }

    #[test]
    fn cox_cases() {
        // Same cases as the Go test-suite (index/regexp_test.go); the
        // printed form differs slightly where common trigrams are factored.
        assert_eq!(q("Abcdef"), "Abc bcd cde def");
        assert_eq!(q("(abc)(def)"), "abc bcd cde def");
        assert_eq!(q("abc.*(def|ghi)"), "abc (def|ghi)");
        assert_eq!(q("abc(def|ghi)"), "abc (bcd cde)|(bcg cgh) (def|ghi)");
        assert_eq!(q("a+hello"), "ahe ell hel llo");
        assert_eq!(q("a*hello"), "ell hel llo");
        assert_eq!(q("def|abc"), "(abc|def)");
        assert_eq!(q("abc|def"), "(abc|def)");
        assert_eq!(q("ab[cde]"), "(abc|abd|abe)");
        assert_eq!(q("ab[cde]fgh"), "fgh (abc bcf cfg)|(abd bdf dfg)|(abe bef efg)");
        assert_eq!(q("."), "+");
        assert_eq!(q("(abc)+"), "abc");
        assert_eq!(q("(abc)*"), "+");
        assert_eq!(q("(abc)?"), "+");
        assert_eq!(q("(abc){2}"), "abc bca cab");
        assert_eq!(q("(abc){2,}"), "abc bca cab");
        assert_eq!(q("(abc){1,3}"), "abc");
        assert_eq!(q("hello.*hello"), "ell hel llo");
        assert_eq!(q("^abcd$"), "abc bcd");
        assert_eq!(q("[a-z]+"), "+");
        assert_eq!(q("abc\\z"), "abc");
        assert_eq!(q("abc?def"), "def (abc bcd cde)|(abd bde)");
        assert_eq!(q("(foo|bar)baz"), "baz (oba oob)|(arb rba) (bar|foo)");
        assert_eq!(q("x(abc|abd)y"), "xab (abc|abd) (bcy|bdy)");
        assert_eq!(q("\\bfoobar\\b"), "bar foo oba oob");
        assert_eq!(q("fo+bar"), "bar oba");
    }

    #[test]
    fn case_folding() {
        assert_eq!(
            regexp_query("abc", true).unwrap().to_string(),
            "(ABC|ABc|AbC|Abc|aBC|aBc|abC|abc)"
        );
    }

    #[test]
    fn big_class_is_any() {
        assert_eq!(q("[a-zA-Z]bcdef"), "bcd cde def");
    }
}
