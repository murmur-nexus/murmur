//! Every docs pointer `mur`, the capsule runtime and the escape-conformance harness can print
//! opens a published page.
//!
//! The scan covers every `.rs` file under `crates/` except directories named `target` or `tests`
//! and files named `*_tests.rs`. Before scanning, each file is parsed with `syn` and stripped of
//! doc attributes (`///`, `//!`), of items, impl items, trait items and block-level items under a
//! `cfg` whose predicate holds only in test builds (`cfg(test)`, `cfg(all(test, …))`), and of
//! functions marked `#[test]` or `#[<path>::test]`. `//` comments do not survive parsing. What is
//! left is the code that ships, scanned token by token, macro bodies and attribute arguments
//! included:
//!
//! - no string literal names `docs/content`, the repo-relative source path a reader cannot open;
//! - every `docs_reference_url!("<page>/")` or `docs_reference_url!("<page>/#<anchor>")` names a
//!   page under `docs/content/reference/` that `exclude_docs` in `docs/mkdocs.yml` does not drop,
//!   and that carries `{ #<anchor> }`;
//! - every `diagnostic_link("<code>")` names a code whose `{ #<code lowercased> }` entry exists in
//!   `diagnostics.md`;
//! - every `W-*` code literal has that entry too. A warning prints its link from the code through
//!   `diagnostic_link(code)`, so the code literal is the only place the link can be checked.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::punctuated::Punctuated;
use syn::visit_mut::{self, VisitMut};
use syn::{Attribute, ImplItem, Item, Meta, Stmt, Token, TraitItem};

/// The fewest literal docs-link invocations the walk must reach, and the fewest crates they must
/// span, for the scan to count as having seen the printed pointers at all.
const MIN_CHECKED_INVOCATIONS: usize = 20;
const MIN_CHECKED_CRATES: usize = 3;

/// What one file's shipping code says about docs links.
#[derive(Debug, Default)]
struct Scan {
    /// String literals that contain `docs/content`, as written in the source.
    source_paths: Vec<String>,
    /// The literal argument of each `docs_reference_url!("…")`.
    reference_paths: Vec<String>,
    /// The literal argument of each `diagnostic_link("…")`.
    diagnostic_codes: Vec<String>,
    /// Every string literal that is a whole `W-*` code, such as `"W-SEC-024"`.
    warning_codes: Vec<String>,
}

struct ScannedFile {
    /// Relative to `crates/`.
    path: PathBuf,
    /// The directory, relative to `crates/`, of the nearest `Cargo.toml` above the file.
    krate: String,
    scan: Scan,
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn reference_dir() -> PathBuf {
    crates_dir().join("../docs/content/reference")
}

/// Parses `source`, strips everything that does not ship, and scans the rest.
fn scan_source(source: &str) -> syn::Result<Scan> {
    let mut file = syn::parse_file(source)?;
    StripNonShipping.visit_file_mut(&mut file);
    let mut scan = Scan::default();
    scan_tokens(file.into_token_stream(), &mut scan);
    Ok(scan)
}

/// Removes doc text and test-only code at every nesting depth.
struct StripNonShipping;

impl VisitMut for StripNonShipping {
    fn visit_attribute_mut(&mut self, attr: &mut Attribute) {
        // `///` and `//!` parse to `#[doc = "…"]`; reducing them to a bare `#[doc]` drops the text
        // while leaving every attribute list the same length.
        if attr.path().is_ident("doc") {
            attr.meta = Meta::Path(attr.path().clone());
        }
    }

    fn visit_file_mut(&mut self, file: &mut syn::File) {
        file.items.retain(|item| !is_test_only(item_attrs(item)));
        visit_mut::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, module: &mut syn::ItemMod) {
        if let Some((_, items)) = &mut module.content {
            items.retain(|item| !is_test_only(item_attrs(item)));
        }
        visit_mut::visit_item_mod_mut(self, module);
    }

    fn visit_item_impl_mut(&mut self, block: &mut syn::ItemImpl) {
        block
            .items
            .retain(|item| !is_test_only(impl_item_attrs(item)));
        visit_mut::visit_item_impl_mut(self, block);
    }

    fn visit_item_trait_mut(&mut self, block: &mut syn::ItemTrait) {
        block
            .items
            .retain(|item| !is_test_only(trait_item_attrs(item)));
        visit_mut::visit_item_trait_mut(self, block);
    }

