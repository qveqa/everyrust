//! Query parsing and matching for local file search.
//!
//! Supported syntax:
//!
//! - space-separated terms are AND-ed: `foo bar`;
//! - `|` separates alternatives (OR): `foo|bar`;
//! - double quotes group a phrase: `"report 2026"`;
//! - `*` and `?` wildcards match the whole name: `*.txt`, `?oo*`;
//! - `/pattern/` is a regular expression: `/\d{4}/`;
//! - by default terms match the file name; `--match-path` matches the full path;
//! - matching is case-insensitive unless `--case` is given;
//! - `--whole-word` requires word boundaries around literal terms.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::path::Path;

use anyhow::{bail, Context, Result};
use memchr::memmem::Finder;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};

pub use crate::index::SearchIndex;
use crate::model::Entry;

/// Result ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
}

/// Matching behaviour for a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub regex: bool,
}

/// A parsed query: a disjunction of conjunctions of terms.
#[derive(Clone)]
pub struct Query {
    options: SearchOptions,
    alternatives: Vec<Vec<Term>>,
}

#[derive(Clone)]
enum Term {
    Literal(Finder<'static>),
    AnyLine,
    /// Wildcard, regular expression, or whole-word pattern.
    Regex(Regex),
}

impl Query {
    /// Parses `text` into a query. Errors on empty input or invalid regexes.
    pub fn parse(text: &str, options: SearchOptions) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            bail!("empty search query");
        }
        if options.regex {
            return Ok(Self {
                options,
                alternatives: vec![vec![Term::Regex(build_regex(text, options.case_sensitive)?)]],
            });
        }

        let mut alternatives = Vec::new();
        for part in split_alternatives(text) {
            let mut terms = Vec::new();
            for raw in tokenize(&part) {
                terms.push(compile_term(&raw, options)?);
            }
            if !terms.is_empty() {
                alternatives.push(terms);
            }
        }
        if alternatives.is_empty() {
            bail!("empty search query");
        }
        Ok(Self {
            options,
            alternatives,
        })
    }

    /// Whether entry `index` matches one of the alternatives.
    pub fn matches(&self, index: &SearchIndex, entry_index: usize) -> bool {
        if !index.is_live(entry_index) {
            return false;
        }
        if !self.options.match_path {
            return self.matches_text(index.name(entry_index), index.name_lower(entry_index));
        }
        thread_local! {
            static BUFFERS: RefCell<(Vec<u32>, String, String)> = RefCell::default();
        }
        BUFFERS.with(|buffers| {
            let (chain, original, lowered) = &mut *buffers.borrow_mut();
            index.write_path(entry_index, false, chain, original);
            index.write_path(entry_index, true, chain, lowered);
            self.matches_text(original, lowered)
        })
    }

    fn matches_text(&self, original: &str, lowered: &str) -> bool {
        self.alternatives.iter().any(|terms| {
            terms
                .iter()
                .all(|term| term.matches(original, lowered, self.options.case_sensitive))
        })
    }
}

impl Term {
    fn matches(&self, original: &str, lowered: &str, case_sensitive: bool) -> bool {
        match self {
            Term::Literal(finder) => {
                let text = if case_sensitive { original } else { lowered };
                finder.find(text.as_bytes()).is_some()
            }
            Term::AnyLine => memchr::memchr(b'\n', original.as_bytes()).is_none(),
            Term::Regex(regex) => regex.is_match(original),
        }
    }
}

/// Runs a query over the index and returns matching entries.
///
/// Results are sorted by `sort` in the requested direction and truncated to
/// `limit`. Size sorting keeps directories and unknown sizes at the end.
pub fn search(
    index: &SearchIndex,
    query: &Query,
    sort: SortKey,
    descending: bool,
    limit: Option<usize>,
) -> Vec<Entry> {
    search_indices(index, query, sort, descending, limit)
        .into_par_iter()
        .map(|i| index.entry(i))
        .collect()
}

/// Like [`search`], but returns indexes into [`SearchIndex::entries`] instead
/// of borrows, which is what a UI needs for virtualized result lists.
pub fn search_indices(
    index: &SearchIndex,
    query: &Query,
    sort: SortKey,
    descending: bool,
    limit: Option<usize>,
) -> Vec<usize> {
    if limit == Some(0) {
        return Vec::new();
    }
    let mut hits = filter_indices(index, query, (0..index.len()).into_par_iter());
    if let Some(limit) = limit.filter(|&limit| limit < hits.len()) {
        let compare = |a: &usize, b: &usize| {
            compare_entries(index, *a, *b, sort, descending).then_with(|| a.cmp(b))
        };
        hits.select_nth_unstable_by(limit, compare);
        hits.truncate(limit);
        hits.par_sort_unstable_by(compare);
    } else {
        sort_hits(index, &mut hits, sort, descending);
    }
    hits
}

