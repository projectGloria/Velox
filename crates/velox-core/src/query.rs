//! Query parsing and execution.
//!
//! Two execution plans exist, and which one is chosen is what determines whether
//! a keystroke costs microseconds or milliseconds:
//!
//! * [`Plan::BinarySearch`] — a single positive filename-prefix term with no
//!   filters. Resolves to a contiguous range of each volume's sorted order. No
//!   allocation, no per-entry work, O(log n).
//! * [`Plan::Scan`] / [`Plan::Narrowed`] — anything else. A linear pass over
//!   candidates. `Narrowed` is the important one: when the user appends a
//!   character, the new result set is a subset of the old one, so the scan runs
//!   over the previous results instead of the whole corpus. Typing therefore gets
//!   *cheaper* per keystroke rather than more expensive.

use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::fold::{self, fold_to_string};
use crate::index::{Index, VolumeIndex};
use crate::store::{EntryId, EntryRef, EntryStore, INVALID_ENTRY};

// ---------------------------------------------------------------- query model

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MatchMode {
    /// Names must start with the term. The only mode the binary search serves.
    #[default]
    Prefix,
    /// Term may appear anywhere in the name.
    Substring,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Scope {
    /// Match the filename only.
    #[default]
    Name,
    /// Match anywhere in the full path, including ancestor directory names.
    Path,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Files,
    Folders,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
}

/// Terms are bucketed into OR-groups tracked in a `u64` bitmask, so a query can
/// hold at most this many AND-ed groups. Anything beyond folds into the last.
const MAX_GROUPS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TermKind {
    Prefix,
    Substring,
    Wildcard,
}

#[derive(Clone, Debug)]
struct Term {
    /// Case-folded needle (or wildcard pattern).
    folded: Vec<u8>,
    kind: TermKind,
    negate: bool,
    /// Terms OR'd together; the group as a whole is AND'd with other groups.
    group: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Filters {
    pub only: Option<Kind>,
    /// Folded extensions without the leading dot.
    pub ext: Vec<Vec<u8>>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Windows FILETIME bounds (100 ns ticks since 1601-01-01 UTC).
    pub min_mtime: Option<i64>,
    pub max_mtime: Option<i64>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.only.is_none()
            && self.ext.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.min_mtime.is_none()
            && self.max_mtime.is_none()
    }

    fn accepts(&self, s: &EntryStore, id: EntryId) -> bool {
        if let Some(k) = self.only {
            let is_dir = s.is_dir(id);
            if (k == Kind::Folders) != is_dir {
                return false;
            }
        }
        if !self.ext.is_empty() {
            let name = s.name_bytes_of(id);
            let dot = match memchr::memrchr(b'.', name) {
                Some(d) => d + 1,
                None => return false,
            };
            let got = &name[dot..];
            if !self.ext.iter().any(|e| e.len() == got.len() && fold::fold_starts_with(got, e)) {
                return false;
            }
        }
        let sz = s.size(id);
        if self.min_size.is_some_and(|m| sz < m) || self.max_size.is_some_and(|m| sz > m) {
            return false;
        }
        let mt = s.mtime(id);
        if self.min_mtime.is_some_and(|m| mt < m) || self.max_mtime.is_some_and(|m| mt > m) {
            return false;
        }
        true
    }
}

#[derive(Clone, Debug)]
pub struct Query {
    raw: String,
    terms: Vec<Term>,
    groups: usize,
    pub mode: MatchMode,
    pub scope: Scope,
    pub filters: Filters,
    pub sort: SortKey,
    pub sort_desc: bool,
}

impl Query {
    /// Parse a query string.
    ///
    /// Grammar: space-separated tokens are AND'd. `|` between tokens OR's them.
    /// A leading `!` negates a token. Double quotes group a token containing
    /// spaces. `*` and `?` make a token a wildcard pattern. Tokens of the form
    /// `key:value` are filters (`ext:`, `size:`, `dm:`, `file:`, `folder:`).
    pub fn parse(raw: &str, mode: MatchMode, scope: Scope) -> Query {
        let mut q = Query {
            raw: raw.to_string(),
            terms: Vec::new(),
            groups: 0,
            mode,
            scope,
            filters: Filters::default(),
            sort: SortKey::Name,
            sort_desc: false,
        };
        let tokens = tokenize(raw);
        let mut group = 0usize;
        let mut i = 0;
        while i < tokens.len() {
            let tok = &tokens[i];
            if tok == "|" {
                // Next token joins the previous group instead of starting a new one.
                i += 1;
                if let Some(next) = tokens.get(i) {
                    if let Some(t) = q.make_term(next, group.saturating_sub(1)) {
                        q.terms.push(t);
                    }
                    i += 1;
                }
                continue;
            }
            if q.apply_filter(tok) {
                i += 1;
                continue;
            }
            if let Some(t) = q.make_term(tok, group) {
                q.terms.push(t);
                group += 1;
            }
            i += 1;
        }
        q.groups = group;
        q
    }

    fn make_term(&self, tok: &str, group: usize) -> Option<Term> {
        let (negate, body) = match tok.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, tok),
        };
        if body.is_empty() {
            return None;
        }
        let kind = if body.contains('*') || body.contains('?') {
            TermKind::Wildcard
        } else if negate || self.mode == MatchMode::Substring || self.scope == Scope::Path {
            // Negation and path scope can never use the sorted order anyway, so
            // treat them as substring matches, which is what users expect.
            TermKind::Substring
        } else {
            TermKind::Prefix
        };
        Some(Term { folded: fold_to_string(body), kind, negate, group })
    }

    /// Returns true if the token was consumed as a filter.
    fn apply_filter(&mut self, tok: &str) -> bool {
        let lower = tok.to_ascii_lowercase();
        match lower.as_str() {
            "file:" | "files:" => {
                self.filters.only = Some(Kind::Files);
                return true;
            }
            "folder:" | "folders:" | "dir:" => {
                self.filters.only = Some(Kind::Folders);
                return true;
            }
            _ => {}
        }
        let Some((key, val)) = tok.split_once(':') else { return false };
        if val.is_empty() {
            return false;
        }
        match key.to_ascii_lowercase().as_str() {
            "ext" => {
                self.filters.ext = val
                    .split(';')
                    .filter(|e| !e.is_empty())
                    .map(|e| fold_to_string(e.trim_start_matches('.')))
                    .collect();
                true
            }
            "size" => {
                let (op, rest) = split_op(val);
                match (op, parse_size(rest)) {
                    (Some('>'), Some(n)) => self.filters.min_size = Some(n),
                    (Some('<'), Some(n)) => self.filters.max_size = Some(n),
                    (None, Some(n)) => {
                        self.filters.min_size = Some(n);
                        self.filters.max_size = Some(n);
                    }
                    _ => return false,
                }
                true
            }
            "dm" | "modified" => {
                let (op, rest) = split_op(val);
                match (op, parse_date(rest)) {
                    (Some('>'), Some(t)) => self.filters.min_mtime = Some(t),
                    (Some('<'), Some(t)) => self.filters.max_mtime = Some(t),
                    (None, Some(t)) => {
                        self.filters.min_mtime = Some(t);
                        // A bare date means "that whole day".
                        self.filters.max_mtime = Some(t + 864_000_000_000);
                    }
                    _ => return false,
                }
                true
            }
            "sort" => {
                let (k, desc) = match val.strip_suffix("-desc") {
                    Some(k) => (k, true),
                    None => (val, false),
                };
                self.sort = match k.to_ascii_lowercase().as_str() {
                    "name" => SortKey::Name,
                    "path" => SortKey::Path,
                    "size" => SortKey::Size,
                    "date" | "modified" => SortKey::Modified,
                    _ => return false,
                };
                self.sort_desc = desc;
                true
            }
            _ => false,
        }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.filters.is_empty()
    }

    /// The single positive prefix term this query reduces to, if any. This is the
    /// gate for the binary-search plan.
    fn sole_prefix_term(&self) -> Option<&[u8]> {
        if !self.filters.is_empty() || self.scope != Scope::Name || self.sort != SortKey::Name {
            return None;
        }
        match self.terms.as_slice() {
            [] => Some(&[]),
            [t] if t.kind == TermKind::Prefix && !t.negate => Some(&t.folded),
            _ => None,
        }
    }

    /// Does `self` necessarily match a subset of what `other` matched? If so, a
    /// scan for `self` can run over `other`'s results instead of the whole corpus.
    fn refines(&self, other: &Query) -> bool {
        if self.mode != other.mode
            || self.scope != other.scope
            || other.terms.iter().any(|t| t.negate)
        {
            return false;
        }
        // Every previous term must still be present and no looser than before.
        // The cheap, always-safe case: the raw query grew by appending characters
        // to the last term, and no filters were relaxed.
        if !self.raw.starts_with(&other.raw) {
            return false;
        }
        if !filters_tighten(&other.filters, &self.filters) {
            return false;
        }
        // Appending to a prefix term only ever narrows. Appending to a substring
        // term also only narrows. Appending after a space adds a term: narrows.
        // Appending to a wildcard can widen (`a` -> `a*`), so refuse those.
        !other.terms.iter().any(|t| t.kind == TermKind::Wildcard)
            && !self.terms.iter().any(|t| t.kind == TermKind::Wildcard)
    }

    /// Test one entry. This runs once per candidate on the scan path, so it must
    /// not allocate: group membership is tracked in a pair of `u64` bitmasks
    /// rather than `Vec<bool>`, which is worth roughly an order of magnitude on a
    /// multi-million-entry scan.
    fn matches(&self, s: &EntryStore, id: EntryId, scratch: &mut Vec<u8>) -> bool {
        if !self.filters.accepts(s, id) {
            return false;
        }
        match self.terms.as_slice() {
            [] => return true,
            // Overwhelmingly the common case; skip the group machinery entirely.
            [t] => {
                let hit = match self.scope {
                    Scope::Name => term_hits(t, s.name_bytes_of(id)),
                    Scope::Path => path_hits(t, s, id, scratch),
                };
                return hit != t.negate;
            }
            _ => {}
        }
        let mut group_ok: u64 = 0;
        let mut group_seen: u64 = 0;
        for t in &self.terms {
            let g = t.group.min(MAX_GROUPS - 1);
            let bit = 1u64 << g;
            group_seen |= bit;
            let hit = match self.scope {
                Scope::Name => term_hits(t, s.name_bytes_of(id)),
                Scope::Path => path_hits(t, s, id, scratch),
            };
            if hit != t.negate {
                group_ok |= bit;
            }
        }
        group_ok == group_seen
    }
}

