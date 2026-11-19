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

use std::cmp::Ordering;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use memchr::memmem::Finder;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};

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
        let (original, lowered) = index.target_text(entry_index, self.options.match_path);
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

/// Builds an index over entries for repeated searches.
pub struct SearchIndex {
    entries: Vec<Entry>,
    names: Vec<String>,
    names_lower: Vec<String>,
    paths: Option<OnceLock<(Vec<String>, Vec<String>)>>,
}

impl SearchIndex {
    /// Builds the index. When `need_paths` is set, the first path query
    /// caches the full-path strings for subsequent searches.
    pub fn new(entries: Vec<Entry>, need_paths: bool) -> Self {
        let (names, names_lower) = entries
            .par_iter()
            .map(|entry| {
                let name = entry.name_string();
                let lower = name.to_lowercase();
                (name, lower)
            })
            .unzip();
        Self {
            entries,
            names,
            names_lower,
            paths: need_paths.then(OnceLock::new),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entry(&self, index: usize) -> &Entry {
        &self.entries[index]
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn name_lower(&self, index: usize) -> &str {
        &self.names_lower[index]
    }

    pub fn replace_entry(&mut self, index: usize, entry: Entry) {
        if entry.path.as_os_str() != self.entries[index].path.as_os_str() {
            let name = entry.name_string();
            self.names_lower[index] = name.to_lowercase();
            self.names[index] = name;
            if let Some((paths, paths_lower)) = self.paths.as_mut().and_then(OnceLock::get_mut) {
                let path = entry.path.to_string_lossy();
                paths_lower[index] = path.to_lowercase();
                paths[index] = path.into_owned();
            }
        }
        self.entries[index] = entry;
    }

    pub fn push_entry(&mut self, entry: Entry) {
        let name = entry.name_string();
        self.names_lower.push(name.to_lowercase());
        self.names.push(name);
        if let Some((paths, paths_lower)) = self.paths.as_mut().and_then(OnceLock::get_mut) {
            let path = entry.path.to_string_lossy();
            paths_lower.push(path.to_lowercase());
            paths.push(path.into_owned());
        }
        self.entries.push(entry);
    }

    pub fn swap_remove_entry(&mut self, index: usize) {
        self.entries.swap_remove(index);
        self.names.swap_remove(index);
        self.names_lower.swap_remove(index);
        if let Some((paths, paths_lower)) = self.paths.as_mut().and_then(OnceLock::get_mut) {
            paths.swap_remove(index);
            paths_lower.swap_remove(index);
        }
    }

    fn target_text(&self, index: usize, match_path: bool) -> (&str, &str) {
        if match_path {
            if let Some((paths, paths_lower)) = self.search_paths() {
                return (&paths[index], &paths_lower[index]);
            }
        }
        (&self.names[index], &self.names_lower[index])
    }

    fn search_paths(&self) -> Option<&(Vec<String>, Vec<String>)> {
        self.paths.as_ref().map(|paths| {
            paths.get_or_init(|| {
                self.entries
                    .iter()
                    .map(|entry| {
                        let path = entry.path.to_string_lossy();
                        let lowered = path.to_lowercase();
                        (path.into_owned(), lowered)
                    })
                    .unzip()
            })
        })
    }
}

/// Runs a query over the index and returns matching entries.
///
/// Results are sorted by `sort` in the requested direction and truncated to
/// `limit`. Size sorting keeps directories and unknown sizes at the end.
pub fn search<'a>(
    index: &'a SearchIndex,
    query: &Query,
    sort: SortKey,
    descending: bool,
    limit: Option<usize>,
) -> Vec<&'a Entry> {
    search_indices(index, query, sort, descending, limit)
        .into_iter()
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
    if query.options.match_path {
        // Initialize the shared cache before workers start reading it.
        index.search_paths();
    }
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
pub fn sort_hits(index: &SearchIndex, hits: &mut [usize], sort: SortKey, descending: bool) {
    hits.par_sort_by(|&a, &b| compare_entries(index, a, b, sort, descending));
}

/// Inserts new or changed entries into an existing sorted order.
pub fn insert_sorted_indices(
    index: &SearchIndex,
    order: &mut Vec<usize>,
    updated: &[usize],
    sort: SortKey,
    descending: bool,
) {
    if updated.len() > 32 {
        order.extend_from_slice(updated);
        sort_hits(index, order, sort, descending);
        return;
    }
    for &new_index in updated {
        let position = order.partition_point(|&existing| {
            compare_entries(index, existing, new_index, sort, descending) != Ordering::Greater
        });
        order.insert(position, new_index);
    }
}

fn compare_entries(
    index: &SearchIndex,
    a: usize,
    b: usize,
    sort: SortKey,
    descending: bool,
) -> Ordering {
    let left = index.entry(a);
    let right = index.entry(b);
    let ordering = match sort {
        SortKey::Name => index.name_lower(a).cmp(index.name_lower(b)),
        SortKey::Path => left.path.cmp(&right.path),
        SortKey::Size => {
            let left_size = left.size.filter(|_| !left.is_dir);
            let right_size = right.size.filter(|_| !right.is_dir);
            match (left_size, right_size) {
                (Some(left), Some(right)) => left.cmp(&right),
                (None, None) => Ordering::Equal,
                _ => return left_size.is_none().cmp(&right_size.is_none()),
            }
        }
        SortKey::Modified => left.modified.unwrap_or(0).cmp(&right.modified.unwrap_or(0)),
    };
    let ordering = ordering.then_with(|| left.path.cmp(&right.path));
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

        sort_hits(&index, &mut hits, SortKey::Size, true);
        assert_eq!(hits, [1, 0, 4, 2, 3]);

        sort_hits(&index, &mut hits, SortKey::Size, false);
        assert_eq!(hits, [4, 0, 1, 3, 2]);
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
    fn star_glob_keeps_single_line_regex_semantics() {
        assert_eq!(run(&["one.txt", "two\nlines.txt"], "*", options()), ["one.txt"]);
    }
}