/// Filters a list of entry indexes while preserving its existing order.
pub fn filter_ordered_indices(index: &SearchIndex, query: &Query, order: &[usize]) -> Vec<usize> {
    filter_indices(index, query, order.par_iter().copied())
}

fn filter_indices(
    index: &SearchIndex,
    query: &Query,
    indices: impl IndexedParallelIterator<Item = usize>,
) -> Vec<usize> {
    let uses_regex = query
        .alternatives
        .iter()
        .flatten()
        .any(|term| matches!(term, Term::Regex(_)));
    let indices = indices.with_min_len(4096);
    if uses_regex {
        // Each worker gets its own regex scratch space.
        indices
            .map_init(|| query.clone(), |query, i| query.matches(index, i).then_some(i))
            .filter_map(|hit| hit)
            .collect()
    } else {
        indices.filter(|&i| query.matches(index, i)).collect()
    }
}

/// Sorts hit indexes according to `sort`; ties break by path so the order is
/// deterministic across runs.
///
/// Each hit gets a numeric sort key first. Most comparisons then use only that
/// key, which avoids reading the entries and keeps the sort cache-friendly.
pub fn sort_hits(index: &SearchIndex, hits: &mut [usize], sort: SortKey, descending: bool) {
    let direct = |ordering: Ordering| if descending { ordering.reverse() } else { ordering };
    let ranks = index.path_ranks();
    let path_order = |a: usize, b: usize| match ranks {
        Some(ranks) => ranks[a].cmp(&ranks[b]),
        None => a.cmp(&b),
    };
    if sort == SortKey::Path {
        hits.par_sort_unstable_by(|&a, &b| direct(path_order(a, b)));
        return;
    }
    let mut keyed: Vec<(u128, usize)> = hits
        .par_iter()
        .map(|&hit| (sort_key(index, hit, sort, descending), hit))
        .collect();
    keyed.par_sort_unstable_by(|&(left_key, a), &(right_key, b)| {
        left_key.cmp(&right_key).then_with(|| {
            let tie = match sort {
                SortKey::Name => index.name_lower(a).cmp(index.name_lower(b)),
                _ => Ordering::Equal,
            };
            direct(tie.then_with(|| path_order(a, b)))
        })
    });
    hits.par_iter_mut()
        .zip(keyed.par_iter())
        .for_each(|(hit, &(_, sorted))| *hit = sorted);
}

/// A key whose ascending order matches [`compare_entries`] up to its tie-break.
fn sort_key(index: &SearchIndex, id: usize, sort: SortKey, descending: bool) -> u128 {
    let direct = |value: u64| u128::from(if descending { !value } else { value });
    match sort {
        // Directories and unknown sizes come last in both directions.
        SortKey::Size => match index.size(id).filter(|_| !index.is_dir(id)) {
            Some(size) => direct(size),
            None => 1 << 64,
        },
        SortKey::Modified => direct(index.modified(id).unwrap_or(0)),
        SortKey::Name => {
            // The first eight bytes, zero-padded, order like the whole name.
            let mut prefix = [0; 8];
            let name = index.name_lower(id).as_bytes();
            let length = name.len().min(8);
            prefix[..length].copy_from_slice(&name[..length]);
            direct(u64::from_be_bytes(prefix))
        }
        SortKey::Path => 0,
    }
}

/// Orders paths component by component, like `Path::cmp` does for normalized
/// paths, but works on the raw bytes. A separator sorts before every other
/// byte, so a folder is followed directly by everything inside it. Unlike
/// `Path::cmp`, drive letters compare by case.
pub fn compare_paths(left: &Path, right: &Path) -> Ordering {
    let left = left.as_os_str().as_encoded_bytes();
    let right = right.as_os_str().as_encoded_bytes();
    let mut offset = 0;
    while let Some(position) = first_difference(&left[offset..], &right[offset..]) {
        let position = offset + position;
        let ordering = separator_rank(left[position]).cmp(&separator_rank(right[position]));
        if ordering != Ordering::Equal {
            return ordering;
        }
        // `/` and `\` are the same separator; keep comparing after it.
        offset = position + 1;
    }
    left.len().cmp(&right.len())
}