    fn visit_block_mut(&mut self, block: &mut syn::Block) {
        block.stmts.retain(|stmt| match stmt {
            Stmt::Item(item) => !is_test_only(item_attrs(item)),
            _ => true,
        });
        visit_mut::visit_block_mut(self, block);
    }
}

/// An item compiled only into test builds: a `cfg` whose predicate requires `test`, or a test
/// function (`#[test]`, `#[tokio::test]`).
fn is_test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("cfg") {
            return attr
                .parse_args::<Meta>()
                .is_ok_and(|predicate| requires_test(&predicate));
        }
        attr.path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "test")
    })
}

/// Whether a `cfg` predicate can hold only when `test` is set. `any(test, …)` can hold without
/// it, and `not(test)` holds only without it.
fn requires_test(predicate: &Meta) -> bool {
    match predicate {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|all| all.iter().any(requires_test)),
        _ => false,
    }
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(i) => &i.attrs,
        ImplItem::Fn(i) => &i.attrs,
        ImplItem::Type(i) => &i.attrs,
        ImplItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(i) => &i.attrs,
        TraitItem::Fn(i) => &i.attrs,
        TraitItem::Type(i) => &i.attrs,
        TraitItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

/// Walks `tokens` and every group inside them, recording docs-relevant literals into `scan`.
fn scan_tokens(tokens: TokenStream, scan: &mut Scan) {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    for (i, tree) in trees.iter().enumerate() {
        match tree {
            TokenTree::Literal(literal) => {
                let text = literal.to_string();
                let value = string_value(tree).unwrap_or_else(|| text.clone());
                if value.contains("docs/content") {
                    scan.source_paths.push(text);
                }
                if is_warning_code(&value) {
                    scan.warning_codes.push(value);
                }
            }
            TokenTree::Ident(ident) if ident == "docs_reference_url" => {
                if let (Some(TokenTree::Punct(bang)), Some(TokenTree::Group(args))) =
                    (trees.get(i + 1), trees.get(i + 2))
                {
                    if bang.as_char() == '!' {
                        if let Some(path) = sole_string_argument(args) {
                            scan.reference_paths.push(path);
                        }
                    }
                }
            }
            TokenTree::Ident(ident) if ident == "diagnostic_link" => {
                if let Some(TokenTree::Group(args)) = trees.get(i + 1) {
                    if let Some(code) = sole_string_argument(args) {
                        scan.diagnostic_codes.push(code);
                    }
                }
            }
            TokenTree::Group(group) => scan_tokens(group.stream(), scan),
            _ => {}
        }
    }
}

/// The value of a parenthesised group holding exactly one string literal.
fn sole_string_argument(group: &proc_macro2::Group) -> Option<String> {
    if group.delimiter() != Delimiter::Parenthesis {
        return None;
    }
    let mut inner = group.stream().into_iter();
    match (inner.next(), inner.next()) {
        (Some(only), None) => string_value(&only),
        _ => None,
    }
}

/// `W-<LETTERS>-<DIGITS>`, the shape of every warning code.
fn is_warning_code(value: &str) -> bool {
    let mut parts = value.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("W"), Some(family), Some(number), None)
            if !family.is_empty()
                && family.bytes().all(|b| b.is_ascii_uppercase())
                && !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit())
    )
}

fn string_value(tree: &TokenTree) -> Option<String> {
    let TokenTree::Literal(literal) = tree else {
        return None;
    };
    syn::parse_str::<syn::LitStr>(&literal.to_string())
        .ok()
        .map(|lit| lit.value())
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "target" && name != "tests" {
                collect_sources(&path, out);
            }
        } else if name.ends_with(".rs") && !name.ends_with("_tests.rs") {
            out.push(path);
        }
    }
}

fn owning_crate(crates: &Path, file: &Path) -> String {
    let mut dir = file.parent();
    while let Some(candidate) = dir {
        if candidate.join("Cargo.toml").is_file() {
            return candidate
                .strip_prefix(crates)
                .unwrap()
                .to_string_lossy()
                .into_owned();
        }
        dir = candidate.parent();
    }
    panic!("{} has no Cargo.toml above it", file.display());
}

