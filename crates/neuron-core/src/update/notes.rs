// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Release notes from `CHANGELOG.md`. One `## <version>[ · <date>]` section per release: a
//! headline paragraph, optional `![alt](repo/path)` media lines, `### <group>` subsections of
//! `- **Title.** body` items, and an optional `### Thanks` block. `## Unreleased` and any heading
//! that is not a version are skipped. Rendering is plain text: markdown emphasis is stripped,
//! never interpreted as markup.

use super::version::Version;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Media {
    pub alt: String,
    /// Repo-relative path, validated by [`media_path_ok`].
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The bold lead (`**Title.**`), without its markers; empty for an item that has none.
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub date: Option<String>,
    pub headline: String,
    pub media: Vec<Media>,
    pub sections: Vec<Section>,
    pub thanks: Vec<String>,
    /// The section's own markdown, from its `##` line to the next one, trimmed.
    pub raw: String,
}

/// Every versioned release section in `md`, in file order.
pub fn parse(md: &str) -> Vec<Release> {
    split_sections(md).into_iter().filter_map(|raw| parse_section(&raw)).collect()
}

/// The section for `version`, if the changelog has one.
pub fn find(md: &str, version: &Version) -> Option<Release> {
    parse(md).into_iter().find(|r| &r.version == version)
}

fn split_sections(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    for line in md.lines() {
        if is_h2(line) {
            if let Some(done) = cur.take() {
                out.push(done.trim().to_string());
            }
            cur = Some(String::new());
        }
        if let Some(buf) = cur.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(done) = cur {
        out.push(done.trim().to_string());
    }
    out
}

fn is_h2(line: &str) -> bool {
    line.starts_with("## ")
}

/// The `##` label split into version and optional date: `0.1.8 · 2026-11-20`, `v0.1.7`,
/// `0.1.8 - 2026-11-20` and `[0.1.8] 2026-11-20` all read.
fn heading(line: &str) -> Option<(Version, Option<String>)> {
    let label = line.strip_prefix("## ")?.trim();
    let mut words = label.split(|c: char| c.is_whitespace() || c == '\u{b7}').filter(|w| !w.is_empty() && *w != "-" && *w != "\u{2014}");
    let first = words.next()?.trim_matches(|c| c == '[' || c == ']');
    let version = Version::parse(first)?;
    let date = words.next().map(str::to_string).filter(|d| d.len() == 10 && d.bytes().all(|b| b.is_ascii_digit() || b == b'-'));
    Some((version, date))
}

/// One section's markdown (as carried in a signed manifest) parsed into a [`Release`].
pub fn parse_section(raw: &str) -> Option<Release> {
    let mut lines = raw.lines();
    let (version, date) = heading(lines.next()?)?;
    let mut rel = Release {
        version,
        date,
        headline: String::new(),
        media: Vec::new(),
        sections: Vec::new(),
        thanks: Vec::new(),
        raw: raw.trim().to_string(),
    };
    enum At {
        Head,
        Group,
        Thanks,
    }
    let mut at = At::Head;
    for line in lines {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(h) = t.strip_prefix("### ") {
            let h = h.trim();
            if h.eq_ignore_ascii_case("thanks") {
                at = At::Thanks;
            } else {
                rel.sections.push(Section { title: h.to_string(), items: Vec::new() });
                at = At::Group;
            }
            continue;
        }
        if t.starts_with('#') {
            continue;
        }
        if t.starts_with("![") {
            // an image that is not a confined repo path is dropped, never shown as text
            if let (Some(m), At::Head) = (media_line(t), &at) {
                rel.media.push(m);
            }
            continue;
        }
        match at {
            At::Thanks => {
                let entry = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).unwrap_or(t);
                rel.thanks.push(plain(entry));
            }
            At::Head | At::Group => {
                if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
                    if rel.sections.is_empty() {
                        rel.sections.push(Section { title: String::new(), items: Vec::new() });
                    }
                    at = At::Group;
                    if let Some(sec) = rel.sections.last_mut() {
                        sec.items.push(split_item(item));
                    }
                } else if let Some(last) = rel.sections.last_mut().and_then(|s| s.items.last_mut()) {
                    last.body = join(&last.body, &plain(t));
                } else if matches!(at, At::Head) {
                    rel.headline = join(&rel.headline, &plain(t));
                }
            }
        }
    }
    rel.sections.retain(|s| !s.items.is_empty());
    Some(rel)
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else {
        format!("{a} {b}")
    }
}

fn split_item(item: &str) -> Item {
    let item = item.trim();
    if let Some(rest) = item.strip_prefix("**") {
        if let Some(end) = rest.find("**") {
            return Item { title: plain(&rest[..end]), body: plain(rest[end + 2..].trim()) };
        }
    }
    Item { title: String::new(), body: plain(item) }
}

fn media_line(t: &str) -> Option<Media> {
    let rest = t.strip_prefix("![")?;
    let (alt, rest) = rest.split_once("](")?;
    let path = rest.strip_suffix(')')?.trim();
    media_path_ok(path).then(|| Media { alt: plain(alt), path: path.to_string() })
}

