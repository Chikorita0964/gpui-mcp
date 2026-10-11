//! A strict applier for the unified diffs in `vendor/patches/`.
//!
//! It behaves like `patch -p1 --fuzz=0 --forward`: every context and removed
//! line must match exactly. A hunk may apply at an offset from the line it
//! names, as with GNU patch, but never with mismatched context. It supports
//! what `git diff` emits for these patches: changed files and new files.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

/// A crate's files, keyed by path relative to the crate root.
pub(crate) type Files = BTreeMap<PathBuf, Vec<u8>>;

struct Hunk {
    old_start: usize,
    old: Vec<String>,
    new: Vec<String>,
}

struct FilePatch {
    path: PathBuf,
    created: bool,
    hunks: Vec<Hunk>,
}

/// Apply `patch` to `files`, or fail without a partial result being usable.
pub(crate) fn apply(files: &mut Files, patch: &str) -> Result<()> {
    for file in parse(patch)? {
        let display = file.path.display().to_string();
        let mut lines = if file.created {
            if files.contains_key(&file.path) {
                bail!("{display}: the patch creates it, but it already exists");
            }
            Vec::new()
        } else {
            let content = files.get(&file.path).with_context(|| {
                format!("{display}: the patch changes it, but it doesn't exist")
            })?;
            let text = String::from_utf8(content.clone())
                .with_context(|| format!("{display}: not UTF-8"))?;
            text.split_inclusive('\n').map(str::to_owned).collect()
        };
        let mut shift: isize = 0;
        for (index, hunk) in file.hunks.iter().enumerate() {
            let named =
                if hunk.old.is_empty() { hunk.old_start } else { hunk.old_start.saturating_sub(1) };
            let expected = named.saturating_add_signed(shift);
            let at = find(&lines, &hunk.old, expected)
                .with_context(|| format!("{display}: hunk {} does not apply", index + 1))?;
            lines.splice(at..at + hunk.old.len(), hunk.new.iter().cloned());
            // Later hunks name lines in the original file, so carry this
            // hunk's offset and the lines it added or removed.
            shift =
                signed(at)? - signed(named)? + signed(hunk.new.len())? - signed(hunk.old.len())?;
        }
        files.insert(file.path, lines.concat().into_bytes());
    }
    Ok(())
}

fn signed(value: usize) -> Result<isize> {
    Ok(isize::try_from(value)?)
}

/// Find where `old` matches exactly, trying `expected` first and then the
/// nearest offsets on either side.
fn find(lines: &[String], old: &[String], expected: usize) -> Option<usize> {
    let last = lines.len().checked_sub(old.len())?;
    let matches = |at: usize| lines[at..at + old.len()] == *old;
    for distance in 0..=last.max(expected) {
        if let Some(at) = expected.checked_sub(distance)
            && at <= last
            && matches(at)
        {
            return Some(at);
        }
        let at = expected + distance;
        if distance > 0 && at <= last && matches(at) {
            return Some(at);
        }
    }
    None
}

fn parse(patch: &str) -> Result<Vec<FilePatch>> {
    let mut files = Vec::new();
    let mut lines = patch.lines().peekable();
    let mut current: Option<FilePatch> = None;
    while let Some(line) = lines.next() {
        if line.starts_with("diff --git ") {
            files.extend(current.take());
        } else if line.starts_with("new file mode ") {
            current.get_or_insert_with(empty).created = true;
        } else if line.starts_with("--- ") {
            // A patch without `diff --git` lines starts each file here.
            if current.as_ref().is_some_and(|file| !file.path.as_os_str().is_empty()) {
                files.extend(current.take());
            }
        } else if let Some(target) = line.strip_prefix("+++ ") {
            let relative = target
                .strip_prefix("b/")
                .with_context(|| format!("unsupported target path: {target}"))?;
            current.get_or_insert_with(empty).path = PathBuf::from(relative);
        } else if line.starts_with("deleted file mode ")
            || line.starts_with("rename ")
            || line.starts_with("Binary files ")
        {
            bail!("unsupported patch line: {line}");
        } else if let Some(header) = line.strip_prefix("@@ ") {
            let file =
                current.as_mut().with_context(|| format!("hunk before any file header: {line}"))?;
            let (old_start, old_count, new_count) = parse_header(header)?;
            let mut hunk = Hunk { old_start, old: Vec::new(), new: Vec::new() };
            while hunk.old.len() < old_count || hunk.new.len() < new_count {
                let body = lines.next().context("patch ends inside a hunk")?;
                let (kind, text) = match body.chars().next() {
                    // Some tools strip the space from an empty context line.
                    None => (' ', ""),
                    Some(kind) => (kind, &body[1..]),
                };
                let text = format!("{text}\n");
                match kind {
                    ' ' => {
                        hunk.old.push(text.clone());
                        hunk.new.push(text);
                    }
                    '-' => hunk.old.push(text),
                    '+' => hunk.new.push(text),
                    _ => bail!("unexpected line in hunk: {body}"),
                }
                if lines.peek().is_some_and(|next| next.starts_with('\\')) {
                    lines.next();
                    strip_newline(kind, &mut hunk);
                }
            }
            file.hunks.push(hunk);
        }
    }
    files.extend(current);
    for file in &files {
        if file.path.as_os_str().is_empty() {
            bail!("a file in the patch has no +++ header");
        }
    }
    Ok(files)
}