/// Every shipping source file under `crates/`, scanned once per test binary. A file `syn` cannot
/// parse fails the test rather than dropping out of the scan.
fn scanned_crates() -> &'static [ScannedFile] {
    static SCANNED: OnceLock<Vec<ScannedFile>> = OnceLock::new();
    SCANNED.get_or_init(|| {
        let crates = crates_dir().canonicalize().expect("crates/ directory");
        let mut files = Vec::new();
        collect_sources(&crates, &mut files);
        files
            .into_iter()
            .map(|file| {
                let source = fs::read_to_string(&file)
                    .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
                let scan = scan_source(&source)
                    .unwrap_or_else(|e| panic!("{} does not parse: {e}", file.display()));
                ScannedFile {
                    path: file.strip_prefix(&crates).unwrap().to_path_buf(),
                    krate: owning_crate(&crates, &file),
                    scan,
                }
            })
            .collect()
    })
}

/// The entries of the `exclude_docs: |` block in `docs/mkdocs.yml`: its indented lines, trimmed,
/// without blank lines and `#` comments.
fn excluded_docs() -> Vec<String> {
    let mkdocs = crates_dir().join("../docs/mkdocs.yml");
    let text = fs::read_to_string(&mkdocs).expect("docs/mkdocs.yml");
    let mut lines = text.lines();
    lines
        .by_ref()
        .find(|line| line.trim_end() == "exclude_docs: |")
        .expect("docs/mkdocs.yml has an `exclude_docs: |` block");
    lines
        .take_while(|line| line.is_empty() || line.starts_with(char::is_whitespace))
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && !entry.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Why the reference path in `docs_reference_url!("<path>")` does not open a published section,
/// if it does not.
fn reference_path_problem(path: &str, excluded: &[String]) -> Option<String> {
    let (page, anchor) = match path.split_once("/#") {
        Some((page, anchor)) => (page, Some(anchor)),
        None => match path.strip_suffix('/') {
            Some(page) => (page, None),
            None => {
                return Some("the page path does not end in `/` before any fragment".into());
            }
        },
    };
    let relative = format!("reference/{page}.md");
    let source = reference_dir().join(format!("{page}.md"));
    let Ok(text) = fs::read_to_string(&source) else {
        return Some(format!("docs/content/{relative} does not exist"));
    };
    if excluded.iter().any(|entry| {
        entry == &relative || (entry.ends_with('/') && relative.starts_with(entry.as_str()))
    }) {
        return Some(format!(
            "docs/content/{relative} is listed under exclude_docs in docs/mkdocs.yml, so the site \
             does not publish it"
        ));
    }
    match anchor {
        Some(anchor) if !text.contains(&format!("{{ #{anchor} }}")) => Some(format!(
            "docs/content/{relative} has no `{{ #{anchor} }}` heading"
        )),
        _ => None,
    }
}

/// Why `diagnostic_link(code)` does not open an entry in `diagnostics.md`, if it does not.
fn diagnostic_entry_problem(code: &str, diagnostics: &str) -> Option<String> {
    let anchor = code.to_lowercase();
    (!diagnostics.contains(&format!("{{ #{anchor} }}"))).then(|| {
        format!(
            "{}/diagnostics/#{anchor} — docs/content/reference/diagnostics.md has no \
             `{{ #{anchor} }}` entry for {code}",
            murmur_artifact::DOCS_REFERENCE_URL
        )
    })
}

#[test]
fn no_shipping_code_names_a_docs_source_path() {
    let offenders: Vec<String> = scanned_crates()
        .iter()
        .flat_map(|file| {
            file.scan
                .source_paths
                .iter()
                .map(move |literal| format!("crates/{}: {literal}", file.path.display()))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "shipping code names a docs source path a reader cannot open; build the link from \
         murmur_artifact::docs_reference_url! or diagnostic_link instead:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn every_literal_docs_link_opens_a_published_section() {
    let excluded = excluded_docs();
    let diagnostics =
        fs::read_to_string(reference_dir().join("diagnostics.md")).expect("diagnostics.md");
    let base = murmur_artifact::DOCS_REFERENCE_URL;

    let mut problems = Vec::new();
    let mut checked = 0;
    let mut crates = BTreeSet::new();
    for file in scanned_crates() {
        let location = format!("crates/{}", file.path.display());
        for path in &file.scan.reference_paths {
            if let Some(problem) = reference_path_problem(path, &excluded) {
                problems.push(format!("{location}: {base}/{path} — {problem}"));
            }
            checked += 1;
            crates.insert(file.krate.as_str());
        }
        for code in &file.scan.diagnostic_codes {
            if let Some(problem) = diagnostic_entry_problem(code, &diagnostics) {
                problems.push(format!("{location}: {problem}"));
            }
            checked += 1;
            crates.insert(file.krate.as_str());
        }
        for code in &file.scan.warning_codes {
            if let Some(problem) = diagnostic_entry_problem(code, &diagnostics) {
                problems.push(format!("{location}: {problem}"));
            }
        }
    }

    eprintln!("checked {checked} literal docs links across {crates:?}");
    assert!(
        problems.is_empty(),
        "docs links that do not open a published section:\n{}",
        problems.join("\n")
    );
    assert!(
        checked >= MIN_CHECKED_INVOCATIONS && crates.len() >= MIN_CHECKED_CRATES,
        "the scan checked {checked} literal docs links across {crates:?}; expected at least \
         {MIN_CHECKED_INVOCATIONS} across {MIN_CHECKED_CRATES} crates, so the walk is missing \
         sources"
    );
}

#[test]
fn reference_paths_outside_the_published_site_are_refused() {
    let excluded = excluded_docs();
    assert_eq!(reference_path_problem("cli/", &excluded), None);
    for path in [
        "resource-limits-manual-verification/",
        "no-such-page/",
        "resource-limits/#no-such-anchor",
        "cli",
    ] {
        assert!(
            reference_path_problem(path, &excluded).is_some(),
            "{path} passed"
        );
    }
}

#[test]
fn scanner_strips_comments_docs_and_test_only_code() {
    let reported = |source: &str| {
        scan_source(source)
            .unwrap_or_else(|e| panic!("{source} does not parse: {e}"))
            .source_paths
    };

    assert_eq!(
        reported(r#"fn f() { eprintln!("see docs/content/x.md"); }"#),
        [r#""see docs/content/x.md""#]
    );
    assert_eq!(
        reported(r#"#[cfg(not(test))] fn g() { eprintln!("see docs/content/x.md"); }"#).len(),
        1
    );
    assert_eq!(
        reported(
            r#"#[cfg(any(test, feature = "x"))] fn h() { eprintln!("see docs/content/x.md"); }"#
        )
        .len(),
        1
    );
    for hidden in [
        "fn f() {\n    // see docs/content/x.md\n}",
        "/// see docs/content/x.md\nfn f() {}",
        "//! see docs/content/x.md\nfn f() {}",
        r#"#[cfg(test)] mod t { fn f() { eprintln!("see docs/content/x.md"); } }"#,
        r#"#[test] fn t() { eprintln!("see docs/content/x.md"); }"#,
        r#"#[tokio::test] async fn t() { eprintln!("see docs/content/x.md"); }"#,
        r#"#[cfg(all(test, target_os = "linux"))] mod u { fn f() { eprintln!("see docs/content/x.md"); } }"#,
        r#"impl S { #[cfg(test)] fn f() { eprintln!("see docs/content/x.md"); } }"#,
        r#"fn f() { #[cfg(test)] fn g() { eprintln!("see docs/content/x.md"); } }"#,
    ] {
        assert!(reported(hidden).is_empty(), "reported from:\n{hidden}");
    }

    let links = scan_source(
        r#"
        const A: &str = concat!("see ", murmur_artifact::docs_reference_url!("cli/"));
        #[error("at {}", murmur_artifact::docs_reference_url!("containment/#verification"))]
        struct E;
        fn f(code: &str) -> String { diagnostic_link("E-CAP-016") + &diagnostic_link(code) }
        macro_rules! m { ($path:literal) => { docs_reference_url!($path) }; }
        "#,
    )
    .unwrap();
    assert_eq!(links.reference_paths, ["cli/", "containment/#verification"]);
    assert_eq!(links.diagnostic_codes, ["E-CAP-016"]);

    let codes = scan_source(
        r#"
        pub const W_SEC_024: &str = "W-SEC-024";
        const NOT_A_CODE: [&str; 4] = ["W-SEC-", "W-sec-024", "E-CAP-016", "see W-SEC-024"];
        "#,
    )
    .unwrap();
    assert_eq!(codes.warning_codes, ["W-SEC-024"]);
}
