//! Case-insensitive comparison without a second copy of every name.
//!
//! The index stores exactly one byte string per entry (the display name, UTF-8).
//! Ordering and matching are defined over the *folded* form of that name, and the
//! fold happens during comparison rather than being materialised. For a 5M-entry
//! corpus this saves roughly 110 MB versus keeping a parallel folded-key blob.
//!
//! Almost every filename on a Windows volume is pure ASCII, so the hot loop is a
//! branchless table lookup per byte. The moment either side goes non-ASCII we drop
//! into a char-wise loop using Unicode simple lowercase mapping. Both paths agree
//! on ASCII, so the resulting order is total and consistent.

use std::cmp::Ordering;

/// `A`..`Z` -> `a`..`z`, identity elsewhere. Indexed by byte, so no branches.
static ASCII_FOLD: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = if i >= b'A' as usize && i <= b'Z' as usize { (i as u8) + 32 } else { i as u8 };
        i += 1;
    }
    t
};

#[inline(always)]
pub fn fold_byte(b: u8) -> u8 {
    ASCII_FOLD[b as usize]
}

#[inline]
pub fn is_ascii_bytes(s: &[u8]) -> bool {
    s.iter().all(|b| *b < 0x80)
}

/// Fold a whole string into a caller-provided buffer. Used for query terms, which
/// are folded exactly once per keystroke rather than once per candidate.
pub fn fold_into(s: &str, out: &mut Vec<u8>) {
    out.clear();
    if s.is_ascii() {
        out.extend(s.as_bytes().iter().map(|b| fold_byte(*b)));
        return;
    }
    for ch in s.chars() {
        if ch.is_ascii() {
            out.push(fold_byte(ch as u8));
        } else {
            for lc in ch.to_lowercase() {
                let mut buf = [0u8; 4];
                out.extend_from_slice(lc.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
}

pub fn fold_to_string(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len());
    fold_into(s, &mut v);
    v
}

/// Compare `a` (a raw stored name) against `b` (an already-folded needle).
///
/// Only `a` is folded here; the query side is folded once up front by the caller.
#[inline]
pub fn fold_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().min(b.len());
    let mut i = 0;
    // ASCII fast path: runs to completion for the overwhelming majority of names.
    while i < n {
        let x = a[i];
        if x >= 0x80 {
            return fold_cmp_unicode(&a[i..], &b[i..]);
        }
        let fx = ASCII_FOLD[x as usize];
        let fy = b[i];
        if fx != fy {
            return fx.cmp(&fy);
        }
        i += 1;
    }
    if i < a.len() && a[i] >= 0x80 {
        return fold_cmp_unicode(&a[i..], &b[i..]);
    }
    a.len().cmp(&b.len())
}

/// Slow path from the first non-ASCII byte onwards. `a` is unfolded, `b` is folded.
#[cold]
fn fold_cmp_unicode(a: &[u8], b: &[u8]) -> Ordering {
    let sa = String::from_utf8_lossy(a);
    let mut fa = Vec::with_capacity(a.len() + 8);
    fold_into(&sa, &mut fa);
    fa.as_slice().cmp(b)
}

/// Does the stored name `a` begin with the already-folded `prefix`?
#[inline]
pub fn fold_starts_with(a: &[u8], prefix: &[u8]) -> bool {
    if a.len() < prefix.len() {
        // A shorter unfolded name can still fold longer (e.g. 'ß' -> "ss"), but only
        // when non-ASCII is involved; check that before rejecting.
        if is_ascii_bytes(a) {
            return false;
        }
        let sa = String::from_utf8_lossy(a);
        let mut fa = Vec::with_capacity(a.len() + 8);
        fold_into(&sa, &mut fa);
        return fa.starts_with(prefix);
    }
    let mut i = 0;
    while i < prefix.len() {
        let x = a[i];
        if x >= 0x80 {
            let sa = String::from_utf8_lossy(a);
            let mut fa = Vec::with_capacity(a.len() + 8);
            fold_into(&sa, &mut fa);
            return fa.starts_with(prefix);
        }
        if ASCII_FOLD[x as usize] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Folded substring search. Uses `memchr` to skip to candidate start bytes rather
/// than testing every offset, which is what keeps the non-prefix scan near memory
/// bandwidth.
#[inline]
pub fn fold_contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() && is_ascii_bytes(hay) {
        return false;
    }
    let first = needle[0];
    // Folded needles are lowercase, so an ASCII haystack byte matches if it equals
    // the needle byte or its uppercase twin.
    let upper = if first.is_ascii_lowercase() { first - 32 } else { first };
    let mut base = 0usize;
    while base < hay.len() {
        let rest = &hay[base..];
        let hit = if upper == first {
            memchr::memchr(first, rest)
        } else {
            memchr::memchr2(first, upper, rest)
        };
        let Some(off) = hit else { break };
        let at = base + off;
        if fold_starts_with(&hay[at..], needle) {
            return true;
        }
        base = at + 1;
    }
    // Non-ASCII haystacks can fold to something that matches even when no raw byte did.
    if !is_ascii_bytes(hay) {
        let sa = String::from_utf8_lossy(hay);
        let mut fa = Vec::with_capacity(hay.len() + 8);
        fold_into(&sa, &mut fa);
        return fa.windows(needle.len()).any(|w| w == needle);
    }
    false
}

/// Compare two *raw* (unfolded) stored names. Used to order the sorted index.
#[inline]
pub fn fold_cmp_raw(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i < n {
        let x = a[i];
        let y = b[i];
        if x >= 0x80 || y >= 0x80 {
            return fold_cmp_raw_unicode(&a[i..], &b[i..]);
        }
        let fx = ASCII_FOLD[x as usize];
        let fy = ASCII_FOLD[y as usize];
        if fx != fy {
            return fx.cmp(&fy);
        }
        i += 1;
    }
    if (i < a.len() && a[i] >= 0x80) || (i < b.len() && b[i] >= 0x80) {
        return fold_cmp_raw_unicode(&a[i..], &b[i..]);
    }
    a.len().cmp(&b.len())
}

#[cold]
fn fold_cmp_raw_unicode(a: &[u8], b: &[u8]) -> Ordering {
    let mut fa = Vec::with_capacity(a.len() + 8);
    let mut fb = Vec::with_capacity(b.len() + 8);
    fold_into(&String::from_utf8_lossy(a), &mut fa);
    fold_into(&String::from_utf8_lossy(b), &mut fb);
    fa.cmp(&fb)
}

#[cfg(test)]
mod raw_tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn raw_compare_is_case_insensitive() {
        assert_eq!(fold_cmp_raw(b"README", b"readme"), Equal);
        assert_eq!(fold_cmp_raw(b"apple", b"Banana"), Less);
        assert_eq!(fold_cmp_raw(b"Zed", b"alpha"), Greater);
        assert_eq!(fold_cmp_raw("École".as_bytes(), "école".as_bytes()), Equal);
    }

    #[test]
    fn raw_and_needle_compare_agree() {
        for (x, y) in [("Cargo.toml", "cargo.lock"), ("a", "ab"), ("Zoo", "zoo")] {
            assert_eq!(fold_cmp_raw(x.as_bytes(), y.as_bytes()), fold_cmp(x.as_bytes(), &fold_to_string(y)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn ascii_case_insensitive_order() {
        assert_eq!(fold_cmp(b"README.md", &fold_to_string("readme.md")), Equal);
        assert_eq!(fold_cmp(b"Apple", &fold_to_string("banana")), Less);
        assert_eq!(fold_cmp(b"zeta", &fold_to_string("Alpha")), Greater);
        // Shorter is less when one is a prefix of the other.
        assert_eq!(fold_cmp(b"log", &fold_to_string("logs")), Less);
    }

    #[test]
    fn prefix_matching() {
        assert!(fold_starts_with(b"Cargo.toml", &fold_to_string("car")));
        assert!(fold_starts_with(b"Cargo.toml", &fold_to_string("CARGO.")));
        assert!(!fold_starts_with(b"Cargo.toml", &fold_to_string("argo")));
        assert!(fold_starts_with(b"anything", &fold_to_string("")));
    }

    #[test]
    fn substring_matching() {
        assert!(fold_contains(b"MyDocument.PDF", &fold_to_string("document")));
        assert!(fold_contains(b"MyDocument.PDF", &fold_to_string(".pdf")));
        assert!(!fold_contains(b"MyDocument.PDF", &fold_to_string("xls")));
        assert!(fold_contains(b"abc", &fold_to_string("")));
    }

    #[test]
    fn unicode_folds() {
        assert_eq!(fold_cmp("ÉCOLE.txt".as_bytes(), &fold_to_string("école.txt")), Equal);
        assert!(fold_starts_with("Ünicode".as_bytes(), &fold_to_string("üni")));
        assert!(fold_contains("Straße.txt".as_bytes(), &fold_to_string("straße")));
    }

    #[test]
    fn order_is_total_and_consistent() {
        let mut names: Vec<&str> = vec!["b.txt", "A.txt", "a.TXT", "C", "É", "z"];
        names.sort_by(|x, y| fold_cmp(x.as_bytes(), &fold_to_string(y)));
        // Sorting must not panic and must be self-consistent when re-checked.
        for w in names.windows(2) {
            assert_ne!(fold_cmp(w[0].as_bytes(), &fold_to_string(w[1])), Greater);
        }
    }
}
