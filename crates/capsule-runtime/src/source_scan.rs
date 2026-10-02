//! This crate's own source, read by the tests that pin where in the crate a construct may appear.

use std::path::Path;

/// Every `.rs` file under this crate's `src`, as `(path relative to src, contents)`, sorted by
/// path.
pub(crate) fn crate_sources() -> Vec<(String, String)> {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut sources = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("the crate's src directory is readable") {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                sources.push((
                    relative,
                    std::fs::read_to_string(&path).expect("a readable source file"),
                ));
            }
        }
    }
    assert!(
        sources.len() > 10,
        "the source sweep found only {} files, so it is not sweeping the crate",
        sources.len()
    );
    sources.sort();
    sources
}

/// The part of a source file above its `mod tests`: everything before the first top-level
/// `#[cfg(test)]` whose item, past any further attributes, is `mod tests`.
///
/// Cut there, not at the first `#[cfg(test)]`: several files carry a test-only helper or a
/// `test_support` module far above their `mod tests`, with production code after it, and cutting
/// at the bare attribute would leave that code unscanned. Those test-only items stay in, so a
/// sweep over them errs strict.
pub(crate) fn production_part(source: &str) -> &str {
    let mut offset = 0;
    let mut lines = source.split_inclusive('\n');
    while let Some(line) = lines.next() {
        if line.trim_end() == "#[cfg(test)]" {
            let item = lines
                .clone()
                .map(str::trim_end)
                .find(|next| !next.starts_with("#["));
            if item.is_some_and(|item| item.starts_with("mod tests")) {
                return &source[..offset];
            }
        }
        offset += line.len();
    }
    source
}
