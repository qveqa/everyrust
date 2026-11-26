use std::ffi::OsString;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use everyrust::efu;
use everyrust::model::Entry;
use everyrust::scan;
use everyrust::search::{self, SearchIndex, SearchOptions, SortKey};

/// mimalloc is much faster than the system allocator for the millions of
/// small strings an index holds, especially when threads allocate at once.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const HELP: &str = r#"everyrust — fast local file search

USAGE
  everyrust -root <folder> -search <query> [options]
  everyrust -filelist <list.efu> -search <query> [options]
  everyrust -create-file-list <out.efu> <folder>
  everyrust <query> -root <folder>          # query as the first positional arg

OPTIONS
  -search <text>        Search text (also: --search, or the first positional arg)
  -filelist <path>      Search a .efu/.csv/.txt file list instead of scanning
  -root <path>          Scan this folder, then search (also: -path)
  -create-file-list <out.efu> <folder>
                        Scan <folder> and write an EFU file list
  -export-txt <path>    Write results to a file instead of stdout
  -case                 Case-sensitive matching (default: case-insensitive)
  -whole-word           Require word boundaries around literal terms
  -match-path           Match against the full path instead of the file name
  -regex                Treat the whole query as one regular expression
  -sort <name|path|size|modified>
                        Result order (default: name)
  -desc                 Reverse the result order
  -limit <n>            Keep at most <n> results
  -json                 Print results as JSON
  -verbose              Print progress to stderr
  -close, -no-ui, -instance <name>
                        Accepted for scripting compatibility (ignored)
  -h, -help, --help     Show this help
  -version, --version   Show the version

QUERY SYNTAX
  foo bar       AND: both terms must match
  foo|bar       OR: either alternative matches
  "exact text"  quoted phrase (the space is part of the term)
  *.txt         wildcard over the whole file name (* and ?)
  /regex/       regular expression term
  with -regex   the whole query is one regular expression

EXAMPLES
  everyrust -create-file-list C:\lists\media.efu C:\Media
  everyrust -filelist C:\lists\media.efu -search "*.mp4" -sort size -desc -limit 10
  everyrust -root C:\Users\me -search "report 2026" -export-txt results.txt
  everyrust -root C:\src -search "/\.rs$/"
"#;

#[derive(Debug, Default)]
struct Options {
    search: Option<String>,
    filelist: Option<PathBuf>,
    root: Option<PathBuf>,
    create_file_list: Option<(PathBuf, PathBuf)>,
    export_txt: Option<PathBuf>,
    case_sensitive: bool,
    whole_word: bool,
    match_path: bool,
    regex: bool,
    sort: SortKey,
    desc: bool,
    limit: Option<usize>,
    json: bool,
    verbose: bool,
}

enum ParseOutcome {
    Help,
    Version,
    Options(Options),
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse_args(&args) {
        Ok(ParseOutcome::Help) => {
            print!("{HELP}");
            Ok(0)
        }
        Ok(ParseOutcome::Version) => {
            println!("everyrust {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Ok(ParseOutcome::Options(options)) => {
            execute(options)?;
            Ok(0)
        }
        Err(message) => {
            eprintln!("error: {message}\n\n{HELP}");
            Ok(2)
        }
    }
}

fn parse_args(args: &[OsString]) -> std::result::Result<ParseOutcome, String> {
    if args.is_empty() {
        return Ok(ParseOutcome::Help);
    }

    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy().into_owned();
        if argument == "-h" || argument == "--help" || argument == "-help" || argument == "/?" {
            return Ok(ParseOutcome::Help);
        }
        if argument == "-version" || argument == "--version" || argument == "-V" {
            return Ok(ParseOutcome::Version);
        }

        if let Some(stripped) = argument.strip_prefix('-') {
            let name = stripped.trim_start_matches('-').to_ascii_lowercase();
            match name.as_str() {
                "search" => {
                    options.search = Some(take_value(args, &mut index, &argument)?);
                }
                "filelist" | "file-list" => {
                    options.filelist = Some(PathBuf::from(take_value(args, &mut index, &argument)?));
                }
                "root" | "path" => {
                    options.root = Some(PathBuf::from(take_value(args, &mut index, &argument)?));
                }
                "create-file-list" => {
                    let output = take_value(args, &mut index, &argument)?;
                    let root = take_value(args, &mut index, &argument)?;
                    options.create_file_list = Some((PathBuf::from(output), PathBuf::from(root)));
                }
                "export-txt" => {
                    options.export_txt =
                        Some(PathBuf::from(take_value(args, &mut index, &argument)?));
                }
                "case" => options.case_sensitive = true,
                "whole-word" | "wholeword" => options.whole_word = true,
                "match-path" | "matchpath" => options.match_path = true,
                "regex" | "regular-expression" => options.regex = true,
                "sort" => options.sort = parse_sort(&take_value(args, &mut index, &argument)?)?,
                "desc" | "descending" => options.desc = true,
                "limit" => {
                    let value = take_value(args, &mut index, &argument)?;
                    options.limit = Some(value.parse().map_err(|_| {
                        format!("{argument} expects a number, got {value}")
                    })?);
                }
                "json" => options.json = true,
                "verbose" | "v" => options.verbose = true,
                "close" | "no-ui" => {}
                "instance" => {
                    let _ = take_value(args, &mut index, &argument)?;
                }
                other => return Err(format!("unknown option: -{other}")),
            }
            index += 1;
        } else if options.search.is_none() && options.create_file_list.is_none() {
            options.search = Some(argument);
            index += 1;
        } else if options.root.is_none() && options.filelist.is_none() {
            options.root = Some(PathBuf::from(argument));
            index += 1;
        } else {
            return Err(format!("unexpected argument: {argument}"));
        }
    }

