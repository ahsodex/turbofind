// TurboFind - Fast File Indexer for Windows
// Crawls filesystem in parallel, caches index to disk, fuzzy searches with nucleo

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, terminal,
};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32Str};
use rayon::prelude::*;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// Check if a path_filter looks like an absolute path (drive letter or starts with \)
fn is_absolute_filter(pf: &str) -> bool {
    (pf.len() >= 2 && pf.as_bytes()[1] == b':') || pf.starts_with('\\')
}

/// Single indexed file/directory entry
#[derive(Serialize, Deserialize)]
struct FileEntry {
    path: String,
    path_lower: String, // pre-computed for case-insensitive path matching
    name: String,
    name_lower: String, // pre-computed for case-insensitive matching
    size: u64,
    is_dir: bool,
    extension: String, // lowercase, no dot
}

/// In-memory search index with extension lookup table
#[derive(Serialize, Deserialize)]
struct FileIndex {
    entries: Vec<FileEntry>,
    ext_map: HashMap<String, Vec<usize>>, // extension -> indices into entries
    indexed_at: u64,                      // when this index was built
    roots: Vec<String>,                   // which paths were indexed
}

/// Single search result — may include content match info
struct SearchHit<'a> {
    entry: &'a FileEntry,
    score: i64,
    line_num: usize,   // 0 = filename match only
    line_text: String,  // empty = filename match only
}

/// Extensions that are definitely binary — skip for content search
fn is_binary_extension(ext: &str) -> bool {
    matches!(
        ext,
        "exe" | "dll" | "so" | "dylib" | "bin" | "obj" | "o" | "a" | "lib"
            | "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst"
            | "jpg" | "jpeg" | "png" | "gif" | "bmp" | "ico" | "webp" | "tiff"
            | "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "wma"
            | "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv"
            | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx"
            | "odt" | "ods" | "odp" | "rtf"
            | "msi" | "cab" | "iso" | "img" | "dmg"
            | "class" | "pyc" | "pyo" | "wasm"
            | "ttf" | "otf" | "woff" | "woff2" | "eot"
            | "db" | "sqlite" | "mdb" | "pdb" | "dmp"
    )
}

/// Helper: does entry path match the path filter?
fn path_matches(entry_path_lower: &str, pf: &str) -> bool {
    if is_absolute_filter(pf) {
        // absolute path — prefix match (so in:C:\Downloads matches C:\Downloads\...)
        entry_path_lower.starts_with(pf)
    } else {
        // relative/fragment — substring contains
        entry_path_lower.contains(pf)
    }
}

/// Split query into tokens, respecting double-quoted values (e.g. in:"path with spaces")
fn split_query(query: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut chars = query.chars().peekable();
    while chars.peek().is_some() {
        while chars.peek() == Some(&' ') { chars.next(); }
        if chars.peek().is_none() { break; }
        let mut part = String::new();
        let mut in_quote = false;
        loop {
            match chars.peek() {
                None => break,
                Some(&' ') if !in_quote => break,
                Some(&'"') => {
                    in_quote = !in_quote;
                    chars.next();
                }
                Some(&c) => {
                    part.push(c);
                    chars.next();
                }
            }
        }
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts
}

/// Check if extension supports document text extraction (requires ext: filter for content search)
fn is_document_extension(ext: &str) -> bool {
    matches!(ext, "pdf" | "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "rtf")
}

/// Strip XML tags and decode basic entities, inserting newlines at paragraph boundaries
fn strip_xml_tags(xml: &str) -> String {
    let mut result = String::with_capacity(xml.len() / 2);
    let mut in_tag = false;
    let mut tag_buf = String::new();
    for c in xml.chars() {
        if c == '<' {
            in_tag = true;
            tag_buf.clear();
        } else if c == '>' {
            in_tag = false;
            // paragraph/row boundaries become newlines for line-based searching
            let tl = tag_buf.to_lowercase();
            if tl.starts_with("/w:p") || tl.starts_with("/text:p")
                || tl.starts_with("/a:p") || tl.starts_with("/si")
                || tl.starts_with("/row") || tl.starts_with("/p")
            {
                if !result.ends_with('\n') {
                    result.push('\n');
                }
            }
        } else if in_tag {
            tag_buf.push(c);
        } else {
            result.push(c);
        }
    }
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Extract text from ZIP-based document formats (DOCX, XLSX, PPTX, ODT/ODS/ODP)
fn extract_zip_xml_text(path: &str, prefixes: &[&str]) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut all_text = String::new();
    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.name().to_string();
        if name.ends_with(".xml") && prefixes.iter().any(|p| name.starts_with(p)) {
            let mut xml = String::new();
            if entry.read_to_string(&mut xml).is_ok() {
                all_text.push_str(&strip_xml_tags(&xml));
                all_text.push('\n');
            }
        }
    }
    if all_text.is_empty() { None } else { Some(all_text) }
}

/// Extract text from PDF files
fn extract_pdf_text(path: &str) -> Option<String> {
    pdf_extract::extract_text(path).ok()
}

/// Extract text from RTF files (simple parser — handles control words, hex escapes)
fn extract_rtf_text(path: &str) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let mut result = String::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        match bytes[i] {
            b'{' | b'}' => i += 1,
            b'\\' => {
                i += 1;
                if i >= len { break; }
                if bytes[i] == b'\\' { result.push('\\'); i += 1; continue; }
                if bytes[i] == b'{' { result.push('{'); i += 1; continue; }
                if bytes[i] == b'}' { result.push('}'); i += 1; continue; }
                if bytes[i] == b'\r' || bytes[i] == b'\n' { i += 1; continue; }
                if bytes[i] == b'\'' {
                    // hex escape \'XX
                    i += 1;
                    if i + 1 < len {
                        if let Ok(val) = u8::from_str_radix(
                            std::str::from_utf8(&bytes[i..i + 2]).unwrap_or(""),
                            16,
                        ) {
                            result.push(val as char);
                        }
                        i += 2;
                    }
                    continue;
                }
                // control word
                let word_start = i;
                while i < len && bytes[i].is_ascii_alphabetic() { i += 1; }
                let word = std::str::from_utf8(&bytes[word_start..i]).unwrap_or("");
                // skip optional numeric parameter
                if i < len && (bytes[i] == b'-' || bytes[i].is_ascii_digit()) {
                    if bytes[i] == b'-' { i += 1; }
                    while i < len && bytes[i].is_ascii_digit() { i += 1; }
                }
                // skip delimiter space
                if i < len && bytes[i] == b' ' { i += 1; }
                match word {
                    "par" | "line" => result.push('\n'),
                    "tab" => result.push('\t'),
                    _ => {}
                }
            }
            b'\r' | b'\n' => i += 1,
            c => { result.push(c as char); i += 1; }
        }
    }
    if result.trim().is_empty() { None } else { Some(result) }
}

/// Dispatch to the right extractor based on file extension
fn extract_document_text(path: &str, ext: &str) -> Option<String> {
    match ext {
        "pdf" => extract_pdf_text(path),
        "docx" => extract_zip_xml_text(path, &["word/document", "word/header", "word/footer"]),
        "xlsx" => extract_zip_xml_text(path, &["xl/sharedStrings", "xl/worksheets/"]),
        "pptx" => extract_zip_xml_text(path, &["ppt/slides/slide"]),
        "odt" | "ods" | "odp" => extract_zip_xml_text(path, &["content"]),
        "rtf" => extract_rtf_text(path),
        _ => None,
    }
}

impl FileIndex {
    /// Crawl filesystem roots in parallel and build the index
    fn build(roots: &[&str]) -> Self {
        let start = Instant::now();
        println!("  Indexing filesystem...");

        // rayon parallelizes across roots, walkdir handles recursion
        let all_entries: Vec<FileEntry> = roots
            .par_iter()
            .flat_map(|root| {
                let mut entries = Vec::new();
                for entry in WalkDir::new(root)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    let path = entry.path();
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();

                    // skip hidden and system directories
                    if name.starts_with('.') || name.starts_with('$') {
                        continue;
                    }

                    let metadata = entry.metadata().ok();
                    let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                    let is_dir = entry.file_type().is_dir();
                    let extension = path
                        .extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                        .unwrap_or_default();

                    let path_str = path.to_string_lossy().to_string();
                    entries.push(FileEntry {
                        path_lower: path_str.to_lowercase(),
                        path: path_str,
                        name_lower: name.to_lowercase(),
                        name,
                        size,
                        is_dir,
                        extension,
                    });
                }
                entries
            })
            .collect();

