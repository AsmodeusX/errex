//! Release ordering.
//!
//! Releases arrive as free-form strings (`package@version+build` is the
//! Sentry convention). The daemon needs to know whether one release is newer
//! than another to decide whether an event from it regresses a resolved
//! issue or is the first sighting in a newer release. [`compare`] decides
//! from the strings alone where it can:
//!
//! 1. Both carry an integer build (`…+45`): the larger build is newer.
//!    Builds are the one number our app guarantees only ever increases.
//! 2. Both carry a dotted numeric version (`1.2.3`, `pkg@1.2.3-rc.1`):
//!    semantic-version order, build metadata ignored.
//! 3. Otherwise `None`, and the caller falls back to when each release
//!    first reported anything.

use std::cmp::Ordering;

/// The integer after the last `+`, when it is all digits.
pub fn build_number(release: &str) -> Option<u64> {
    let (_, build) = release.rsplit_once('+')?;
    if build.is_empty() || !build.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    build.parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
struct Version<'a> {
    core: Vec<u64>,
    pre: Option<&'a str>,
}

/// `pkg@1.2.3-pre+meta` → core `[1,2,3]`, pre `pre`. At least two numeric
/// components are required so an opaque id (a commit hash that happens to
/// be all digits, a bare counter) is not mistaken for a version.
fn version(release: &str) -> Option<Version<'_>> {
    let v = release.rsplit_once('@').map_or(release, |(_, v)| v);
    let v = v.split_once('+').map_or(v, |(v, _)| v);
    let v = v.strip_prefix('v').unwrap_or(v);
    let (core, pre) = match v.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (v, None),
    };
    let core: Vec<u64> = core
        .split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                None
            } else {
                part.parse().ok()
            }
        })
        .collect::<Option<_>>()?;
    if core.len() < 2 || pre == Some("") {
        return None;
    }
    Some(Version { core, pre })
}

fn compare_core(a: &[u64], b: &[u64]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let ord = a
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&b.get(i).copied().unwrap_or(0));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Semantic-version precedence of pre-release identifiers: none outranks
/// any; numeric identifiers compare numerically and sort below
/// alphanumeric ones; a shorter list that is a prefix sorts first.
fn compare_pre(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let mut ai = a.split('.');
            let mut bi = b.split('.');
            loop {
                match (ai.next(), bi.next()) {
                    (None, None) => return Ordering::Equal,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(x), Some(y)) => {
                        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => x.cmp(y),
                        };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                }
            }
        }
    }
}

/// Order two releases from their strings. `None` means the strings do not
/// decide it (different formats, or no version in either); the caller then
/// orders by first-seen time.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    if a == b {
        return Some(Ordering::Equal);
    }
    let (va, vb) = (version(a), version(b));
    if let (Some(ba), Some(bb)) = (build_number(a), build_number(b)) {
        if ba != bb {
            return Some(ba.cmp(&bb));
        }
    }
    let (va, vb) = (va?, vb?);
    match compare_core(&va.core, &vb.core).then_with(|| compare_pre(va.pre, vb.pre)) {
        Ordering::Equal => None,
        ord => Some(ord),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_number_is_the_integer_after_the_last_plus() {
        assert_eq!(build_number("zeus-suite@0.0.15+12"), Some(12));
        assert_eq!(build_number("1.2.3+45"), Some(45));
        assert_eq!(build_number("1.2.3+exp.sha.5114f85"), None);
        assert_eq!(build_number("1.2.3+"), None);
        assert_eq!(build_number("1.2.3"), None);
    }

    #[test]
    fn the_build_number_decides_whatever_the_version_says() {
        assert_eq!(
            compare("zeus-suite@0.0.15+12", "zeus-suite@0.0.14+11"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare("zeus-suite@0.0.9+10", "zeus-suite@0.0.10+9"),
            Some(Ordering::Greater),
            "9 < 10 as builds even though 0.0.9 < 0.0.10 as versions"
        );
        assert_eq!(
            compare("zeus-suite@1.0.0+100", "zeus-suite@1.0.0+99"),
            Some(Ordering::Greater),
            "numeric, not lexical"
        );
    }

    #[test]
    fn semver_decides_without_builds() {
        assert_eq!(compare("app@1.10.0", "app@1.9.3"), Some(Ordering::Greater));
        assert_eq!(compare("1.0.0-rc.1", "1.0.0"), Some(Ordering::Less));
        assert_eq!(compare("1.0.0-rc.2", "1.0.0-rc.10"), Some(Ordering::Less));
        assert_eq!(
            compare("1.0.0-alpha", "1.0.0-alpha.1"),
            Some(Ordering::Less)
        );
        assert_eq!(compare("1.0.0-1", "1.0.0-alpha"), Some(Ordering::Less));
        assert_eq!(compare("v2.0", "1.9.9"), Some(Ordering::Greater));
        assert_eq!(compare("app@1.2.3+45", "app@1.2.4"), Some(Ordering::Less));
    }

    #[test]
    fn strings_that_do_not_decide_return_none() {
        assert_eq!(compare("a", "a"), Some(Ordering::Equal));
        assert_eq!(compare("5114f85", "9ac0e21"), None);
        assert_eq!(compare("app@1.2.3", "5114f85"), None);
        assert_eq!(compare("123", "124"), None, "a bare number is opaque");
        assert_eq!(compare("1.2.3+a", "1.2.3+b"), None);
    }
}