fn separator_rank(byte: u8) -> u16 {
    if std::path::is_separator(char::from(byte)) {
        0
    } else {
        u16::from(byte) + 1
    }
}

/// Index of the first differing byte within the shorter length.
fn first_difference(left: &[u8], right: &[u8]) -> Option<usize> {
    let length = left.len().min(right.len());
    let mut offset = 0;
    while offset + 8 <= length {
        let a = u64::from_le_bytes(left[offset..offset + 8].try_into().unwrap());
        let b = u64::from_le_bytes(right[offset..offset + 8].try_into().unwrap());
        if a != b {
            return Some(offset + ((a ^ b).trailing_zeros() / 8) as usize);
        }
        offset += 8;
    }
    (offset..length).find(|&position| left[position] != right[position])
}

/// Inserts new or changed entries into an existing sorted order.
pub fn insert_sorted_indices(
    index: &SearchIndex,
    order: &mut Vec<usize>,
    updated: &[usize],
    sort: SortKey,
    descending: bool,
) {
    if updated.is_empty() {
        return;
    }
    let compare = |a: usize, b: usize| compare_entries(index, a, b, sort, descending).then_with(|| a.cmp(&b));
    let mut updated = updated.to_vec();
    updated.sort_by(|&a, &b| compare(a, b));
    // Find each position by binary search, then merge in one pass.
    let positions: Vec<usize> = updated
        .iter()
        .map(|&new| order.partition_point(|&existing| compare(existing, new) != Ordering::Greater))
        .collect();
    let mut merged = Vec::with_capacity(order.len() + updated.len());
    let mut last = 0;
    for (&new, &position) in updated.iter().zip(&positions) {
        merged.extend_from_slice(&order[last..position]);
        merged.push(new);
        last = position;
    }
    merged.extend_from_slice(&order[last..]);
    *order = merged;
}

fn compare_entries(
    index: &SearchIndex,
    a: usize,
    b: usize,
    sort: SortKey,
    descending: bool,
) -> Ordering {
    let ordering = match sort {
        SortKey::Name => index.name_lower(a).cmp(index.name_lower(b)),
        SortKey::Path => Ordering::Equal,
        SortKey::Size => {
            let left_size = index.size(a).filter(|_| !index.is_dir(a));
            let right_size = index.size(b).filter(|_| !index.is_dir(b));
            match (left_size, right_size) {
                (Some(left), Some(right)) => left.cmp(&right),
                (None, None) => Ordering::Equal,
                _ => return left_size.is_none().cmp(&right_size.is_none()),
            }
        }
        SortKey::Modified => index.modified(a).unwrap_or(0).cmp(&index.modified(b).unwrap_or(0)),
    };
    let ordering = ordering.then_with(|| index.compare_by_path(a, b));
    if descending {
        ordering.reverse()
    } else {
        ordering
    }
}

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .with_context(|| format!("invalid regular expression: {pattern}"))
}

fn split_alternatives(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '|' if !in_quotes => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    parts.push(current);
    parts
}

fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in text.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ch if ch.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    terms.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
}

fn compile_term(raw: &str, options: SearchOptions) -> Result<Term> {
    if raw.len() >= 2 && raw.starts_with('/') && raw.ends_with('/') {
        return Ok(Term::Regex(build_regex(
            &raw[1..raw.len() - 1],
            options.case_sensitive,
        )?));
    }
    if !raw.is_empty() && raw.bytes().all(|byte| byte == b'*') {
        // This is the same single-line match as the corresponding anchored glob.
        return Ok(Term::AnyLine);
    }
    if raw.contains('*') || raw.contains('?') {
        return Ok(Term::Regex(build_regex(
            &glob_to_regex(raw),
            options.case_sensitive,
        )?));
    }
    if options.whole_word {
        let escaped = regex::escape(raw);
        let pattern = format!(r"(?:^|[^\p{{L}}\p{{N}}_]){escaped}(?:[^\p{{L}}\p{{N}}_]|$)");
        return Ok(Term::Regex(build_regex(&pattern, options.case_sensitive)?));
    }
    let literal = if options.case_sensitive {
        std::borrow::Cow::Borrowed(raw)
    } else {
        std::borrow::Cow::Owned(raw.to_lowercase())
    };
    Ok(Term::Literal(Finder::new(literal.as_bytes()).into_owned()))
}