        // build extension -> index lookup for O(1) ext: filtering
        let mut ext_map: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, entry) in all_entries.iter().enumerate() {
            if !entry.extension.is_empty() {
                ext_map.entry(entry.extension.clone()).or_default().push(i);
            }
        }

        let count = all_entries.len();
        let elapsed = start.elapsed();
        println!(
            "  Indexed {} files in {:.2}s ({:.0} files/sec)",
            count,
            elapsed.as_secs_f64(),
            count as f64 / elapsed.as_secs_f64()
        );

        Self {
            entries: all_entries,
            ext_map,
            indexed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            roots: roots.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Serialize index to binary file (postcard) for fast reload
    fn save(&self, path: &Path) -> io::Result<()> {
        let data = postcard::to_allocvec(self)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        fs::write(path, data)
    }

    /// Merge another index into this one (adds new entries, deduplicates roots)
    fn merge(&mut self, other: Self) {
        let base = self.entries.len();
        // add other's extension map entries, offset by base
        for (ext, indices) in other.ext_map {
            let entry = self.ext_map.entry(ext).or_default();
            for idx in indices {
                entry.push(base + idx);
            }
        }
        self.entries.extend(other.entries);
        for root in other.roots {
            if !self.roots.contains(&root) {
                self.roots.push(root);
            }
        }
        self.indexed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
    }

    /// Deserialize index from binary cache
    fn load(path: &Path) -> io::Result<Self> {
        let data = fs::read(path)?;
        // cap file size to prevent allocation bombs from tampered cache
        const MAX_CACHE_SIZE: usize = 512 * 1024 * 1024; // 512MB
        if data.len() > MAX_CACHE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cache file exceeds 512MB limit",
            ));
        }
        let index: Self = postcard::from_bytes(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        println!("  Loaded index: {} files from cache", index.entries.len());
        Ok(index)
    }

    /// Search with optional filters (ext:, dir:, regex:/re:, grep:/content:)
    /// Dispatches to fuzzy, regex, or content search based on filters
    fn search(&self, query: &str, max_results: usize, cancel: &AtomicBool) -> Vec<SearchHit<'_>> {
        // parse query into filters and search terms (quote-aware for paths with spaces)
        let parts = split_query(query);
        let mut ext_filter: Option<String> = None;
        let mut dir_only = false;
        let mut use_regex = false;
        let mut content_search = false;
        let mut grep_term = String::new();
        let mut path_filter: Option<String> = None;
        let mut search_terms: Vec<String> = Vec::new();

        for part in &parts {
            if let Some(ext) = part.strip_prefix("ext:") {
                ext_filter = Some(ext.to_lowercase().replace('.', ""));
            } else if let Some(p) = part.strip_prefix("in:") {
                if !p.is_empty() {
                    path_filter = Some(p.to_lowercase().replace('/', "\\"));
                }
            } else if part == "dir:" || part == "folder:" {
                dir_only = true;
            } else if part == "re:" || part == "regex:" {
                use_regex = true;
            } else if let Some(rest) = part
                .strip_prefix("re:")
                .or_else(|| part.strip_prefix("regex:"))
            {
                use_regex = true;
                if !rest.is_empty() {
                    search_terms.push(rest.to_string());
                }
            } else if part == "grep:" || part == "content:" {
                content_search = true;
            } else if let Some(term) = part
                .strip_prefix("grep:")
                .or_else(|| part.strip_prefix("content:"))
            {
                content_search = true;
                if !term.is_empty() {
                    grep_term = term.to_string();
                }
            } else {
                search_terms.push(part.clone());
            }
        }

        // build search query: for content search, prefer grep_term if given
        let search_query = if content_search && !grep_term.is_empty() {
            // combine grep_term with any remaining search_terms
            let mut q = grep_term;
            if !search_terms.is_empty() {
                q.push(' ');
                q.push_str(&search_terms.join(" "));
            }
            q
        } else {
            search_terms.join(" ")
        };

        // dispatch to content search
        if content_search {
            return self.search_content(
                &search_query,
                use_regex,
                ext_filter.as_deref(),
                path_filter.as_deref(),
                max_results,
                cancel,
            );
        }

        // dispatch to regex filename search
        if use_regex && !search_query.is_empty() {
            let re = match RegexBuilder::new(&search_query)
                .case_insensitive(true)
                .size_limit(1 << 20)
                .build()
            {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let mut results: Vec<SearchHit<'_>> = self
                .entries
                .par_iter()
                .filter_map(|entry| {
                    if let Some(ref ext) = ext_filter {
                        if &entry.extension != ext {
                            return None;
                        }
                    }
                    if dir_only && !entry.is_dir {
                        return None;
                    }
                    if let Some(ref pf) = path_filter {
                        if !path_matches(&entry.path_lower, pf.as_str()) {
                            return None;
                        }
                    }
                    if re.is_match(&entry.name_lower) {
                        Some(SearchHit {
                            entry,
                            score: 0,
                            line_num: 0,
                            line_text: String::new(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            results.truncate(max_results);
            return results;
        }

        // fuzzy filename search (default)
        let search_lower = search_query.to_lowercase();
        let pattern = Pattern::parse(&search_query, CaseMatching::Ignore, Normalization::Smart);

        let mut results: Vec<SearchHit<'_>> = self
            .entries
            .par_iter()
            .map_init(
                || (Matcher::new(nucleo::Config::DEFAULT), Vec::new()),
                |(matcher, buf), entry| {
                    if let Some(ref ext) = ext_filter {
                        if &entry.extension != ext {
                            return None;
                        }
                    }
                    if dir_only && !entry.is_dir {
                        return None;
                    }
                    if let Some(ref pf) = path_filter {
                        if !path_matches(&entry.path_lower, pf.as_str()) {
                            return None;
                        }
                    }
                    if search_query.is_empty() {
                        return Some(SearchHit {
                            entry,
                            score: 0i64,
                            line_num: 0,
                            line_text: String::new(),
                        });
                    }

                    pattern
                        .score(Utf32Str::new(&entry.name_lower, buf), matcher)
                        .map(|score| {
                            let mut s = score as i64;
                            if entry.name_lower.contains(&search_lower) {
                                s += 10000;
                            }
                            SearchHit {
                                entry,
                                score: s,
                                line_num: 0,
                                line_text: String::new(),
                            }
                        })
                },
            )
            .flatten()
            .collect();

        results.sort_by(|a, b| b.score.cmp(&a.score));
        results.truncate(max_results);
        results
    }

    /// Search file contents for a pattern (plain text or regex)
    fn search_content(
        &self,
        query: &str,
        use_regex: bool,
        ext_filter: Option<&str>,
        path_filter: Option<&str>,
        max_results: usize,
        cancel: &AtomicBool,
    ) -> Vec<SearchHit<'_>> {
        if query.is_empty() {
            return Vec::new();
        }

        const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB

        // narrow candidates by extension and path
        let candidates: Vec<&FileEntry> = if let Some(ext) = ext_filter {
            if let Some(indices) = self.ext_map.get(ext) {
                indices
                    .iter()
                    .map(|&i| &self.entries[i])
                    .filter(|e| {
                        !e.is_dir
                            && e.size > 0
                            && e.size < MAX_FILE_SIZE
                            && path_filter
                                .map_or(true, |pf| path_matches(&e.path_lower, pf))
                    })
                    .collect()
            } else {
                return Vec::new();
            }
        } else {
            self.entries
                .iter()
                .filter(|e| {
                    !e.is_dir
                        && e.size > 0
                        && e.size < MAX_FILE_SIZE
                        && (!is_binary_extension(&e.extension)
                            || (is_document_extension(&e.extension) && path_filter.is_some()))
                        && path_filter
                            .map_or(true, |pf| path_matches(&e.path_lower, pf))
                })
                .collect()
        };

        let regex = if use_regex {
            match RegexBuilder::new(query)
                .case_insensitive(true)
                .size_limit(1 << 20)
                .build()
            {
                Ok(r) => Some(r),
                Err(_) => return Vec::new(),
            }
        } else {
            None
        };
        let search_lower = query.to_lowercase();

        // single-file mode: if only 1 candidate, show all matches instead of capping at 5
        let per_file_limit = if candidates.len() == 1 { usize::MAX } else { 5 };
        let mut hits: Vec<SearchHit<'_>> = candidates
            .par_iter()
            .flat_map(|entry| {
                if cancel.load(Ordering::Relaxed) {
                    return Vec::new();
                }
                let content = if is_document_extension(&entry.extension) {
                    match extract_document_text(&entry.path, &entry.extension) {
                        Some(c) => c,
                        None => return Vec::new(),
                    }
                } else {
                    match fs::read_to_string(&entry.path) {
                        Ok(c) => c,
                        Err(_) => return Vec::new(),
                    }
                };
                let mut file_hits = Vec::new();
                for (line_idx, line) in content.lines().enumerate() {
                    let matched = if let Some(ref re) = regex {
                        re.is_match(line)
                    } else {
                        line.to_lowercase().contains(&search_lower)
                    };
                    if matched {
                        let safe_line = sanitize_terminal_text(line.trim());
                        file_hits.push(SearchHit {
                            entry,
                            score: 0,
                            line_num: line_idx + 1,
                            line_text: safe_line.chars().take(200).collect(),
                        });
                        if file_hits.len() >= per_file_limit {
                            break;
                        }
                    }
                }
                file_hits
            })
            .collect();

        // group by file path then line number for readable output
        hits.sort_by(|a, b| {
            a.entry.path.cmp(&b.entry.path)
                .then(a.line_num.cmp(&b.line_num))
        });
        hits.truncate(max_results);
        hits
    }
}

/// Pad or truncate string to exactly w chars.
/// Ensures every TUI line is the same width — prevents flicker and line wrapping.
fn fit(s: &str, w: usize) -> String {
    let char_count = s.chars().count();
    if char_count >= w {
        s.chars().take(w).collect()
    } else {
        format!("{:<width$}", s, width = w)
    }
}

/// Remove terminal-control and hard line-separator chars from untrusted text.
/// Keeps display text inert so file contents cannot emit terminal escape sequences.
fn sanitize_terminal_text(s: &str) -> String {
    s.chars().filter(|&c| is_safe_terminal_char(c)).collect()
}

/// True if a character is safe to pass through to terminal output.
fn is_safe_terminal_char(c: char) -> bool {
    if c.is_control() {
        return false;
    }

    // Remove hard line separators.
    if matches!(c, '\u{2028}' | '\u{2029}') {
        return false;
    }

    // Remove invisible formatting and bidi controls that can spoof/reorder text.
    if matches!(
        c,
        '\u{200B}'..='\u{200F}' // ZW* + direction marks
            | '\u{202A}'..='\u{202E}' // bidi embedding/override
            | '\u{2060}'..='\u{2064}' // word joiner/invisible ops
            | '\u{2066}'..='\u{2069}' // bidi isolate controls
            | '\u{FEFF}' // BOM / zero-width no-break space
    ) {
        return false;
    }

    // Remove Unicode noncharacters.
    let u = c as u32;
    if (0xFDD0..=0xFDEF).contains(&u) || (u & 0xFFFE == 0xFFFE) {
        return false;
    }

    true
}

/// Count visible characters, excluding ANSI escape sequences
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            len += 1;
        }
    }
    len
}

/// Pad or truncate to w visible chars, preserving ANSI escape codes
fn fit_styled(s: &str, w: usize) -> String {
    let vis = visible_len(s);
    if vis >= w {
        let mut result = String::new();
        let mut visible = 0;
        let mut in_escape = false;
        for c in s.chars() {
            if in_escape {
                result.push(c);
                if c.is_ascii_alphabetic() {
                    in_escape = false;
                }
            } else if c == '\x1b' {
                in_escape = true;
                result.push(c);
            } else {
                if visible >= w {
                    break;
                }
                result.push(c);
                visible += 1;
            }
        }
        result.push_str("\x1b[0m");
        result
    } else {
        format!("{}{:width$}", s, "", width = w - vis)
    }
}

/// Highlight first occurrence of term in text (case-insensitive) with bold red ANSI
fn highlight_term(text: &str, term: &str) -> String {
    if term.is_empty() {
        return text.to_string();
    }
    let tl = text.to_lowercase();
    let terml = term.to_lowercase();
    if let Some(pos) = tl.find(&terml) {
        let end = pos + terml.len();
        if end <= text.len() && text.is_char_boundary(pos) && text.is_char_boundary(end) {
            let before = &text[..pos];
            let matched = &text[pos..end];
            let after = &text[end..];
            return format!("{}\x1b[1;31m{}\x1b[0m{}", before, matched, after);
        }
    }
    text.to_string()
}

/// Extract the search term from a query (strips filter prefixes)
fn extract_search_term(query: &str) -> String {
    let mut grep_term = String::new();
    let mut extra = Vec::new();
    for part in split_query(query) {
        if let Some(t) = part.strip_prefix("grep:").or_else(|| part.strip_prefix("content:")) {
            if !t.is_empty() { grep_term = t.to_string(); }
        } else if !part.starts_with("ext:") && !part.starts_with("in:")
            && part != "dir:" && part != "folder:"
            && part != "re:" && part != "regex:"
            && !part.starts_with("re:") && !part.starts_with("regex:") {
            extra.push(part);
        }
    }
    if !grep_term.is_empty() { grep_term } else { extra.join(" ") }
}

/// Human-readable file size (compact format)
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{}K", bytes / KB)
    } else {
        format!("{}B", bytes)
    }
}