/// A media path a release may reference: repo-relative, no traversal, no scheme, an image
/// extension Neuron decodes.
pub fn media_path_ok(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !path.is_empty()
        && path.len() <= 200
        && !path.starts_with('/')
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && path.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        && [".webp", ".png", ".gif", ".jpg", ".jpeg"].iter().any(|ext| lower.ends_with(ext))
}

/// Markdown inline text as plain text: emphasis and code markers dropped, `[text](url)` kept as
/// its text.
pub fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('[') {
            if let Some((text, tail)) = after.split_once("](") {
                if let Some(close) = tail.find(')') {
                    if !text.contains(']') {
                        out.push_str(text);
                        rest = &tail[close + 1..];
                        continue;
                    }
                }
            }
        }
        let mut chars = rest.chars();
        let Some(c) = chars.next() else { break };
        let next = chars.as_str();
        if c == '`' || ((c == '*' || c == '_') && next.starts_with(c)) {
            rest = if c == '`' { next } else { &next[1..] };
            continue;
        }
        out.push(c);
        rest = next;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Changelog

## Unreleased

- **Not yet.** Work in progress.

## 0.1.8 · 2026-11-20
The in-app updater.
![The update pill](docs/media/update-pill.webp)
![bad](../../etc/passwd.png)
![remote](https://example.com/x.png)
### New
- **Updates.** Neuron checks for releases and
  installs them in one click. (thanks @someone, #123)
- **Patch notes.** Shown inline.
### Fixed
- A plain item with `code` and [a link](https://x.y).
### Thanks
@someone, @another

## v0.1.7

### Security
- BOUND macros run behind a wall.
";

    #[test]
    fn parses_sections_media_items_and_thanks() {
        let all = parse(SAMPLE);
        assert_eq!(all.len(), 2, "Unreleased is skipped");
        let r = &all[0];
        assert_eq!(r.version.to_string(), "0.1.8");
        assert_eq!(r.date.as_deref(), Some("2026-11-20"));
        assert_eq!(r.headline, "The in-app updater.");
        assert_eq!(r.media, vec![Media { alt: "The update pill".into(), path: "docs/media/update-pill.webp".into() }]);
        assert_eq!(r.sections.len(), 2);
        assert_eq!(r.sections[0].title, "New");
        assert_eq!(r.sections[0].items[0].title, "Updates.");
        assert_eq!(r.sections[0].items[0].body, "Neuron checks for releases and installs them in one click. (thanks @someone, #123)");
        assert_eq!(r.sections[1].items[0], Item { title: String::new(), body: "A plain item with code and a link.".into() });
        assert_eq!(r.thanks, vec!["@someone, @another".to_string()]);
        assert!(r.raw.starts_with("## 0.1.8") && r.raw.ends_with("@someone, @another"));
        assert_eq!(all[1].version.to_string(), "0.1.7");
        assert_eq!(all[1].sections[0].title, "Security");
    }

    #[test]
    fn lowercase_headings_and_other_group_names_read_as_written() {
        let md = "## unreleased
- **soon.** x

## 0.1.8 · 2026-11-20
short, dry.
### better
- **luma-t.** learns your taste.
### odds and ends
- **misc.** y
### THANKS
- @a
- @b
";
        let all = parse(md);
        assert_eq!(all.len(), 1);
        let r = &all[0];
        assert_eq!(r.sections.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), vec!["better", "odds and ends"]);
        assert_eq!(r.sections[0].items[0].title, "luma-t.");
        assert_eq!(r.thanks, vec!["@a".to_string(), "@b".to_string()]);
    }

    #[test]
    fn a_section_round_trips_through_its_raw_text() {
        let r = find(SAMPLE, &Version::parse("0.1.8").unwrap()).unwrap();
        assert_eq!(parse_section(&r.raw), Some(r));
        assert!(find(SAMPLE, &Version::parse("9.9.9").unwrap()).is_none());
    }

    #[test]
    fn malformed_input_degrades_without_panicking() {
        for junk in ["", "##", "## \n", "## not-a-version\n- x", "### orphan\n- y", "## 1.2.3\n![](", "## 1.2.3\n- **unclosed", "## 1.2.3\n[[[](", "\u{0}\u{ffff}"] {
            let _ = parse(junk);
            let _ = parse_section(junk);
        }
        let r = parse_section("## 1.2.3\n- **unclosed bold").unwrap();
        assert_eq!(r.sections[0].items[0].body, "unclosed bold");
        assert!(parse_section("## Unreleased\n- x").is_none());
    }

    #[test]
    fn the_shipped_changelog_parses() {
        let md = include_str!("../../../../CHANGELOG.md");
        let all = parse(md);
        assert!(!all.is_empty(), "CHANGELOG.md has at least one versioned release");
        for r in &all {
            assert!(r.media.iter().all(|m| media_path_ok(&m.path)));
        }
    }

    #[test]
    fn media_paths_are_confined_to_the_repo() {
        assert!(media_path_ok("docs/media/a-b_c.webp"));
        for bad in ["/abs.png", "a/../b.png", "./a.png", "a//b.png", "docs/x.svg", "https://x/y.png", "a\\b.png", ""] {
            assert!(!media_path_ok(bad), "{bad}");
        }
    }
}