fn empty() -> FilePatch {
    FilePatch { path: PathBuf::new(), created: false, hunks: Vec::new() }
}

/// Apply a `\ No newline at end of file` marker to the line before it.
fn strip_newline(kind: char, hunk: &mut Hunk) {
    let trim = |line: Option<&mut String>| {
        if let Some(line) = line {
            line.pop();
        }
    };
    match kind {
        '-' => trim(hunk.old.last_mut()),
        '+' => trim(hunk.new.last_mut()),
        _ => {
            trim(hunk.old.last_mut());
            trim(hunk.new.last_mut());
        }
    }
}

/// Parse `-a,b +c,d @@ ...` into the old start and both line counts.
fn parse_header(header: &str) -> Result<(usize, usize, usize)> {
    let mut ranges = header.split(' ');
    let old = ranges.next().and_then(|range| range.strip_prefix('-'));
    let new = ranges.next().and_then(|range| range.strip_prefix('+'));
    let (Some(old), Some(new)) = (old, new) else {
        bail!("malformed hunk header: @@ {header}");
    };
    let range = |range: &str| -> Result<(usize, usize)> {
        let (start, count) = range.split_once(',').unwrap_or((range, "1"));
        Ok((start.parse()?, count.parse()?))
    };
    let (old_start, old_count) =
        range(old).with_context(|| format!("malformed hunk header: @@ {header}"))?;
    let (_, new_count) =
        range(new).with_context(|| format!("malformed hunk header: @@ {header}"))?;
    Ok((old_start, old_count, new_count))
}

#[cfg(test)]
mod tests {
    use super::{Files, apply};
    use std::path::PathBuf;

    fn files(content: &str) -> Files {
        Files::from([(PathBuf::from("src/a.rs"), content.as_bytes().to_vec())])
    }

    #[test]
    fn applies_changes_new_files_and_offsets() -> anyhow::Result<()> {
        let mut tree = files("zero\none\ntwo\nthree\n");
        let patch = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
                     @@ -1,2 +1,2 @@\n one\n-two\n+TWO\n\
                     diff --git a/src/b.rs b/src/b.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/b.rs\n\
                     @@ -0,0 +1 @@\n+new\n";
        apply(&mut tree, patch)?;
        assert_eq!(tree[&PathBuf::from("src/a.rs")], b"zero\none\nTWO\nthree\n");
        assert_eq!(tree[&PathBuf::from("src/b.rs")], b"new\n");
        Ok(())
    }

    #[test]
    fn rejects_mismatched_context() {
        let mut tree = files("one\ntwo\n");
        let patch = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n one\n-three\n+3\n";
        assert!(apply(&mut tree, patch).is_err());
    }

    #[test]
    fn honours_missing_final_newline() -> anyhow::Result<()> {
        let mut tree = files("one\ntwo");
        let patch =
            "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -2 +2 @@\n-two\n\\ No newline at end of file\n+2\n";
        apply(&mut tree, patch)?;
        assert_eq!(tree[&PathBuf::from("src/a.rs")], b"one\n2\n");
        Ok(())
    }
}
