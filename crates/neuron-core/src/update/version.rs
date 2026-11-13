// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Release versions: `v1.2.3`, `1.2.3-rc.1`, with semver precedence (a pre-release sorts below
//! its release; numeric identifiers compare numerically and below alphanumeric ones).

use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ident {
    Num(u64),
    Alpha(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<Ident>,
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3`, `1.2.3-rc.1` or `1.2.3+build` (build metadata is ignored, as semver
    /// precedence ignores it). Leading zeros and empty identifiers are refused.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split_once('+').map_or(s, |(core, _)| core);
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let num = |p: Option<&str>| -> Option<u64> {
            let p = p?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
                return None;
            }
            p.parse().ok()
        };
        let major = num(parts.next())?;
        let minor = num(parts.next())?;
        let patch = num(parts.next())?;
        if parts.next().is_some() {
            return None;
        }
        let mut idents = Vec::new();
        if let Some(pre) = pre {
            for id in pre.split('.') {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return None;
                }
                if id.bytes().all(|b| b.is_ascii_digit()) {
                    if id.len() > 1 && id.starts_with('0') {
                        return None;
                    }
                    idents.push(Ident::Num(id.parse().ok()?));
                } else {
                    idents.push(Ident::Alpha(id.to_string()));
                }
            }
        }
        Some(Self { major, minor, patch, pre: idents })
    }

    /// The version this binary was built as.
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Self { major: 0, minor: 0, patch: 0, pre: Vec::new() })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// The release tag spelling, `v1.2.3[-pre]`.
    pub fn tag(&self) -> String {
        format!("v{self}")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                Ident::Num(n) => write!(f, "{n}")?,
                Ident::Alpha(a) => f.write_str(a)?,
            }
        }
        Ok(())
    }
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
            (Ident::Num(_), Ident::Alpha(_)) => Ordering::Less,
            (Ident::Alpha(_), Ident::Num(_)) => Ordering::Greater,
            (Ident::Alpha(a), Ident::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} parses"))
    }

    #[test]
    fn parses_tags_and_plain_versions() {
        assert_eq!(v("v0.1.8"), v("0.1.8"));
        assert_eq!(v("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
        assert_eq!(v("1.2.3+build.7"), v("1.2.3"));
        assert_eq!(v("v0.1.8").tag(), "v0.1.8");
        for bad in ["", "1.2", "1.2.3.4", "01.2.3", "1.2.x", "1.2.3-", "1.2.3-a..b", "1.2.3-01", "v", "1.2.3-é"] {
            assert!(Version::parse(bad).is_none(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn semver_precedence_including_prereleases() {
        let order = [
            "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2",
            "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.0.1", "1.1.0", "2.0.0",
        ];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert!(v("0.1.10") > v("0.1.9"), "numeric, not lexical");
        assert_eq!(v("0.1.7").cmp(&v("v0.1.7")), Ordering::Equal);
    }

    #[test]
    fn the_running_build_has_a_version() {
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