/// Interactive TUI — renders entire frame into a buffer, writes once to avoid flicker
fn run_tui(index: &mut FileIndex, cache_path: &Path, roots: &[&str]) -> io::Result<()> {
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let mut query = String::new();
    let mut cursor_pos: usize = 0; // char position within query
    let mut results: Vec<SearchHit<'_>> = Vec::new();
    let mut selected: usize = 0;
    let mut scroll_offset: usize = 0;
    let mut search_time = std::time::Duration::ZERO;
    let mut buf = String::with_capacity(16384); // pre-alloc frame buffer
    let mut show_help = false;
    let mut help_scroll: usize = 0;
    let no_cancel = AtomicBool::new(false);
    // owned storage for content search results (can't borrow index across thread boundary)
    let mut content_results: Vec<(FileEntry, i64, usize, String)> = Vec::new();
    let mut use_content_results = false;
    let mut content_searched = false; // true after a content search has been executed
    // preview pane state
    let mut preview_lines: Vec<String> = Vec::new();
    let mut last_preview_path = String::new();

    loop {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let w = cols as usize;
        let total_rows = (rows as usize).saturating_sub(6); // rows available for results + preview

        // build entire frame into buf, then write once = zero flicker
        buf.clear();

        if show_help {
            let help_lines: Vec<&str> = vec![
                "  TurboFind - Help",
                "",
                "  SEARCH FILTERS:",
                "    ext:rs        Filter by file extension",
                "    dir:          Show only directories",
                "    in:path       Restrict to paths containing 'path'",
                "                  Use full path for prefix match: in:C:\\Dir",
                "    regex:pat     Regex match on filenames",
                "    grep:term     Search file contents (plain text)",
                "    content:term  Same as grep:",
                "    grep:pat regex:  Regex search file contents",
                "",
                "  DOCUMENT SEARCH (use ext: or in: to target):",
                "    ext:pdf grep:term    PDF (text-based only, not scanned)",
                "    ext:docx grep:term   Word documents",
                "    ext:xlsx grep:term   Excel spreadsheets",
                "    ext:pptx grep:term   PowerPoint slides",
                "    ext:odt/ods/odp      LibreOffice documents",
                "    ext:rtf grep:term    Rich Text Format",
                "    in:file.docx grep:term  Search specific document via in:",
                "",
                "  EXAMPLES:",
                "    budget              Fuzzy match all files",
                "    ext:rs config       Only .rs files matching 'config'",
                "    in:src ext:py       .py files under paths with 'src'",
                "    regex:\\.test\\.      Regex match filenames",
                "    grep:TODO in:src    Search contents under 'src'",
                "",
                "  KEYS:",
                "    Up/Down       Navigate results (scroll help)",
                "    PgUp/PgDown   Scroll by page",
                "    Home/End      Jump to first/last result",
                "    Tab           Complete in: with selected path (file or dir)",
                "    Ctrl+Left/Right  Jump cursor by word",
                "    Ctrl+Home/End Jump cursor to start/end",
                "    Ctrl+U        Clear search line",
                "    Ctrl+K        Delete to end of line",
                "    Ctrl+Bksp     Delete word before cursor",
                "    Ctrl+Del      Delete word after cursor",
                "    Enter         Show file in folder",
                "    Ctrl+O        Open file directly",
                "    Ctrl+R        Rebuild index",
                "    Ctrl+N        Add path to index",
                "    Ctrl+B        Run benchmark",
                "    F1            Toggle this help",
                "    Esc           Quit",
                "",
                "  CLI FLAGS:",
                "    --help      Show usage and exit",
                "    --reindex   Force rebuild (ignore cache)",
                "    --no-cache  Don't read or write cache",
            ];

            let visible_rows = (rows as usize).saturating_sub(1); // -1 for footer
            let max_scroll = help_lines.len().saturating_sub(visible_rows);
            if help_scroll > max_scroll {
                help_scroll = max_scroll;
            }

            for i in 0..visible_rows {
                let line_idx = help_scroll + i;
                if line_idx < help_lines.len() {
                    buf.push_str(&fit(help_lines[line_idx], w));
                } else {
                    buf.push_str(&fit("", w));
                }
            }

            execute!(stdout, cursor::MoveTo(0, 0))?;
            write!(stdout, "{}", buf)?;
            execute!(stdout, cursor::MoveTo(0, rows - 1))?;
            let footer = if max_scroll > 0 {
                format!("  F1/Esc:close | Up/Down/PgUp/PgDn:scroll [{}/{}]", help_scroll + 1, max_scroll + 1)
            } else {
                "  Press F1 or Esc to close help".to_string()
            };
            write!(stdout, "{}", fit(&footer, w))?;
            stdout.flush()?;

            if let Event::Key(key) = event::read()? {
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::F(1) | KeyCode::Esc => { show_help = false; help_scroll = 0; }
                    KeyCode::Up => { if help_scroll > 0 { help_scroll -= 1; } }
                    KeyCode::Down => { if help_scroll < max_scroll { help_scroll += 1; } }
                    KeyCode::PageUp => { help_scroll = help_scroll.saturating_sub(visible_rows); }
                    KeyCode::PageDown => { help_scroll = (help_scroll + visible_rows).min(max_scroll); }
                    KeyCode::Home => { help_scroll = 0; }
                    KeyCode::End => { help_scroll = max_scroll; }
                    _ => {}
                }
            }
            continue;
        }

        // row 0: header with file count and search stats
        let is_content_query = query.split_whitespace().any(|p| {
            p == "grep:" || p == "content:"
                || p.starts_with("grep:") || p.starts_with("content:")
        });
        let result_count = if use_content_results {
            content_results.len()
        } else {
            results.len()
        };

        // layout: split available rows between results and preview
        let show_preview = result_count > 0;
        let result_rows = if show_preview {
            (total_rows * 60 / 100).max(3)
        } else {
            total_rows
        };
        let preview_rows = if show_preview {
            total_rows.saturating_sub(result_rows + 1) // -1 for preview separator
        } else {
            0
        };

        // load preview file content when selection changes
        let (sel_path, sel_line, sel_ext, sel_is_dir) = if use_content_results {
            content_results.get(selected)
                .map(|(e, _, ln, _)| (e.path.as_str(), *ln, e.extension.as_str(), e.is_dir))
                .unwrap_or(("", 0, "", false))
        } else {
            results.get(selected)
                .map(|h| (h.entry.path.as_str(), h.line_num, h.entry.extension.as_str(), h.entry.is_dir))
                .unwrap_or(("", 0, "", false))
        };
        if sel_path != last_preview_path.as_str() {
            last_preview_path = sel_path.to_string();
            if sel_is_dir {
                preview_lines = vec!["(directory)".to_string()];
            } else if !sel_path.is_empty() && is_document_extension(sel_ext) {
                preview_lines = extract_document_text(sel_path, sel_ext)
                    .filter(|c| c.len() <= 1_048_576)
                    .map(|c| c.lines().map(sanitize_terminal_text).collect())
                    .unwrap_or_else(|| vec!["(cannot extract text)".to_string()]);
            } else if !sel_path.is_empty() && !is_binary_extension(sel_ext) {
                preview_lines = fs::read_to_string(sel_path)
                    .ok()
                    .filter(|c| c.len() <= 1_048_576)
                    .map(|c| c.lines().map(sanitize_terminal_text).collect())
                    .unwrap_or_else(|| vec!["(cannot read file)".to_string()]);
            } else if !sel_path.is_empty() {
                preview_lines = vec!["(binary file)".to_string()];
            } else {
                preview_lines = Vec::new();
            }
        }
        let preview_highlight = sel_line; // 1-based line to highlight, 0 = none
        let search_term = extract_search_term(&query);

        let header = if is_content_query && result_count == 0 && !query.is_empty() && !content_searched {
            format!(
                "  TurboFind | {} files | Press Enter to search",
                index.entries.len()
            )
        } else if !query.is_empty() {
            let scroll_info = if result_count > result_rows {
                format!(" [{}-{}/{}]", scroll_offset + 1, (scroll_offset + result_rows).min(result_count), result_count)
            } else {
                String::new()
            };
            format!(
                "  TurboFind | {} files | {} results in {:.1}ms{}",
                index.entries.len(),
                result_count,
                search_time.as_secs_f64() * 1000.0,
                scroll_info
            )
        } else {
            format!("  TurboFind | {} files indexed", index.entries.len())
        };
        buf.push_str(&fit(&header, w));

        // row 1: spacer
        buf.push_str(&fit("", w));

        // row 2: search input with cursor (horizontally scrollable)
        let search_line = if query.is_empty() {
            "  > Filters: ext:rs dir: in:path regex: grep:term content:term".to_string()
        } else {
            let prefix = "  > ";
            let available = w.saturating_sub(prefix.len());
            let before: String = query.chars().take(cursor_pos).collect();
            let after: String = query.chars().skip(cursor_pos).collect();
            let full_display = format!("{}|{}", before, after);
            let full_len = full_display.chars().count();
            if full_len <= available {
                format!("{}{}", prefix, full_display)
            } else {
                // scroll so cursor (|) stays visible, roughly centered
                let cursor_char_pos = before.chars().count();
                let half = available / 2;
                let start = if cursor_char_pos <= half {
                    0
                } else {
                    (cursor_char_pos - half).min(full_len.saturating_sub(available))
                };
                let visible: String = full_display.chars().skip(start).take(available).collect();
                format!("{}{}", prefix, visible)
            }
        };
        buf.push_str(&fit(&search_line, w));

        // row 3: separator
        buf.push_str(&fit(&"\u{2500}".repeat(w), w));

        // ensure scroll_offset keeps selected visible
        if selected < scroll_offset {
            scroll_offset = selected;
        } else if selected >= scroll_offset + result_rows {
            scroll_offset = selected + 1 - result_rows;
        }

        // rows 4+: search results with color highlighting
        for i in 0..result_rows {
            let result_idx = scroll_offset + i;
            // get entry data from either content_results or regular results
            let hit_data: Option<(&FileEntry, i64, usize, &str)> = if use_content_results {
                content_results.get(result_idx).map(|(e, s, ln, lt)| (e, *s, *ln, lt.as_str()))
            } else {
                results.get(result_idx).map(|h| (h.entry, h.score, h.line_num, h.line_text.as_str()))
            };
            if let Some((entry, _score, line_num, line_text)) = hit_data {
                let tag = if entry.is_dir {
                    "DIR"
                } else {
                    match entry.extension.as_str() {
                        "rs" | "py" | "js" | "ts" | "c" | "cpp" | "java" | "go" => "SRC",
                        "jpg" | "png" | "gif" | "bmp" | "svg" | "webp" => "IMG",
                        "mp3" | "wav" | "flac" | "ogg" | "m4a" => "AUD",
                        "mp4" | "mkv" | "avi" | "mov" | "webm" => "VID",
                        "zip" | "rar" | "7z" | "tar" | "gz" => "ZIP",
                        "exe" | "msi" => "EXE",
                        "pdf" => "PDF",
                        "doc" | "docx" => "DOC",
                        "xls" | "xlsx" => "XLS",
                        _ => "   ",
                    }
                };

                // show last 2 path components to keep it readable
                let parent = Path::new(&entry.path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let short_path: String = Path::new(&parent)
                    .components()
                    .rev()
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("\\");

                let size_str = if entry.is_dir {
                    String::new()
                } else {
                    format_size(entry.size)
                };

                let is_selected = result_idx == selected;
                if is_selected {
                    // selected: reverse video, plain text for clarity
                    let line = if line_num > 0 {
                        format!(" > [{}] {}  {}  L{}:{}", tag, entry.name, short_path, line_num, line_text)
                    } else {
                        format!(" > [{}] {}  {}  {}", tag, entry.name, short_path, size_str)
                    };
                    buf.push_str(&format!("\x1b[7m{}\x1b[0m", fit(&line, w)));
                } else {
                    // non-selected: colored parts
                    if line_num > 0 {
                        let highlighted_text = highlight_term(line_text, &search_term);
                        let line = format!(
                            "   \x1b[90m[{}]\x1b[0m \x1b[33m{}\x1b[0m  \x1b[90m{}\x1b[0m  \x1b[36mL{}\x1b[0m:{}",
                            tag, entry.name, short_path, line_num, highlighted_text
                        );
                        buf.push_str(&fit_styled(&line, w));
                    } else {
                        // filename match: highlight search term in name
                        let name_styled = if !search_term.is_empty() {
                            let nl = entry.name_lower.as_str();
                            let tl = search_term.to_lowercase();
                            if let Some(pos) = nl.find(&tl) {
                                let end = pos + tl.len();
                                if end <= entry.name.len()
                                    && entry.name.is_char_boundary(pos)
                                    && entry.name.is_char_boundary(end)
                                {
                                    format!(
                                        "\x1b[33m{}\x1b[1;31m{}\x1b[0;33m{}\x1b[0m",
                                        &entry.name[..pos],
                                        &entry.name[pos..end],
                                        &entry.name[end..]
                                    )
                                } else {
                                    format!("\x1b[33m{}\x1b[0m", entry.name)
                                }
                            } else {
                                format!("\x1b[33m{}\x1b[0m", entry.name)
                            }
                        } else {
                            format!("\x1b[33m{}\x1b[0m", entry.name)
                        };
                        let line = format!(
                            "   \x1b[90m[{}]\x1b[0m {}  \x1b[90m{}\x1b[0m  {}",
                            tag, name_styled, short_path, size_str
                        );
                        buf.push_str(&fit_styled(&line, w));
                    }
                }
            } else {
                // blank line to overwrite stale results
                buf.push_str(&fit("", w));
            }
        }

        // preview separator + preview pane
        if show_preview && preview_rows > 0 {
            // preview separator with filename
            let preview_fname = Path::new(&last_preview_path)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            let sep_label = format!("\u{2500}\u{2500} \x1b[1m{}\x1b[0m ", preview_fname);
            let sep_vis_len = 2 + preview_fname.len() + 1;
            let sep_right = "\u{2500}".repeat(w.saturating_sub(sep_vis_len));
            buf.push_str(&fit_styled(&format!("{}{}", sep_label, sep_right), w));

            // center preview on matching line
            let preview_center = if preview_highlight > 0 {
                preview_highlight.saturating_sub(1) // 0-based
            } else {
                0
            };
            let preview_start = preview_center.saturating_sub(preview_rows / 2);

            for j in 0..preview_rows {
                let line_idx = preview_start + j;
                if line_idx < preview_lines.len() {
                    let ln = line_idx + 1; // 1-based display
                    let content = &preview_lines[line_idx];
                    let is_match_line = preview_highlight > 0 && ln == preview_highlight;

                    let preview_line = if is_match_line {
                        // highlighted match line: bold yellow line number; matched term in bold+underline+bright-yellow
                        let tl = content.to_lowercase();
                        let terml = search_term.to_lowercase();
                        let line_content = if !terml.is_empty() {
                            if let Some(pos) = tl.find(&terml) {
                                let end = pos + terml.len();
                                if end <= content.len()
                                    && content.is_char_boundary(pos)
                                    && content.is_char_boundary(end)
                                {
                                    format!(
                                        "{}\x1b[1;4;93m{}\x1b[0m{}",
                                        &content[..pos],
                                        &content[pos..end],
                                        &content[end..]
                                    )
                                } else {
                                    content.to_string()
                                }
                            } else {
                                content.to_string()
                            }
                        } else {
                            content.to_string()
                        };
                        format!("\x1b[1;33m{:>4}\x1b[0m {}", ln, line_content)
                    } else {
                        // context lines: bright near match, dimmed far
                        let dist = if preview_highlight > 0 {
                            (line_idx as isize - (preview_highlight as isize - 1)).unsigned_abs()
                        } else {
                            usize::MAX
                        };
                        if dist <= 3 {
                            format!("\x1b[36m{:>4}\x1b[0m {}", ln, content)
                        } else {
                            format!("\x1b[90m{:>4}\x1b[0m \x1b[90m{}\x1b[0m", ln, content)
                        }
                    };
                    buf.push_str(&fit_styled(&preview_line, w));
                } else {
                    buf.push_str(&fit("", w));
                }
            }
        }

        // single write of the entire frame buffer
        execute!(stdout, cursor::MoveTo(0, 0))?;
        write!(stdout, "{}", buf)?;

        // footer with keybindings
        execute!(stdout, cursor::MoveTo(0, rows - 1))?;
        write!(
            stdout,
            "{}",
            fit(
                "  F1:Help | Up/Dn/PgUp/PgDn | Tab:in:path | Ctrl+N:Add | Enter:Show | Ctrl+O:Open | Esc:Quit",
                w
            )
        )?;

        stdout.flush()?;

        // --- input handling ---
        // only process KeyPress events (Windows sends Press + Release)
        if let Event::Key(key) = event::read()? {
            if key.kind != event::KeyEventKind::Press {
                continue;
            }
            let mut needs_search = false;
            let mut force_search = false;
            match key {
                // exit
                KeyEvent {
                    code: KeyCode::Esc, ..
                } => break,
                KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => break,

                // delete character before cursor
                KeyEvent {
                    code: KeyCode::Backspace,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    if cursor_pos > 0 {
                        let idx = query.char_indices().nth(cursor_pos - 1).map(|(i, _)| i);
                        if let Some(i) = idx {
                            query.remove(i);
                            cursor_pos -= 1;
                        }
                    }
                    needs_search = true;
                }

                // delete word before cursor (Ctrl+Backspace)
                // Windows terminals may send Backspace+CTRL or Char('\x7f')
                KeyEvent {
                    code: KeyCode::Backspace,
                    modifiers,
                    ..
                } if modifiers.contains(KeyModifiers::CONTROL) => {
                    if cursor_pos > 0 {
                        let chars: Vec<char> = query.chars().collect();
                        let mut new_pos = cursor_pos.min(chars.len());
                        // skip delimiters backward (mirror of Ctrl+Right)
                        while new_pos > 0 && (chars[new_pos - 1] == ' ' || chars[new_pos - 1] == ':' || chars[new_pos - 1] == '\\' || chars[new_pos - 1] == '"') { new_pos -= 1; }
                        // skip word chars backward
                        while new_pos > 0
                            && chars[new_pos - 1] != ' '
                            && chars[new_pos - 1] != ':'
                            && chars[new_pos - 1] != '\\'
                            && chars[new_pos - 1] != '"'
                        { new_pos -= 1; }
                        let byte_start = query.char_indices().nth(new_pos).map(|(i, _)| i).unwrap_or(0);
                        let byte_end = query.char_indices().nth(cursor_pos).map(|(i, _)| i).unwrap_or(query.len());
                        query.replace_range(byte_start..byte_end, "");
                        cursor_pos = new_pos;
                    }
                    needs_search = true;
                }

                // Ctrl+Backspace alternative: Windows Terminal sends Char('\x7f')
                KeyEvent {
                    code: KeyCode::Char('\x7f'),
                    ..
                } => {
                    if cursor_pos > 0 {
                        let chars: Vec<char> = query.chars().collect();
                        let mut new_pos = cursor_pos.min(chars.len());
                        while new_pos > 0 && (chars[new_pos - 1] == ' ' || chars[new_pos - 1] == ':' || chars[new_pos - 1] == '\\' || chars[new_pos - 1] == '"') { new_pos -= 1; }
                        while new_pos > 0
                            && chars[new_pos - 1] != ' '
                            && chars[new_pos - 1] != ':'
                            && chars[new_pos - 1] != '\\'
                            && chars[new_pos - 1] != '"'
                        { new_pos -= 1; }
                        let byte_start = query.char_indices().nth(new_pos).map(|(i, _)| i).unwrap_or(0);
                        let byte_end = query.char_indices().nth(cursor_pos).map(|(i, _)| i).unwrap_or(query.len());
                        query.replace_range(byte_start..byte_end, "");
                        cursor_pos = new_pos;
                    }
                    needs_search = true;
                }

                // delete character at cursor
                KeyEvent {
                    code: KeyCode::Delete,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    let char_count = query.chars().count();
                    if cursor_pos < char_count {
                        let idx = query.char_indices().nth(cursor_pos).map(|(i, _)| i);
                        if let Some(i) = idx {
                            query.remove(i);
                        }
                    }
                    needs_search = true;
                }

                // delete word after cursor (Ctrl+Delete)
                KeyEvent {
                    code: KeyCode::Delete,
                    modifiers,
                    ..
                } if modifiers.contains(KeyModifiers::CONTROL) => {
                    let chars: Vec<char> = query.chars().collect();
                    let len = chars.len();
                    if cursor_pos < len {
                        let mut end = cursor_pos;
                        // skip word chars
                        while end < len
                            && chars[end] != ' '
                            && chars[end] != ':'
                            && chars[end] != '\\'
                            && chars[end] != '"'
                        { end += 1; }
                        // skip trailing delimiters/spaces
                        while end < len && (chars[end] == ' ' || chars[end] == ':' || chars[end] == '\\' || chars[end] == '"') {
                            end += 1;
                        }
                        let byte_start = query.char_indices().nth(cursor_pos).map(|(i, _)| i).unwrap_or(query.len());
                        let byte_end = query.char_indices().nth(end).map(|(i, _)| i).unwrap_or(query.len());
                        query.replace_range(byte_start..byte_end, "");
                    }
                    needs_search = true;
                }

                // clear entire line (Ctrl+U)
                KeyEvent {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    query.clear();
                    cursor_pos = 0;
                    needs_search = true;
                }

                // delete from cursor to end of line (Ctrl+K)
                KeyEvent {
                    code: KeyCode::Char('k'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    let byte_idx = query.char_indices()
                        .nth(cursor_pos)
                        .map(|(i, _)| i)
                        .unwrap_or(query.len());
                    query.truncate(byte_idx);
                    needs_search = true;
                }

                // open file directly with default application (Ctrl+O)
                KeyEvent {
                    code: KeyCode::Char('o'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    let sel_path = if use_content_results {
                        content_results.get(selected).map(|(e, _, _, _)| e.path.as_str())
                    } else {
                        results.get(selected).map(|h| h.entry.path.as_str())
                    };
                    if let Some(p) = sel_path {
                        let path_owned = p.to_string();
                        #[cfg(target_os = "windows")]
                        {
                            let _ = std::process::Command::new("explorer")
                                .arg(&path_owned)
                                .spawn();
                        }
                        #[cfg(target_os = "linux")]
                        {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(&path_owned)
                                .spawn();
                        }
                    }
                }

                // run benchmark (leaves TUI temporarily)
                KeyEvent {
                    code: KeyCode::Char('b'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    // exit alternate screen to show benchmark output
                    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
                    terminal::disable_raw_mode()?;

                    let test_queries = ["main", "config", "test", "cargo", "readme"];
                    println!("\n  Running benchmark...");
                    for q in &test_queries {
                        let start = Instant::now();
                        let iterations = 100;
                        for _ in 0..iterations {
                            let _ = index.search(q, 100, &no_cancel);
                        }
                        let avg = start.elapsed() / iterations;
                        println!("    '{}' -> avg {:.2}ms", q, avg.as_secs_f64() * 1000.0);
                    }
                    println!("  Press Enter to continue...");
                    let _ = io::stdin().read_line(&mut String::new());

                    // re-enter TUI
                    terminal::enable_raw_mode()?;
                    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
                }

                // rebuild index
                KeyEvent {
                    code: KeyCode::Char('r'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
                    terminal::disable_raw_mode()?;

                    println!("\n  Rebuilding index...");
                    *index = FileIndex::build(roots);
                    index.save(cache_path).ok();
                    query.clear();
                    results = Vec::new();
                    selected = 0;
                    scroll_offset = 0;
                    cursor_pos = 0;

                    terminal::enable_raw_mode()?;
                    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
                }

                // add a new root path to the index
                KeyEvent {
                    code: KeyCode::Char('n'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    // stay in raw mode but leave alternate screen so user sees the prompt
                    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;

                    // mini readline with filesystem Tab completion
                    let mut input = String::new();
                    print!("\r\n  Add path (Tab to complete, Enter to confirm, Esc to cancel): ");
                    print!("\r\n  > ");
                    stdout.flush()?;

                    let add_path = loop {
                        if let Event::Key(k) = event::read()? {
                            if k.kind != event::KeyEventKind::Press {
                                continue;
                            }
                            match k.code {
                                KeyCode::Esc => break None,
                                KeyCode::Enter => break Some(input.clone()),
                                KeyCode::Backspace => {
                                    if input.pop().is_some() {
                                        print!("\r  > {}\x1b[K", input);
                                        stdout.flush()?;
                                    }
                                }
                                KeyCode::Tab => {
                                    // filesystem Tab completion
                                    let partial = if input.is_empty() {
                                        ".".to_string()
                                    } else {
                                        input.clone()
                                    };
                                    let p = PathBuf::from(&partial);
                                    let (dir, prefix) = if p.is_dir() {
                                        (p, String::new())
                                    } else {
                                        let parent = p.parent().unwrap_or(Path::new(".")).to_path_buf();
                                        let prefix = p.file_name()
                                            .map(|f| f.to_string_lossy().to_lowercase())
                                            .unwrap_or_default();
                                        (parent, prefix)
                                    };
                                    if let Ok(entries) = fs::read_dir(&dir) {
                                        let mut matches: Vec<PathBuf> = entries
                                            .filter_map(|e| e.ok())
                                            .filter(|e| {
                                                if prefix.is_empty() {
                                                    true
                                                } else {
                                                    e.file_name()
                                                        .to_string_lossy()
                                                        .to_lowercase()
                                                        .starts_with(&prefix)
                                                }
                                            })
                                            .map(|e| e.path())
                                            .collect();
                                        matches.sort();
                                        if matches.len() == 1 {
                                            input = matches[0].to_string_lossy().to_string();
                                            if matches[0].is_dir() {
                                                input.push(std::path::MAIN_SEPARATOR);
                                            }
                                        } else if matches.len() > 1 {
                                            // show matches below prompt
                                            print!("\r\n");
                                            for (i, m) in matches.iter().take(15).enumerate() {
                                                let name = m.file_name()
                                                    .map(|f| f.to_string_lossy().to_string())
                                                    .unwrap_or_default();
                                                let suffix = if m.is_dir() { "\\" } else { "" };
                                                if i > 0 && i % 4 == 0 {
                                                    print!("\r\n");
                                                }
                                                print!("  {}{}", name, suffix);
                                            }
                                            if matches.len() > 15 {
                                                print!("  ...({} more)", matches.len() - 15);
                                            }
                                            print!("\r\n  > {}", input);
                                        }
                                    }
                                    print!("\r  > {}\x1b[K", input);
                                    stdout.flush()?;
                                }
                                KeyCode::Char(c) => {
                                    input.push(c);
                                    print!("\r  > {}\x1b[K", input);
                                    stdout.flush()?;
                                }
                                _ => {}
                            }
                        }
                    };

                    if let Some(raw) = add_path {
                        let raw = raw.trim().to_string();
                        if !raw.is_empty() {
                            let path = {
                                let p = PathBuf::from(&raw);
                                if p.is_relative() {
                                    std::env::current_dir()
                                        .map(|cwd| cwd.join(&p))
                                        .unwrap_or(p)
                                } else {
                                    p
                                }
                            };
                            let path_str = path.to_string_lossy().to_string();
                            if path.exists() {
                                print!("\r\n  Indexing {}...", path_str);
                                stdout.flush()?;
                                let new_index = FileIndex::build(&[path_str.as_str()]);
                                let added = new_index.entries.len();
                                index.merge(new_index);
                                index.save(cache_path).ok();
                                print!("\r\n  Added {} files. Total: {}", added, index.entries.len());
                            } else {
                                print!("\r\n  Path not found: {}", path_str);
                            }
                        }
                    }

                    query.clear();
                    results = Vec::new();
                    selected = 0;
                    scroll_offset = 0;
                    cursor_pos = 0;

                    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
                }

                // help overlay
                KeyEvent {
                    code: KeyCode::F(1),
                    ..
                } => {
                    show_help = true;
                }

                // Tab: complete in: filter with selected path (file or directory)
                KeyEvent {
                    code: KeyCode::Tab,
                    ..
                } => {
                    let sel_entry = if use_content_results {
                        content_results.get(selected).map(|(e, _, _, _)| e)
                    } else {
                        results.get(selected).map(|h| h.entry)
                    };
                    if let Some(entry) = sel_entry {
                        // for dirs, use dir path; for files, use the file's full path
                        // quote path if it contains spaces so the tokenizer keeps it as one token
                        let full_path = entry.path.clone();
                        let new_parts: Vec<String> = split_query(&query)
                            .into_iter()
                            .filter(|p| !p.starts_with("in:"))
                            .collect();
                        query = if full_path.contains(' ') {
                            format!("in:\"{}\"", full_path)
                        } else {
                            format!("in:{}", full_path)
                        };
                        if !new_parts.is_empty() {
                            query.push(' ');
                            query.push_str(&new_parts.join(" "));
                        }
                        cursor_pos = query.chars().count();
                        needs_search = true;
                    }
                }

                KeyEvent {
                    code: KeyCode::Char(c),
                    modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                    ..
                } => {
                    // filter out control characters (e.g. stray \x7f from terminals)
                    if !c.is_control() {
                        // insert at cursor position
                        let byte_idx = query.char_indices()
                            .nth(cursor_pos)
                            .map(|(i, _)| i)
                            .unwrap_or(query.len());
                        query.insert(byte_idx, c);
                        cursor_pos += 1;
                        needs_search = true;
                    }
                }

                // cursor movement within search query
                KeyEvent {
                    code: KeyCode::Left,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    if cursor_pos > 0 {
                        cursor_pos -= 1;
                    }
                }
                KeyEvent {
                    code: KeyCode::Right,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    if cursor_pos < query.chars().count() {
                        cursor_pos += 1;
                    }
                }

                // word jump (Ctrl+Left / Ctrl+Right)
                KeyEvent {
                    code: KeyCode::Left,
                    modifiers,
                    ..
                } if modifiers.contains(KeyModifiers::CONTROL) => {
                    if cursor_pos > 0 {
                        let chars: Vec<char> = query.chars().collect();
                        let mut p = cursor_pos.min(chars.len());
                        // skip delimiters backward (mirror of Ctrl+Right's forward skip)
                        while p > 0 && (chars[p - 1] == ' ' || chars[p - 1] == ':' || chars[p - 1] == '\\' || chars[p - 1] == '"') {
                            p -= 1;
                        }
                        // skip word chars backward
                        while p > 0
                            && chars[p - 1] != ' '
                            && chars[p - 1] != ':'
                            && chars[p - 1] != '\\'
                            && chars[p - 1] != '"'
                        {
                            p -= 1;
                        }
                        cursor_pos = p;
                    }
                }
                KeyEvent {
                    code: KeyCode::Right,
                    modifiers,
                    ..
                } if modifiers.contains(KeyModifiers::CONTROL) => {
                    let chars: Vec<char> = query.chars().collect();
                    let len = chars.len();
                    if cursor_pos < len {
                        let mut p = cursor_pos;
                        // skip word chars
                        while p < len
                            && chars[p] != ' '
                            && chars[p] != ':'
                            && chars[p] != '\\'
                            && chars[p] != '"'
                        {
                            p += 1;
                        }
                        // skip trailing spaces/delimiters
                        while p < len && (chars[p] == ' ' || chars[p] == ':' || chars[p] == '\\' || chars[p] == '"') {
                            p += 1;
                        }
                        cursor_pos = p;
                    }
                }

                // navigate results
                KeyEvent {
                    code: KeyCode::Up, ..
                } => {
                    if selected > 0 {
                        selected -= 1;
                    }
                }
                KeyEvent {
                    code: KeyCode::Down,
                    ..
                } => {
                    if selected + 1 < result_count {
                        selected += 1;
                    }
                }
                KeyEvent {
                    code: KeyCode::PageUp,
                    ..
                } => {
                    if selected >= result_rows {
                        selected -= result_rows;
                    } else {
                        selected = 0;
                    }
                }
                KeyEvent {
                    code: KeyCode::PageDown,
                    ..
                } => {
                    selected += result_rows;
                    if selected >= result_count {
                        selected = result_count.saturating_sub(1);
                    }
                }
                KeyEvent {
                    code: KeyCode::Home,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    selected = 0;
                }
                KeyEvent {
                    code: KeyCode::End,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    selected = result_count.saturating_sub(1);
                }

                // move cursor to beginning/end of search line (Ctrl+Home / Ctrl+End)
                KeyEvent {
                    code: KeyCode::Home,
                    modifiers,
                    ..
                } if modifiers.contains(KeyModifiers::CONTROL) => {
                    cursor_pos = 0;
                }
                KeyEvent {
                    code: KeyCode::End,
                    modifiers,
                    ..
                } if modifiers.contains(KeyModifiers::CONTROL) => {
                    cursor_pos = query.chars().count();
                }

                // show file in folder (Enter)
                KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    // if query is a content search with no results yet, trigger it
                    let is_content = query.split_whitespace().any(|p| {
                        p == "grep:" || p == "content:"
                            || p.starts_with("grep:") || p.starts_with("content:")
                    });
                    if is_content && result_count == 0 {
                        force_search = true;
                    } else {
                        let sel_path = if use_content_results {
                            content_results.get(selected).map(|(e, _, _, _)| e.path.as_str())
                        } else {
                            results.get(selected).map(|h| h.entry.path.as_str())
                        };
                        if let Some(p) = sel_path {
                            let path_owned = p.to_string();
                            #[cfg(target_os = "windows")]
                            {
                                let _ = std::process::Command::new("explorer")
                                    .arg("/select,")
                                    .arg(&path_owned)
                                    .spawn();
                            }
                            #[cfg(target_os = "linux")]
                            {
                                // xdg-open on the parent folder (no select equivalent)
                                let folder = Path::new(&path_owned)
                                    .parent()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                let _ = std::process::Command::new("xdg-open")
                                    .arg(&folder)
                                    .spawn();
                            }
                            #[cfg(target_os = "macos")]
                            {
                                let _ = std::process::Command::new("open")
                                    .arg("-R")
                                    .arg(&path_owned)
                                    .spawn();
                            }
                        }
                    }
                }
                _ => {}
            }
            if needs_search || force_search {
                // skip auto-search for content/grep queries — user presses Enter to trigger
                let is_content = query.split_whitespace().any(|p| {
                    p == "grep:" || p == "content:"
                        || p.starts_with("grep:") || p.starts_with("content:")
                });
                if is_content && !force_search {
                    results = Vec::new();
                    content_results = Vec::new();
                    use_content_results = false;
                    content_searched = false;
                } else if is_content && force_search {
                    // run content search in background thread with cancel support
                    let cancel = Arc::new(AtomicBool::new(false));
                    let cancel_clone = Arc::clone(&cancel);
                    let query_clone = query.clone();

                    // SAFETY: index lives for the duration of the search;
                    // we join the thread before touching index again.
                    let index_addr = index as *const FileIndex as usize;
                    let handle = std::thread::spawn(move || {
                        let idx = unsafe { &*(index_addr as *const FileIndex) };
                        idx.search(&query_clone, 1000, &cancel_clone)
                            .into_iter()
                            .map(|h| {
                                // copy data out so results don't borrow index
                                (
                                    h.entry.path.clone(),
                                    h.entry.path_lower.clone(),
                                    h.entry.name.clone(),
                                    h.entry.name_lower.clone(),
                                    h.entry.size,
                                    h.entry.is_dir,
                                    h.entry.extension.clone(),
                                    h.score,
                                    h.line_num,
                                    h.line_text,
                                )
                            })
                            .collect::<Vec<_>>()
                    });

                    let search_start = Instant::now();
                    let mut cancelled = false;
                    let mut dots = 0;

                    // poll for Esc while search runs
                    loop {
                        if handle.is_finished() {
                            break;
                        }
                        // show searching status
                        dots = (dots + 1) % 4;
                        let dot_str = ".".repeat(dots);
                        let elapsed_ms = search_start.elapsed().as_secs_f64() * 1000.0;
                        let status = format!(
                            "  Searching{:<4} ({:.0}ms) — press Esc to cancel",
                            dot_str, elapsed_ms
                        );
                        execute!(stdout, cursor::MoveTo(0, 0))?;
                        write!(stdout, "{}", fit(&status, w))?;
                        stdout.flush()?;

                        if event::poll(std::time::Duration::from_millis(200))? {
                            if let Event::Key(k) = event::read()? {
                                if k.kind == event::KeyEventKind::Press
                                    && k.code == KeyCode::Esc
                                {
                                    cancel.store(true, Ordering::Relaxed);
                                    cancelled = true;
                                    break;
                                }
                            }
                        }
                    }

                    let owned_results = handle.join().unwrap_or_default();

                    if cancelled {
                        // clear status, keep previous results
                        search_time = search_start.elapsed();
                    } else {
                        // store owned results as temporary entries for display
                        // rebuild results from owned data
                        content_results = owned_results
                            .into_iter()
                            .map(
                                |(path, path_lower, name, name_lower, size, is_dir, extension, score, line_num, line_text)| {
                                    (
                                        FileEntry {
                                            path,
                                            path_lower,
                                            name,
                                            name_lower,
                                            size,
                                            is_dir,
                                            extension,
                                        },
                                        score,
                                        line_num,
                                        line_text,
                                    )
                                },
                            )
                            .collect();
                        selected = 0;
                        scroll_offset = 0;
                        search_time = search_start.elapsed();
                        use_content_results = true;
                        content_searched = true;
                    }
                } else {
                    selected = 0;
                    scroll_offset = 0;
                    use_content_results = false;
                    content_searched = false;
                    let start = Instant::now();
                    results = if query.is_empty() {
                        Vec::new()
                    } else {
                        index.search(&query, 1000, &no_cancel)
                    };
                    search_time = start.elapsed();
                }
            }
        }
    }

    // restore terminal state
    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    Ok(())
}

fn main() {
    // cache lives in %LOCALAPPDATA%/turbofind/index.bin (Windows)
    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("turbofind");
    fs::create_dir_all(&cache_dir).ok();
    let cache_path = cache_dir.join("index.bin");

    // parse CLI args: flags and root directories
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("TurboFind - Fast file indexer and search");
        println!();
        println!("USAGE: turbofind [OPTIONS] [DIRECTORIES...]");
        println!();
        println!("OPTIONS:");
        println!("  --help, -h    Show this help and exit");
        println!("  --reindex     Force rebuild index (ignore cache)");
        println!("  --no-cache    Don't read or write cache file");
        println!();
        println!("SEARCH FILTERS (type in the TUI):");
        println!("  ext:rs           Filter by file extension");
        println!("  dir:             Show only directories");
        println!("  in:text          Restrict to paths containing 'text'");
        println!("  in:C:\\\\Dir        Prefix match on full path");
        println!("  regex:pat        Regex match on filenames");
        println!("  grep:term        Search file contents (plain text)");
        println!("  content:term     Same as grep:");
        println!("  grep:pat regex:  Regex search file contents");
        println!();
        println!("DOCUMENT SEARCH (use ext: or in: to target):");
        println!("  ext:pdf grep:term    PDF (text-based only, not scanned)");
        println!("  ext:docx grep:term   Word documents");
        println!("  ext:xlsx grep:term   Excel spreadsheets");
        println!("  ext:pptx grep:term   PowerPoint slides");
        println!("  ext:odt/ods/odp      LibreOffice documents");
        println!("  ext:rtf grep:term    Rich Text Format");
        println!("  in:file.docx grep:term  Search specific document via in:");
        println!();
        println!("KEYS:");
        println!("  Up/Down          Navigate results");
        println!("  PgUp/PgDown      Scroll by page");
        println!("  Home/End         Jump to first/last result");
        println!("  Tab              Complete in: with selected path (file or dir)");
        println!("  Ctrl+Left/Right  Jump cursor by word");
        println!("  Ctrl+Home/End    Jump cursor to start/end");
        println!("  Ctrl+U           Clear search line");
        println!("  Ctrl+K           Delete to end of line");
        println!("  Ctrl+Bksp        Delete word before cursor");
        println!("  Ctrl+Del         Delete word after cursor");
        println!("  Enter            Show file in folder");
        println!("  Ctrl+O           Open file directly");
        println!("  Ctrl+R           Rebuild index");
        println!("  Ctrl+N           Add path to index");
        println!("  Ctrl+B           Run benchmark");
        println!("  F1               Toggle help overlay");
        println!("  Esc              Quit");
        return;
    }

    let no_cache = args.iter().any(|a| a == "--no-cache");
    let force_reindex = args.iter().any(|a| a == "--reindex");
    let root_args: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(|a| {
            // resolve relative paths (including ".") to absolute
            let p = PathBuf::from(a);
            if p.is_relative() {
                std::env::current_dir()
                    .map(|cwd| cwd.join(&p))
                    .unwrap_or(p)
            } else {
                p
            }
            .to_string_lossy()
            .to_string()
        })
        .collect();

    let default_roots = vec![
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .to_string(),
    ];

    let use_roots: Vec<String> = if !root_args.is_empty() {
        root_args
    } else {
        default_roots
    };
    let roots: Vec<&str> = use_roots.iter().map(|s| s.as_str()).collect();

    // load cached index if fresh (<1 hour), unless overridden by flags
    let mut index = if no_cache || force_reindex {
        if no_cache {
            println!("  --no-cache: building fresh index (won't save)");
        } else {
            println!("  --reindex: rebuilding index...");
        }
        let idx = FileIndex::build(&roots);
        if !no_cache {
            idx.save(&cache_path).ok();
        }
        idx
    } else if cache_path.exists() {
        match FileIndex::load(&cache_path) {
            Ok(cached) => {
                let age = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    - cached.indexed_at;
                // invalidate cache if roots changed or stale
                let roots_match = {
                    let mut cached_sorted = cached.roots.clone();
                    let mut current_sorted: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
                    cached_sorted.sort();
                    current_sorted.sort();
                    cached_sorted == current_sorted
                };
                if !roots_match {
                    println!("  Cache roots differ, rebuilding for {:?}...", roots);
                    let idx = FileIndex::build(&roots);
                    idx.save(&cache_path).ok();
                    idx
                } else if age > 3600 {
                    println!("  Cache stale, rebuilding...");
                    let idx = FileIndex::build(&roots);
                    idx.save(&cache_path).ok();
                    idx
                } else {
                    println!("  Using cached index ({}s old)", age);
                    cached
                }
            }
            Err(_) => {
                let idx = FileIndex::build(&roots);
                idx.save(&cache_path).ok();
                idx
            }
        }
    } else {
        let idx = FileIndex::build(&roots);
        idx.save(&cache_path).ok();
        idx
    };

    if let Err(e) = run_tui(&mut index, &cache_path, &roots) {
        eprintln!("TUI error: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    // --- Unit tests for pure helper functions ---

    #[test]
    fn test_is_absolute_filter() {
        assert!(is_absolute_filter("C:\\Users"));
        assert!(is_absolute_filter("D:\\"));
        assert!(is_absolute_filter("\\\\server\\share"));
        assert!(!is_absolute_filter("src"));
        assert!(!is_absolute_filter("downloads\\foo"));
        assert!(!is_absolute_filter(""));
    }

    #[test]
    fn test_path_matches_absolute() {
        // absolute path = prefix match
        assert!(path_matches("c:\\users\\docs\\file.txt", "c:\\users"));
        assert!(path_matches("c:\\users\\docs\\file.txt", "c:\\users\\docs"));
        assert!(!path_matches("c:\\users\\docs\\file.txt", "d:\\users"));
    }

    #[test]
    fn test_path_matches_relative() {
        // relative/fragment = substring contains
        assert!(path_matches("c:\\users\\docs\\file.txt", "docs"));
        assert!(path_matches("c:\\users\\docs\\file.txt", "file"));
        assert!(!path_matches("c:\\users\\docs\\file.txt", "photos"));
    }

    #[test]
    fn test_is_binary_extension() {
        assert!(is_binary_extension("exe"));
        assert!(is_binary_extension("jpg"));
        assert!(is_binary_extension("pdf"));
        assert!(is_binary_extension("zip"));
        assert!(!is_binary_extension("rs"));
        assert!(!is_binary_extension("txt"));
        assert!(!is_binary_extension("py"));
        assert!(!is_binary_extension(""));
    }

    #[test]
    fn test_fit_truncates() {
        let s = "hello world";
        let result = fit(s, 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_fit_pads() {
        let s = "hi";
        let result = fit(s, 5);
        assert_eq!(result, "hi   ");
    }

    #[test]
    fn test_fit_exact() {
        let s = "exact";
        let result = fit(s, 5);
        assert_eq!(result, "exact");
    }

    #[test]
    fn test_fit_unicode() {
        // multi-byte chars should not panic
        let s = "héllo wörld";
        let result = fit(s, 5);
        assert_eq!(result.chars().count(), 5);
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500B");
        assert_eq!(format_size(1024), "1K");
        assert_eq!(format_size(2048), "2K");
        assert_eq!(format_size(1048576), "1.0M");
        assert_eq!(format_size(1073741824), "1.0G");
    }

    // --- Integration tests with temp directories ---

    /// Create a temp dir with some test files and build an index from it
    fn build_test_index() -> (TempDir, FileIndex) {
        let dir = TempDir::new().unwrap();
        let base = dir.path();

        // create test files
        fs::write(base.join("readme.txt"), "Hello world").unwrap();
        fs::write(base.join("config.rs"), "fn main() { println!(\"hello\"); }").unwrap();
        fs::write(base.join("data.json"), "{\"key\": \"value\"}").unwrap();
        fs::write(base.join("notes.txt"), "some notes here").unwrap();
        fs::write(base.join("test.py"), "import os\nprint('hello')").unwrap();

        // create a subdirectory with files
        let sub = base.join("subdir");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("nested.rs"), "// nested rust file\nfn nested() {}").unwrap();
        fs::write(sub.join("deep.txt"), "deep content with TODO marker").unwrap();

        let root = base.to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        (dir, index)
    }

    #[test]
    fn test_index_finds_all_files() {
        let (_dir, index) = build_test_index();
        // subdir + 7 files = 8 entries (root dir itself may not be included)
        assert!(index.entries.len() >= 8);
    }

    #[test]
    fn test_index_extension_map() {
        let (_dir, index) = build_test_index();
        assert!(index.ext_map.contains_key("txt"));
        assert!(index.ext_map.contains_key("rs"));
        assert!(index.ext_map.contains_key("json"));
        assert!(index.ext_map.contains_key("py"));
        assert_eq!(index.ext_map["txt"].len(), 3); // readme.txt, notes.txt, deep.txt
        assert_eq!(index.ext_map["rs"].len(), 2);  // config.rs, nested.rs
    }

    #[test]
    fn test_fuzzy_search() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        let results = index.search("readme", 100, &cancel);
        assert!(!results.is_empty());
        assert_eq!(results[0].entry.name, "readme.txt");
    }

    #[test]
    fn test_fuzzy_search_empty_query() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        // empty query with no filters returns all entries
        let results = index.search("", 100, &cancel);
        assert_eq!(results.len(), index.entries.len());
    }

    #[test]
    fn test_extension_filter() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        let results = index.search("ext:rs", 100, &cancel);
        assert_eq!(results.len(), 2);
        for hit in &results {
            assert_eq!(hit.entry.extension, "rs");
        }
    }

    #[test]
    fn test_dir_filter() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        let results = index.search("dir:", 100, &cancel);
        assert!(!results.is_empty());
        for hit in &results {
            assert!(hit.entry.is_dir);
        }
    }

    #[test]
    fn test_regex_search() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        let results = index.search("regex:\\.txt$", 100, &cancel);
        assert_eq!(results.len(), 3);
        for hit in &results {
            assert!(hit.entry.name.ends_with(".txt"));
        }
    }

    #[test]
    fn test_regex_invalid_pattern() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        let results = index.search("regex:[invalid", 100, &cancel);
        assert!(results.is_empty()); // invalid regex returns empty, no panic
    }

    #[test]
    fn test_content_search() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        let results = index.search("grep:TODO", 100, &cancel);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.name, "deep.txt");
        assert!(results[0].line_num > 0);
        assert!(results[0].line_text.contains("TODO"));
    }

    #[test]
    fn test_content_search_no_match() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        let results = index.search("grep:NONEXISTENT_STRING_XYZ", 100, &cancel);
        assert!(results.is_empty());
    }

    #[test]
    fn test_content_search_with_ext_filter() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        // "hello" appears in config.rs and test.py
        let results = index.search("grep:hello ext:rs", 100, &cancel);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.extension, "rs");
    }

    #[test]
    fn test_content_search_cancel() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(true); // pre-cancelled
        let results = index.search("grep:hello", 100, &cancel);
        assert!(results.is_empty());
    }

    #[test]
    fn test_path_filter() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        let results = index.search("in:subdir", 100, &cancel);
        assert!(!results.is_empty());
        for hit in &results {
            assert!(hit.entry.path_lower.contains("subdir"));
        }
    }

    #[test]
    fn test_combined_filters() {
        let (_dir, index) = build_test_index();
        let cancel = AtomicBool::new(false);
        // ext:txt in subdir should only match deep.txt
        let results = index.search("ext:txt in:subdir", 100, &cancel);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.name, "deep.txt");
    }

    #[test]
    fn test_index_save_load_roundtrip() {
        let (dir, index) = build_test_index();
        let cache_path = dir.path().join("test_index.bin");
        index.save(&cache_path).unwrap();
        let loaded = FileIndex::load(&cache_path).unwrap();
        assert_eq!(loaded.entries.len(), index.entries.len());
        assert_eq!(loaded.roots, index.roots);
        assert_eq!(loaded.ext_map.len(), index.ext_map.len());
    }

    #[test]
    fn test_index_merge() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();

        let dir_a = base.join("a");
        let dir_b = base.join("b");
        fs::create_dir(&dir_a).unwrap();
        fs::create_dir(&dir_b).unwrap();
        fs::write(dir_a.join("file_a.txt"), "aaa").unwrap();
        fs::write(dir_b.join("file_b.txt"), "bbb").unwrap();

        let root_a = dir_a.to_string_lossy().to_string();
        let root_b = dir_b.to_string_lossy().to_string();
        let mut index_a = FileIndex::build(&[root_a.as_str()]);
        let index_b = FileIndex::build(&[root_b.as_str()]);

        let count_a = index_a.entries.len();
        let count_b = index_b.entries.len();
        index_a.merge(index_b);

        assert_eq!(index_a.entries.len(), count_a + count_b);
        assert_eq!(index_a.roots.len(), 2);
    }

    #[test]
    fn test_content_search_strips_control_chars() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("escape.txt"),
            "normal line\n\x1b[47mwhite background\x1b[0m\n",
        )
        .unwrap();

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let cancel = AtomicBool::new(false);
        let results = index.search("grep:white", 100, &cancel);
        assert_eq!(results.len(), 1);
        // verify no control chars in line_text
        assert!(!results[0].line_text.chars().any(|c| c.is_control()));
    }

    #[test]
    fn test_sanitize_terminal_text_removes_line_separators() {
        let s = "ok\u{2028}next\u{2029}last\x1b[31m";
        let out = sanitize_terminal_text(s);
        assert_eq!(out, "oknextlast[31m");
        assert!(!out.chars().any(|c| c.is_control()));
    }

    #[test]
    fn test_sanitize_terminal_text_removes_bidi_and_zero_width() {
        let s = "ab\u{202E}cd\u{200B}ef\u{2066}gh\u{2069}";
        let out = sanitize_terminal_text(s);
        assert_eq!(out, "abcdefgh");
    }

    #[test]
    fn test_sanitize_terminal_text_preserves_non_english() {
        let s = "cafe\u{0301} 你好 Привет العربية";
        let out = sanitize_terminal_text(s);
        assert_eq!(out, s);
    }

    #[test]
    fn test_content_search_strips_unicode_line_separators() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("u2028.txt"), "prefix\u{2028}needle").unwrap();

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let cancel = AtomicBool::new(false);
        let results = index.search("grep:needle", 100, &cancel);

        assert_eq!(results.len(), 1);
        assert!(!results[0].line_text.contains('\u{2028}'));
        assert!(results[0].line_text.contains("needle"));
    }

    #[test]
    fn test_hidden_files_skipped() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("visible.txt"), "yes").unwrap();
        fs::write(dir.path().join(".hidden"), "no").unwrap();

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let names: Vec<&str> = index.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"visible.txt"));
        assert!(!names.contains(&".hidden"));
    }

    #[test]
    fn test_max_results_limit() {
        let dir = TempDir::new().unwrap();
        for i in 0..50 {
            fs::write(dir.path().join(format!("file_{}.txt", i)), "content").unwrap();
        }

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let cancel = AtomicBool::new(false);
        let results = index.search("file", 10, &cancel);
        assert!(results.len() <= 10);
    }

    // --- Tests for new helper functions ---

    #[test]
    fn test_visible_len() {
        assert_eq!(visible_len("hello"), 5);
        assert_eq!(visible_len("\x1b[33mhello\x1b[0m"), 5);
        assert_eq!(visible_len("\x1b[1;31mhi\x1b[0m there"), 8);
        assert_eq!(visible_len(""), 0);
        assert_eq!(visible_len("\x1b[7m\x1b[0m"), 0);
    }

    #[test]
    fn test_fit_styled_truncates() {
        let s = "\x1b[33mhello world\x1b[0m";
        let result = fit_styled(s, 5);
        assert_eq!(visible_len(&result), 5);
        assert!(result.contains("\x1b[0m")); // has reset
    }

    #[test]
    fn test_fit_styled_pads() {
        let s = "\x1b[33mhi\x1b[0m";
        let result = fit_styled(s, 10);
        assert_eq!(visible_len(&result), 10);
    }

    #[test]
    fn test_highlight_term_found() {
        let result = highlight_term("Hello World", "world");
        assert!(result.contains("\x1b[1;31m"));
        assert!(result.contains("World"));
        assert!(result.contains("\x1b[0m"));
    }

    #[test]
    fn test_highlight_term_not_found() {
        let result = highlight_term("Hello World", "xyz");
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_highlight_term_empty() {
        let result = highlight_term("Hello", "");
        assert_eq!(result, "Hello");
    }

    #[test]
    fn test_extract_search_term_grep() {
        assert_eq!(extract_search_term("grep:TODO"), "TODO");
        assert_eq!(extract_search_term("content:hello"), "hello");
        assert_eq!(extract_search_term("grep:foo ext:rs"), "foo");
    }

    #[test]
    fn test_extract_search_term_plain() {
        assert_eq!(extract_search_term("config"), "config");
        assert_eq!(extract_search_term("ext:rs main"), "main");
    }

    // --- Tests for quote-aware query tokenizer ---

    #[test]
    fn test_split_query_simple() {
        let parts = split_query("ext:rs config");
        assert_eq!(parts, vec!["ext:rs", "config"]);
    }

    #[test]
    fn test_split_query_quoted_path() {
        let parts = split_query("in:\"C:\\Path With Spaces\\file.docx\" grep:hello");
        assert_eq!(parts, vec!["in:C:\\Path With Spaces\\file.docx", "grep:hello"]);
    }

    #[test]
    fn test_split_query_no_quotes() {
        let parts = split_query("in:C:\\NoSpaces\\file.rs grep:test");
        assert_eq!(parts, vec!["in:C:\\NoSpaces\\file.rs", "grep:test"]);
    }

    #[test]
    fn test_split_query_empty() {
        let parts = split_query("");
        assert!(parts.is_empty());
    }

    // --- Tests for multiple content matches ---

    #[test]
    fn test_content_search_multiple_matches() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("multi.txt"),
            "line one TODO\nline two\nline three TODO\nline four TODO\n",
        )
        .unwrap();

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let cancel = AtomicBool::new(false);
        let results = index.search("grep:TODO", 100, &cancel);
        // should find 3 matches (lines 1, 3, 4) instead of just 1
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].line_num, 1);
        assert_eq!(results[1].line_num, 3);
        assert_eq!(results[2].line_num, 4);
    }

    #[test]
    fn test_content_search_max_per_file() {
        let dir = TempDir::new().unwrap();
        // Create two files with more than 5 matches each — cap applies per file
        let content: String = (0..10).map(|i| format!("line {} MATCH\n", i)).collect();
        fs::write(dir.path().join("many1.txt"), &content).unwrap();
        fs::write(dir.path().join("many2.txt"), &content).unwrap();

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let cancel = AtomicBool::new(false);
        let results = index.search("grep:MATCH", 100, &cancel);
        // capped at 5 per file × 2 files = 10
        assert_eq!(results.len(), 10);
    }

    #[test]
    fn test_content_search_single_file_no_cap() {
        let dir = TempDir::new().unwrap();
        // Single file: all matches returned (no per-file cap)
        let content: String = (0..10).map(|i| format!("line {} MATCH\n", i)).collect();
        fs::write(dir.path().join("many.txt"), &content).unwrap();

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let cancel = AtomicBool::new(false);
        let results = index.search("grep:MATCH", 100, &cancel);
        assert_eq!(results.len(), 10);
    }

    // --- Tests for document extraction ---

    #[test]
    fn test_is_document_extension() {
        assert!(is_document_extension("pdf"));
        assert!(is_document_extension("docx"));
        assert!(is_document_extension("xlsx"));
        assert!(is_document_extension("pptx"));
        assert!(is_document_extension("odt"));
        assert!(is_document_extension("ods"));
        assert!(is_document_extension("odp"));
        assert!(is_document_extension("rtf"));
        assert!(!is_document_extension("doc")); // legacy, not supported
        assert!(!is_document_extension("txt"));
        assert!(!is_document_extension("rs"));
    }

    #[test]
    fn test_strip_xml_tags() {
        let xml = "<w:p><w:r><w:t>Hello World</w:t></w:r></w:p>";
        let result = strip_xml_tags(xml);
        assert!(result.contains("Hello World"));
        assert!(!result.contains("<"));
        assert!(!result.contains(">"));
    }

    #[test]
    fn test_strip_xml_entities() {
        let xml = "<t>A &amp; B &lt; C</t>";
        let result = strip_xml_tags(xml);
        assert!(result.contains("A & B < C"));
    }

    #[test]
    fn test_strip_xml_paragraph_breaks() {
        let xml = "<w:p><w:t>Line 1</w:t></w:p><w:p><w:t>Line 2</w:t></w:p>";
        let result = strip_xml_tags(xml);
        let lines: Vec<&str> = result.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_extract_rtf_basic() {
        let dir = TempDir::new().unwrap();
        let rtf_path = dir.path().join("test.rtf");
        fs::write(&rtf_path, r"{\rtf1\ansi Hello World\par Second line}").unwrap();
        let result = extract_rtf_text(rtf_path.to_str().unwrap());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("Hello World"));
        assert!(text.contains("Second line"));
    }

    #[test]
    fn test_extract_rtf_hex_escape() {
        let dir = TempDir::new().unwrap();
        let rtf_path = dir.path().join("test.rtf");
        fs::write(&rtf_path, r"{\rtf1 caf\'e9}").unwrap();
        let result = extract_rtf_text(rtf_path.to_str().unwrap());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("caf\u{e9}")); // café
    }

    #[test]
    fn test_extract_document_unsupported() {
        assert!(extract_document_text("nofile.xyz", "xyz").is_none());
    }

    #[test]
    fn test_content_search_rtf() {
        let dir = TempDir::new().unwrap();
        let rtf_path = dir.path().join("doc.rtf");
        fs::write(&rtf_path, r"{\rtf1\ansi Important TODO item here\par Done}").unwrap();

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let cancel = AtomicBool::new(false);
        // RTF is in is_binary_extension, so broad grep won't hit it
        // but ext:rtf grep:TODO should find it
        let results = index.search("ext:rtf grep:TODO", 100, &cancel);
        assert_eq!(results.len(), 1);
        assert!(results[0].line_text.contains("TODO"));
    }

    #[test]
    fn test_docx_extraction_via_zip() {
        use std::io::Write as IoWrite;
        let dir = TempDir::new().unwrap();
        let docx_path = dir.path().join("test.docx");

        // Create a minimal DOCX (ZIP with word/document.xml)
        let file = fs::File::create(&docx_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip_writer.start_file("word/document.xml", options).unwrap();
        IoWrite::write_all(
            &mut zip_writer,
            b"<w:document><w:body><w:p><w:r><w:t>Budget Report 2026</w:t></w:r></w:p></w:body></w:document>",
        ).unwrap();
        zip_writer.finish().unwrap();

        let root = dir.path().to_string_lossy().to_string();
        let index = FileIndex::build(&[root.as_str()]);
        let cancel = AtomicBool::new(false);
        let results = index.search("ext:docx grep:Budget", 100, &cancel);
        assert_eq!(results.len(), 1);
        assert!(results[0].line_text.contains("Budget"));
    }
}