    Ok(ParseOutcome::Options(options))
}

fn take_value(
    args: &[OsString],
    index: &mut usize,
    flag: &str,
) -> std::result::Result<String, String> {
    *index += 1;
    args.get(*index)
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{flag} expects a value"))
}

fn parse_sort(value: &str) -> std::result::Result<SortKey, String> {
    let normalized = value.to_ascii_lowercase().replace(['-', ' '], "_");
    match normalized.as_str() {
        "name" | "filename" => Ok(SortKey::Name),
        "path" => Ok(SortKey::Path),
        "size" => Ok(SortKey::Size),
        "modified" | "date_modified" | "datemodified" | "date" => Ok(SortKey::Modified),
        other => Err(format!("unknown sort key: {other}")),
    }
}

fn execute(options: Options) -> Result<()> {
    if let Some((output, root)) = &options.create_file_list {
        if options.verbose {
            eprintln!("scanning {} ...", root.display());
        }
        let mut entries = scan::scan(root)?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        efu::write_file_list(output, &entries)?;
        eprintln!("{} entries written to {}", entries.len(), output.display());
        return Ok(());
    }

    let query_text = options
        .search
        .as_deref()
        .ok_or_else(|| anyhow!("missing -search <text>"))?;
    if options.filelist.is_some() && options.root.is_some() {
        bail!("use either -filelist or -root, not both");
    }

    let index = if let Some(filelist) = &options.filelist {
        if options.verbose {
            eprintln!("loading {}", filelist.display());
        }
        SearchIndex::new(efu::read_file_list(filelist)?, options.match_path)
    } else if let Some(root) = &options.root {
        if options.verbose {
            eprintln!("scanning {} ...", root.display());
        }
        scan::scan_index(root)?.0
    } else {
        bail!("nothing to search: pass -filelist <list.efu> or -root <folder>");
    };

    if options.verbose {
        eprintln!("indexed {} entries", index.live_len());
    }
    let search_options = SearchOptions {
        case_sensitive: options.case_sensitive,
        whole_word: options.whole_word,
        match_path: options.match_path,
        regex: options.regex,
    };
    let query = search::Query::parse(query_text, search_options)?;
    let hits = search::search(&index, &query, options.sort, options.desc, options.limit);

    output(&hits, &options)?;
    eprintln!("{} item(s)", hits.len());
    Ok(())
}

fn output(hits: &[Entry], options: &Options) -> Result<()> {
    if let Some(path) = &options.export_txt {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let file =
            File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        for entry in hits {
            writeln!(writer, "{}", entry.path.display())?;
        }
        writer.flush()?;
        return Ok(());
    }

    if options.json {
        let values: Vec<serde_json::Value> = hits
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "path": entry.path.display().to_string(),
                    "name": entry.name_string(),
                    "directory": entry.is_dir,
                    "size": entry.size,
                    "modified": entry.modified,
                    "created": entry.created,
                    "attributes": entry.attributes,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&values)?);
        return Ok(());
    }

    let stdout = std::io::stdout();
    let mut writer = std::io::BufWriter::new(stdout.lock());
    for entry in hits {
        writeln!(writer, "{}", entry.path.display())?;
    }
    writer.flush()?;
    Ok(())
}