fn filters_tighten(old: &Filters, new: &Filters) -> bool {
    if old.only.is_some() && old.only != new.only {
        return false;
    }
    if !old.ext.is_empty() && old.ext != new.ext {
        return false;
    }
    let ge = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(x), Some(y)) => y >= x,
        (Some(_), None) => false,
        _ => true,
    };
    let le = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(x), Some(y)) => y <= x,
        (Some(_), None) => false,
        _ => true,
    };
    ge(old.min_size, new.min_size)
        && le(old.max_size, new.max_size)
        && ge(old.min_mtime.map(|v| v as u64), new.min_mtime.map(|v| v as u64))
        && le(old.max_mtime.map(|v| v as u64), new.max_mtime.map(|v| v as u64))
}

#[inline]
fn term_hits(t: &Term, name: &[u8]) -> bool {
    match t.kind {
        TermKind::Prefix => fold::fold_starts_with(name, &t.folded),
        TermKind::Substring => fold::fold_contains(name, &t.folded),
        TermKind::Wildcard => wildcard_match(name, &t.folded),
    }
}

/// Path-scope matching without building a `String` per candidate.
///
/// A needle with no separator matches the full path exactly when it matches some
/// single path component, so we walk the (short) ancestor chain and test names
/// directly out of the name blob. A needle containing `\` can straddle a
/// component boundary, so that case falls back to reconstructing the path.
fn path_hits(t: &Term, s: &EntryStore, id: EntryId, scratch: &mut Vec<u8>) -> bool {
    if t.folded.contains(&b'\\') || t.kind == TermKind::Wildcard {
        scratch.clear();
        let p = s.path(id);
        scratch.extend_from_slice(p.as_bytes());
        return term_hits(t, scratch);
    }
    let mut cur = id;
    let mut guard = 0;
    while cur != INVALID_ENTRY && guard < 256 {
        if !s.is_live(cur) {
            break;
        }
        if term_hits(t, s.name_bytes_of(cur)) {
            return true;
        }
        let p = s.parent(cur);
        if p == cur {
            break;
        }
        cur = p;
        guard += 1;
    }
    // The drive label is part of the path too (`C:`).
    fold::fold_contains(s.label.as_bytes(), &t.folded)
}