fn glob_to_regex(glob: &str) -> String {
    let mut pattern = String::from("^");
    for ch in glob.chars() {
        match ch {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            ch => pattern.push_str(&regex::escape(&ch.to_string())),
        }
    }
    pattern.push('$');
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[&str]) -> Vec<Entry> {
        paths
            .iter()
            .map(|p| Entry {
                path: std::path::PathBuf::from(p),
                is_dir: false,
                size: Some(1),
                modified: None,
                created: None,
                attributes: None,
            })
            .collect()
    }

    fn options() -> SearchOptions {
        SearchOptions::default()
    }

    fn run(paths: &[&str], query: &str, opts: SearchOptions) -> Vec<String> {
        let index = SearchIndex::new(entries(paths), opts.match_path);
        let query = Query::parse(query, opts).unwrap();
        search(&index, &query, SortKey::Name, false, None)
            .into_iter()
            .map(|e| e.name_string())
            .collect()
    }

    #[test]
    fn tokenize_handles_quotes_and_whitespace() {
        assert_eq!(tokenize(r#"foo "bar baz" qux"#), vec!["foo", "bar baz", "qux"]);
    }

    #[test]
    fn literal_substring_and_and_logic() {
        let paths = ["report.txt", "report_final.txt", "notes.txt"];
        assert_eq!(run(&paths, "report", options()), ["report.txt", "report_final.txt"]);
        assert_eq!(run(&paths, "report final", options()), ["report_final.txt"]);
        assert!(run(&paths, "report missing", options()).is_empty());
    }

    #[test]
    fn quotes_match_phrases() {
        let paths = ["my report.txt", "myreport.txt"];
        assert_eq!(run(&paths, r#""my report""#, options()), ["my report.txt"]);
    }

    #[test]
    fn or_alternatives() {
        let paths = ["a.txt", "b.md", "c.rs"];
        let mut found = run(&paths, "*.txt|*.rs", options());
        found.sort();
        assert_eq!(found, ["a.txt", "c.rs"]);
    }

    #[test]
    fn wildcards_match_whole_name() {
        let paths = ["a.txt", "b.txt", "c.md", "atxt"];
        assert_eq!(run(&paths, "*.txt", options()), ["a.txt", "b.txt"]);
        assert_eq!(run(&paths, "?.txt", options()), ["a.txt", "b.txt"]);
        assert_eq!(run(&paths, "a*", options()), ["a.txt", "atxt"]);
        assert!(run(&paths, "x*", options()).is_empty());
    }

    #[test]
    fn case_sensitivity_is_off_by_default() {
        let paths = ["README.md", "readme.txt"];
        assert_eq!(run(&paths, "readme", options()).len(), 2);
        let mut case_sensitive = options();
        case_sensitive.case_sensitive = true;
        assert_eq!(
            run(&paths, "readme", case_sensitive),
            ["readme.txt"]
        );
    }

    #[test]
    fn whole_word_needs_boundaries() {
        let paths = ["report.txt", "reporter.txt", "my-report.md"];
        let mut opts = options();
        opts.whole_word = true;
        assert_eq!(
            run(&paths, "report", opts),
            ["my-report.md", "report.txt"]
        );
        assert!(run(&["reporter.txt"], "report", opts).is_empty());
    }

    #[test]
    fn match_path_uses_full_path() {
        let paths = [r"C:\docs\a.txt", r"C:\music\a.txt", r"C:\docs\b.md"];
        let mut opts = options();
        opts.match_path = true;
        assert_eq!(run(&paths, r"docs\a.txt", opts), ["a.txt"]);
        assert_eq!(run(&paths, r"docs", opts).len(), 2);
        assert!(run(&paths, r"music\a.txt", options()).is_empty());
    }

    #[test]
    fn regex_terms() {
        let paths = ["report-2024.txt", "report-2026.txt", "notes.txt"];
        let found = run(&paths, r"/\d{4}/", options());
        assert_eq!(found, ["report-2024.txt", "report-2026.txt"]);
    }

    #[test]
    fn regex_mode_uses_whole_query() {
        let paths = ["a1.txt", "a.txt", "b2.txt"];
        let mut opts = options();
        opts.regex = true;
        assert_eq!(
            run(&paths, r"^[ab]\d\.txt$", opts),
            ["a1.txt", "b2.txt"]
        );
    }

    #[test]
    fn results_are_sorted_and_limited() {
        let paths = ["b.txt", "a.txt", "c.txt"];
        let index = SearchIndex::new(entries(&paths), false);
        let query = Query::parse("*.txt", options()).unwrap();
        let hits = search(&index, &query, SortKey::Name, false, Some(2));
        let names: Vec<String> = hits.iter().map(|e| e.name_string()).collect();
        assert_eq!(names, ["a.txt", "b.txt"]);

        let hits = search(&index, &query, SortKey::Name, true, None);
        let names: Vec<String> = hits.iter().map(|e| e.name_string()).collect();
        assert_eq!(names, ["c.txt", "b.txt", "a.txt"]);
    }

    #[test]
    fn size_sort_keeps_directories_and_unknown_sizes_last() {
        let mut files = entries(&["small", "largest", "unknown", "folder", "empty"]);
        files[1].size = Some(u64::MAX);
        files[2].size = None;
        files[3].is_dir = true;
        files[4].size = Some(0);
        let index = SearchIndex::new(files, false);
        let mut hits = [0, 1, 2, 3, 4];
        let names = |hits: &[usize]| hits.iter().map(|&hit| index.name(hit).to_owned()).collect::<Vec<_>>();

        sort_hits(&index, &mut hits, SortKey::Size, true);
        assert_eq!(names(&hits), ["largest", "small", "empty", "unknown", "folder"]);

        sort_hits(&index, &mut hits, SortKey::Size, false);
        assert_eq!(names(&hits), ["empty", "small", "largest", "folder", "unknown"]);
    }

    #[test]
    fn cached_order_and_limited_search_match_full_sort() {
        let mut files = entries(&["report.txt", "report.txt", "notes.txt"]);
        files[1].modified = Some(13);
        for _ in 0..3000 {
            files.extend(entries(&["report2.txt", "draft.txt", "report1.txt"]));
        }
        let index = SearchIndex::new(files, false);
        let query = Query::parse("*.txt|missing", options()).unwrap();
        let sort = SortKey::Name;
        let full = search_indices(&index, &query, sort, true, None);
        let limited = search_indices(&index, &query, sort, true, Some(3010));
        assert_eq!(limited, full[..3010]);

        let query = Query::parse("report*.txt", options()).unwrap();
        let mut order: Vec<usize> = (0..index.len()).collect();
        sort_hits(&index, &mut order, sort, true);
        assert_eq!(
            filter_ordered_indices(&index, &query, &order),
            search_indices(&index, &query, sort, true, None),
        );
    }

    #[test]
    fn path_order_matches_components_and_sorted_indexes_agree() {
        let paths = ["/a b", "/a/x", "/a", "/ab", "/a/x/y", "/a-b/c", "/"];
        let mut sorted: Vec<&Path> = paths.iter().map(Path::new).collect();
        sorted.sort_by(|a, b| compare_paths(a, b));
        let mut expected: Vec<&Path> = paths.iter().map(Path::new).collect();
        expected.sort();
        assert_eq!(sorted, expected);

        let mut files = entries(&["/d/b", "/d/a", "/c", "/d", "/a"]);
        files[0].size = Some(5);
        files[3].is_dir = true;
        let shuffled = SearchIndex::new(files.clone(), false);
        files.sort_by(|a, b| compare_paths(&a.path, &b.path));
        let ordered = SearchIndex::new(files, false);
        assert!(ordered.is_path_sorted());
        for sort in [SortKey::Name, SortKey::Path, SortKey::Size, SortKey::Modified] {
            for descending in [false, true] {
                let names = |index: &SearchIndex| {
                    let mut hits: Vec<usize> = (0..index.len()).collect();
                    sort_hits(index, &mut hits, sort, descending);
                    hits.iter().map(|&hit| index.path(hit)).collect::<Vec<_>>()
                };
                assert_eq!(names(&shuffled), names(&ordered), "{sort:?} {descending}");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn mixed_separators_compare_like_path_cmp() {
        let pairs = [(r"C:\a\z", "C:/a/b"), (r"C:\a/b", r"C:/a\b"), (r"C:\a b", "C:/a/b")];
        for (left, right) in pairs {
            assert_eq!(
                compare_paths(Path::new(left), Path::new(right)),
                Path::new(left).cmp(Path::new(right)),
                "{left} vs {right}"
            );
        }
    }

    #[test]
    fn star_glob_keeps_single_line_regex_semantics() {
        assert_eq!(run(&["one.txt", "two\nlines.txt"], "*", options()), ["one.txt"]);
    }
}
