//! Port of libalpm's `alpm_pkg_vercmp` (epoch:version-release ordering).

use std::cmp::Ordering;

pub fn vercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (e1, v1, r1) = split_evr(a);
    let (e2, v2, r2) = split_evr(b);
    rpmvercmp(e1, e2)
        .then_with(|| rpmvercmp(v1, v2))
        .then_with(|| match (r1, r2) {
            (Some(r1), Some(r2)) => rpmvercmp(r1, r2),
            _ => Ordering::Equal,
        })
}

fn split_evr(s: &str) -> (&str, &str, Option<&str>) {
    let (epoch, rest) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) if s.as_bytes()[i] == b':' => (&s[..i], &s[i + 1..]),
        _ => ("0", s),
    };
    let epoch = if epoch.is_empty() { "0" } else { epoch };
    match rest.rfind('-') {
        Some(i) => (epoch, &rest[..i], Some(&rest[i + 1..])),
        None => (epoch, rest, None),
    }
}

fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (si, sj) = (i, j);
        while i < a.len() && !a[i].is_ascii_alphanumeric() {
            i += 1;
        }
        while j < b.len() && !b[j].is_ascii_alphanumeric() {
            j += 1;
        }
        if i >= a.len() || j >= b.len() {
            break;
        }
        // Different separator lengths decide.
        if i - si != j - sj {
            return (i - si).cmp(&(j - sj));
        }
        let isnum = a[i].is_ascii_digit();
        let seg = |s: &[u8], mut k: usize| {
            let start = k;
            while k < s.len()
                && if isnum {
                    s[k].is_ascii_digit()
                } else {
                    s[k].is_ascii_alphabetic()
                }
            {
                k += 1;
            }
            (start, k)
        };
        let (a0, a1) = seg(a, i);
        let (b0, b1) = seg(b, j);
        if b0 == b1 {
            // Numeric segments are newer than alpha ones.
            return if isnum {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let (mut x, mut y) = (&a[a0..a1], &b[b0..b1]);
        if isnum {
            while x.first() == Some(&b'0') {
                x = &x[1..];
            }
            while y.first() == Some(&b'0') {
                y = &y[1..];
            }
            if x.len() != y.len() {
                return x.len().cmp(&y.len());
            }
        }
        match x.cmp(y) {
            Ordering::Equal => {}
            o => return o,
        }
        i = a1;
        j = b1;
    }
    let rest_a = &a[i.min(a.len())..];
    let rest_b = &b[j.min(b.len())..];
    if rest_a.is_empty() && rest_b.is_empty() {
        return Ordering::Equal;
    }
    // An empty remainder beats a remaining alpha string, loses to anything else.
    let a_alpha = rest_a.first().is_some_and(|c| c.is_ascii_alphabetic());
    let b_alpha = rest_b.first().is_some_and(|c| c.is_ascii_alphabetic());
    if (rest_a.is_empty() && !b_alpha) || a_alpha {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::*;

    #[test]
    fn matches_alpm_vercmp() {
        // Cases from pacman's test/util/vercmptest.sh
        let cases = [
            ("1.5.0", "1.5.0", Equal),
            ("1.5.1", "1.5.0", Greater),
            ("1.5.1", "1.5", Greater),
            ("1.5.0-1", "1.5.0-1", Equal),
            ("1.5.0-1", "1.5.0-2", Less),
            ("1.5.0-1", "1.5.1-1", Less),
            ("1.5.0-2", "1.5.1-1", Less),
            ("1.5-1", "1.5", Equal),
            ("1.1-1", "1.1", Equal),
            ("1.0-1", "1.1", Less),
            ("1.1-1", "1.0", Greater),
            ("1.5b-1", "1.5-1", Less),
            ("1.5b", "1.5", Less),
            ("1.5", "1.5b", Greater),
            ("1.5b-1", "1.5b", Equal),
            ("1.5-1", "1.5b", Greater),
            ("1.0a", "1.0alpha", Less),
            ("1.0alpha", "1.0b", Less),
            ("1.0b", "1.0beta", Less),
            ("1.0beta", "1.0rc", Less),
            ("1.0rc", "1.0", Less),
            ("1.0.a", "1.0.1", Less),
            ("1.0.1", "1.0.a", Greater),
            ("1.0.a", "1.0.alpha", Less),
            ("1.0", "1.0.0", Less),
            ("1.0.0", "1.0", Greater),
            ("1.0.1", "1.0", Greater),
            ("1.0", "1.0.1", Less),
            ("1.0", "1.0a", Greater),
            ("1:1.0", "1.0", Greater),
            ("1.0", "1:1.0", Less),
            ("1:1.0", "2:1.0", Less),
            ("1:1.0-1", "1:1.0-2", Less),
            ("0:1.0", "1.0", Equal),
            ("1.0_1", "1.0.1", Equal),
            ("1.0__1", "1.0.1", Greater),
            ("2.0", "10.0", Less),
            ("1.010", "1.10", Equal),
            ("1.2.3-1", "1.2.3-1.1", Less),
        ];
        for (a, b, want) in cases {
            assert_eq!(vercmp(a, b), want, "vercmp({a}, {b})");
            assert_eq!(vercmp(b, a), want.reverse(), "vercmp({b}, {a})");
        }
    }
}