/// Iterative `*` / `?` matcher with backtracking. Operates on folded bytes, so
/// `hay` is folded lazily one byte at a time.
fn wildcard_match(hay: &[u8], pat: &[u8]) -> bool {
    let (mut h, mut p) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while h < hay.len() {
        if p < pat.len() && (pat[p] == b'?' || fold::fold_byte(hay[h]) == pat[p]) {
            h += 1;
            p += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = h;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            h = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

// ------------------------------------------------------------------- results

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plan {
    /// Two binary searches per volume; nothing materialised.
    BinarySearch,
    /// Full linear pass over the corpus.
    Scan,
    /// Linear pass over the previous query's results.
    Narrowed,
}

#[derive(Clone, Debug)]
pub struct VolRange {
    pub volume: u16,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug)]
pub enum Matches {
    /// Contiguous ranges of each volume's sorted order, plus any overlay hits.
    /// Ordered by name within a volume, volumes concatenated in order.
    Ranges { ranges: Vec<VolRange>, overlay: Vec<EntryRef> },
    List(Vec<EntryRef>),
}

#[derive(Clone, Debug)]
pub struct SearchResults {
    pub matches: Matches,
    pub total: usize,
    pub plan: Plan,
    pub took: Duration,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Materialise a window of results — what a virtualized list actually needs.
    /// Never touches more entries than the window it is asked for (plus skipped
    /// tombstones).
    pub fn window(&self, index: &Index, offset: usize, count: usize) -> Vec<EntryRef> {
        let mut out = Vec::with_capacity(count.min(4096));
        match &self.matches {
            Matches::List(v) => {
                let lo = offset.min(v.len());
                let hi = (offset + count).min(v.len());
                out.extend_from_slice(&v[lo..hi]);
            }
            Matches::Ranges { ranges, overlay } => {
                let mut skip = offset;
                for r in ranges {
                    if out.len() >= count {
                        break;
                    }
                    let vol = index.volumes[r.volume as usize].read().unwrap();
                    for id in vol.iter_range(r.start..r.end) {
                        if skip > 0 {
                            skip -= 1;
                            continue;
                        }
                        out.push(EntryRef::new(r.volume, id));
                        if out.len() >= count {
                            break;
                        }
                    }
                }
                if out.len() < count {
                    let lo = skip.min(overlay.len());
                    let hi = (lo + count - out.len()).min(overlay.len());
                    out.extend_from_slice(&overlay[lo..hi]);
                }
            }
        }
        out
    }

    /// Sort an already-fetched result set without re-running the query. Forces
    /// materialisation when the requested order is not the stored one.
    pub fn sort_by(&mut self, index: &Index, key: SortKey, desc: bool) {
        if key == SortKey::Name && !desc && matches!(self.matches, Matches::Ranges { .. }) {
            return; // already in this order
        }
        let mut all = match std::mem::replace(&mut self.matches, Matches::List(Vec::new())) {
            Matches::List(v) => v,
            m => {
                let saved = SearchResults {
                    matches: m,
                    total: self.total,
                    plan: self.plan,
                    took: self.took,
                };
                saved.window(index, 0, self.total)
            }
        };
        sort_refs(&mut all, index, key, desc);
        self.matches = Matches::List(all);
    }
}

fn sort_refs(refs: &mut [EntryRef], index: &Index, key: SortKey, desc: bool) {
    // Pull the sort keys out once rather than locking per comparison.
    let mut keyed: Vec<(EntryRef, u64, String)> = refs
        .iter()
        .map(|r| {
            let vol = index.volumes[r.volume() as usize].read().unwrap();
            let id = r.entry();
            match key {
                SortKey::Size => (*r, vol.store.size(id), String::new()),
                SortKey::Modified => (*r, vol.store.mtime(id) as u64, String::new()),
                SortKey::Path => (*r, 0, vol.store.path(id).to_ascii_lowercase()),
                SortKey::Name => (*r, 0, vol.store.name(id).to_ascii_lowercase()),
            }
        })
        .collect();
    match key {
        SortKey::Size | SortKey::Modified => keyed.sort_by_key(|a| a.1),
        _ => keyed.sort_by(|a, b| a.2.cmp(&b.2)),
    }
    if desc {
        keyed.reverse();
    }
    for (dst, src) in refs.iter_mut().zip(keyed) {
        *dst = src.0;
    }
}

// ------------------------------------------------------------------ searcher

/// Executes queries against an index, remembering the previous result set so a
/// query that refines the last one can be answered by narrowing it.
pub struct Searcher {
    last: Option<(Query, Vec<EntryRef>)>,
    /// Result sets above this are not cached for narrowing; re-scanning is
    /// cheaper than holding tens of megabytes of refs.
    pub narrow_cache_limit: usize,
}

impl Default for Searcher {
    fn default() -> Self {
        Searcher { last: None, narrow_cache_limit: 1 << 20 }
    }
}

impl Searcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn search(&mut self, index: &Index, query: &Query) -> SearchResults {
        let t0 = Instant::now();

        // Plan A: the fast path. One positive prefix term, no filters.
        if let Some(prefix) = query.sole_prefix_term() {
            let mut ranges = Vec::with_capacity(index.volumes.len());
            let mut overlay = Vec::new();
            let mut total = 0usize;
            for v in &index.volumes {
                let vol = v.read().unwrap();
                let r = vol.prefix_range(prefix);
                total += vol.count_range(r.clone());
                if r.end > r.start {
                    ranges.push(VolRange { volume: vol.id, start: r.start, end: r.end });
                }
                let before = overlay.len();
                vol.delta_prefix_matches(prefix, &mut overlay);
                total += overlay.len() - before;
            }
            self.last = None; // ranges are not a narrowing base
            return SearchResults {
                matches: Matches::Ranges { ranges, overlay },
                total,
                plan: Plan::BinarySearch,
                took: t0.elapsed(),
            };
        }

        // Plan B: narrow the previous result set if this query refines it.
        if let Some((prev_q, prev)) = &self.last {
            if query.refines(prev_q) {
                let mut out = Vec::with_capacity(prev.len() / 4 + 16);
                // Results are grouped by volume, so walk the runs and take each
                // volume's read lock once. Locking per result made narrowing
                // slower than a fresh scan.
                let mut i = 0usize;
                while i < prev.len() {
                    let vol_id = prev[i].volume();
                    let mut j = i;
                    while j < prev.len() && prev[j].volume() == vol_id {
                        j += 1;
                    }
                    let vol = index.volumes[vol_id as usize].read().unwrap();
                    let parts: Vec<Vec<EntryRef>> = prev[i..j]
                        .par_chunks(8 * 1024)
                        .map(|chunk| {
                            let mut scratch = Vec::new();
                            let mut local = Vec::new();
                            for &r in chunk {
                                let id = r.entry();
                                if vol.store.is_live(id)
                                    && query.matches(&vol.store, id, &mut scratch)
                                {
                                    local.push(r);
                                }
                            }
                            local
                        })
                        .collect();
                    out.reserve(parts.iter().map(|p| p.len()).sum());
                    for p in &parts {
                        out.extend_from_slice(p);
                    }
                    i = j;
                }
                let total = out.len();
                let cached = total <= self.narrow_cache_limit;
                let res = SearchResults {
                    matches: Matches::List(out.clone()),
                    total,
                    plan: Plan::Narrowed,
                    took: t0.elapsed(),
                };
                self.last = if cached { Some((query.clone(), out)) } else { None };
                return res;
            }
        }

        // Plan C: full scan.
        let mut out = Vec::new();
        for v in &index.volumes {
            let vol = v.read().unwrap();
            let mut hits = scan_volume(&vol, query);
            out.append(&mut hits);
        }
        if query.sort != SortKey::Name || query.sort_desc {
            sort_refs(&mut out, index, query.sort, query.sort_desc);
        }
        let total = out.len();
        let cached = total <= self.narrow_cache_limit;
        let res = SearchResults {
            matches: Matches::List(out.clone()),
            total,
            plan: Plan::Scan,
            took: t0.elapsed(),
        };
        self.last = if cached { Some((query.clone(), out)) } else { None };
        res
    }
}

fn scan_volume(vol: &VolumeIndex, query: &Query) -> Vec<EntryRef> {
    vol.scan(|s, id| {
        // One scratch buffer per closure call is wasteful for path scope, but the
        // path branch only allocates for needles containing a separator.
        let mut scratch = Vec::new();
        query.matches(s, id, &mut scratch)
    })
}

// ------------------------------------------------------------------ tokenizer

fn tokenize(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in raw.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '|' if !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push("|".to_string());
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn split_op(v: &str) -> (Option<char>, &str) {
    match v.as_bytes().first() {
        Some(b'>') => (Some('>'), &v[1..]),
        Some(b'<') => (Some('<'), &v[1..]),
        Some(b'=') => (None, &v[1..]),
        _ => (None, v),
    }
}

fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim();
    let (num, mul) = if let Some(n) = v.strip_suffix("gb") {
        (n, 1u64 << 30)
    } else if let Some(n) = v.strip_suffix("mb") {
        (n, 1u64 << 20)
    } else if let Some(n) = v.strip_suffix("kb") {
        (n, 1u64 << 10)
    } else {
        (v.trim_end_matches('b'), 1)
    };
    num.trim().parse::<u64>().ok().map(|n| n * mul)
}

/// `YYYY-MM-DD` to Windows FILETIME. Proleptic Gregorian, UTC, no DST handling —
/// enough for a date-range filter.
fn parse_date(v: &str) -> Option<i64> {
    let mut it = v.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next().unwrap_or("1").parse().ok()?;
    let d: i64 = it.next().unwrap_or("1").parse().ok()?;
    if !(1601..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from civil epoch (Howard Hinnant's algorithm), rebased to 1601-01-01.
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_1970 = era * 146_097 + doe - 719_468;
    let days_from_1601 = days_from_1970 + 134_774;
    Some(days_from_1601 * 864_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{synthetic_volume, SourceKind, VolumeIndex};
    use crate::store::EntryStore;

    fn index_with(names: &[&str]) -> Index {
        let mut s = EntryStore::new("C:");
        let root = s.push("", INVALID_ENTRY, 0, 0, true);
        s.set_root(root);
        let docs = s.push("Documents", root, 0, 0, true);
        for (i, n) in names.iter().enumerate() {
            s.push(n, if i % 2 == 0 { root } else { docs }, (i as u64 + 1) * 1000, i as i64, false);
        }
        Index::new(vec![VolumeIndex::build(0, SourceKind::Walk, s)])
    }

    fn run(idx: &Index, q: &str, mode: MatchMode) -> Vec<String> {
        let mut se = Searcher::new();
        let query = Query::parse(q, mode, Scope::Name);
        let res = se.search(idx, &query);
        let w = res.window(idx, 0, 1000);
        assert_eq!(w.len(), res.total, "window length must agree with total for `{q}`");
        w.iter().map(|r| idx.describe(*r).unwrap().name).collect()
    }

    #[test]
    fn prefix_query_uses_binary_search() {
        let idx = index_with(&["report.txt", "Report2.doc", "notes.md", "readme"]);
        let mut se = Searcher::new();
        let res = se.search(&idx, &Query::parse("rep", MatchMode::Prefix, Scope::Name));
        assert_eq!(res.plan, Plan::BinarySearch);
        assert_eq!(res.total, 2);
    }

    #[test]
    fn prefix_is_case_insensitive_and_ordered() {
        let idx = index_with(&["report.txt", "Report2.doc", "notes.md", "readme"]);
        assert_eq!(run(&idx, "re", MatchMode::Prefix), ["readme", "report.txt", "Report2.doc"]);
    }

    #[test]
    fn substring_mode_finds_interior_matches() {
        let idx = index_with(&["report.txt", "myreport.doc", "notes.md"]);
        let mut names = run(&idx, "report", MatchMode::Substring);
        names.sort();
        assert_eq!(names, ["myreport.doc", "report.txt"]);
    }

    #[test]
    fn implicit_and_of_multiple_terms() {
        let idx = index_with(&["annual report 2024.txt", "annual notes.txt", "report draft.txt"]);
        let names = run(&idx, "annual report", MatchMode::Substring);
        assert_eq!(names, ["annual report 2024.txt"]);
    }

    #[test]
    fn negation_excludes() {
        let idx = index_with(&["a.log", "b.log", "c.txt"]);
        let mut names = run(&idx, ".log !b", MatchMode::Substring);
        names.sort();
        assert_eq!(names, ["a.log"]);
    }

    #[test]
    fn or_groups_alternatives() {
        let idx = index_with(&["alpha.txt", "beta.txt", "gamma.txt"]);
        let mut names = run(&idx, "alpha | beta", MatchMode::Substring);
        names.sort();
        assert_eq!(names, ["alpha.txt", "beta.txt"]);
    }

    #[test]
    fn wildcards() {
        let idx = index_with(&["setup.exe", "setup32.exe", "readme.txt"]);
        let mut names = run(&idx, "setup*.exe", MatchMode::Prefix);
        names.sort();
        assert_eq!(names, ["setup.exe", "setup32.exe"]);

        let names = run(&idx, "setup??.exe", MatchMode::Prefix);
        assert_eq!(names, ["setup32.exe"]);
    }

    #[test]
    fn extension_and_kind_filters() {
        let idx = index_with(&["a.rs", "b.md", "c.rs"]);
        let mut names = run(&idx, "ext:rs", MatchMode::Prefix);
        names.sort();
        assert_eq!(names, ["a.rs", "c.rs"]);

        let names = run(&idx, "folder:", MatchMode::Prefix);
        assert_eq!(names, ["", "Documents"]);
    }

    #[test]
    fn size_filter() {
        let idx = index_with(&["a", "b", "c"]); // sizes 1000, 2000, 3000
        let names = run(&idx, "size:>2500", MatchMode::Prefix);
        assert_eq!(names, ["c"]);
        // Directories report size 0, so a `<` bound needs `file:` to exclude them.
        let names = run(&idx, "size:<1500 file:", MatchMode::Prefix);
        assert_eq!(names, ["a"]);
        // Results come back in folded-name order, so "a" precedes "Documents".
        let names = run(&idx, "size:<1500", MatchMode::Prefix);
        assert_eq!(names, ["", "a", "Documents"]);
    }

    #[test]
    fn path_scope_matches_ancestor_directories() {
        let idx = index_with(&["x.txt", "y.txt"]);
        let mut se = Searcher::new();
        let q = Query::parse("Documents", MatchMode::Substring, Scope::Path);
        let res = se.search(&idx, &q);
        // Only entries under Documents (odd indices) plus the directory itself.
        assert_eq!(res.total, 2);
    }

    #[test]
    fn narrowing_reuses_previous_results() {
        let idx = index_with(&["report.txt", "reporting.doc", "notes.md"]);
        let mut se = Searcher::new();
        let a = se.search(&idx, &Query::parse("rep", MatchMode::Substring, Scope::Name));
        assert_eq!(a.plan, Plan::Scan);
        let b = se.search(&idx, &Query::parse("repor", MatchMode::Substring, Scope::Name));
        assert_eq!(b.plan, Plan::Narrowed);
        assert_eq!(b.total, 2);
        let c = se.search(&idx, &Query::parse("reporti", MatchMode::Substring, Scope::Name));
        assert_eq!(c.plan, Plan::Narrowed);
        assert_eq!(c.total, 1);
    }

    #[test]
    fn narrowing_agrees_with_full_scan() {
        let vol = synthetic_volume(0, "C:", 20_000);
        let idx = Index::new(vec![vol]);
        let mut inc = Searcher::new();
        let mut fresh = Searcher::new();
        for q in ["r", "re", "rep", "repo", "repor", "report"] {
            let a = inc.search(&idx, &Query::parse(q, MatchMode::Substring, Scope::Name));
            fresh.reset();
            let b = fresh.search(&idx, &Query::parse(q, MatchMode::Substring, Scope::Name));
            assert_eq!(a.total, b.total, "narrowed and fresh disagree on `{q}`");
        }
    }

    #[test]
    fn widening_does_not_narrow() {
        let idx = index_with(&["report.txt", "notes.md"]);
        let mut se = Searcher::new();
        se.search(&idx, &Query::parse("report", MatchMode::Substring, Scope::Name));
        let b = se.search(&idx, &Query::parse("rep", MatchMode::Substring, Scope::Name));
        assert_ne!(b.plan, Plan::Narrowed, "shortening the query must re-scan");
    }

    #[test]
    fn sorting_does_not_requery() {
        let idx = index_with(&["a", "b", "c"]); // sizes 1000, 2000, 3000
        let mut se = Searcher::new();
        let res = se.search(&idx, &Query::parse("ext:", MatchMode::Prefix, Scope::Name));
        let mut res2 = se.search(&idx, &Query::parse("", MatchMode::Prefix, Scope::Name));
        res2.sort_by(&idx, SortKey::Size, true);
        let names: Vec<String> =
            res2.window(&idx, 0, 3).iter().map(|r| idx.describe(*r).unwrap().name).collect();
        assert_eq!(names, ["c", "b", "a"]);
        let _ = res.window(&idx, 0, 1);
    }

    #[test]
    fn parses_sizes_and_dates() {
        assert_eq!(parse_size("10kb"), Some(10240));
        assert_eq!(parse_size("2mb"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size("512"), Some(512));
        // 1601-01-01 is FILETIME zero.
        assert_eq!(parse_date("1601-01-01"), Some(0));
        // 1970-01-01 is 11644473600 seconds later.
        assert_eq!(parse_date("1970-01-01"), Some(116_444_736_000_000_000));
        // Agrees with the formatter, which is the inverse computation.
        assert_eq!(crate::format_filetime(parse_date("2024-01-01").unwrap()), "2024-01-01 00:00:00");
    }

    #[test]
    fn wildcard_matcher_edge_cases() {
        assert!(wildcard_match(b"setup.exe", b"*.exe"));
        assert!(wildcard_match(b"setup.exe", b"setup*"));
        assert!(wildcard_match(b"setup.exe", b"*"));
        assert!(wildcard_match(b"SETUP.EXE", b"setup.???"));
        assert!(!wildcard_match(b"setup.exe", b"*.dll"));
        assert!(!wildcard_match(b"ab", b"a?c"));
        assert!(wildcard_match(b"aaa", b"a*a"));
    }
}
