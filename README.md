# TurboFind

Fast file indexer and search for Windows. Indexes 1M+ files in ~12s, then fuzzy-searches them in ~15ms.

Built with Rust using rayon for parallel crawling, nucleo for fuzzy matching (same engine as Helix editor), and crossterm for the TUI.

## Benchmarks

Tested on 1,093,594 files (RTX 3060 / Ryzen system):

| Operation | Time |
|---|---|
| First index | ~12s (65K files/sec) |
| Cache reload | <1s |
| Fuzzy search (avg) | ~15ms (1.09M files) |

Measured over 100 iterations per query using release build. Run `Ctrl+B` inside the TUI to benchmark on your machine.

## Build

```bash
git clone https://github.com/fulong97/turbofind.git
cd turbofind
cargo build --release
cargo test
```

Binary ends up at `target/release/turbofind.exe`.

## Install

```bash
cargo install --path .
```

This copies `turbofind` to `~/.cargo/bin/` which is already in your PATH. Run `turbofind` from anywhere.

Or grab the pre-built `.exe` from [Releases](https://github.com/fulong97/turbofind/releases).

## Usage

```bash
# Index current directory
turbofind

# Index specific directories (relative paths and "." work)
turbofind C:\Projects D:\Documents
turbofind .

# Force rebuild index (ignore cache)
turbofind --reindex

# Run without reading or writing cache
turbofind --no-cache

# Combine flags with custom roots
turbofind --reindex C:\Projects
```

### Search filters

| Query | What it does |
|---|---|
| `budget` | Fuzzy match all files |
| `dir:` | Only directories |
| `ext:rs config` | Only .rs files matching "config" |
| `in:Projects config` | Files under paths containing "Projects" |
| `in:C:\Downloads` | Prefix match on full path |
| `in:src\utils ext:rs` | .rs files under paths with "src\utils" |
| `regex:\.test\.` | Regex match filenames |
| `regex: ext:rs ^lib` | Regex match .rs filenames starting with "lib" |
| `grep:TODO` | Search file contents for "TODO" |
| `grep:TODO regex:` | Regex search file contents |
| `content:fixme ext:py` | Search .py file contents for "fixme" |
| `content:fn\s+main ext:rs` | Regex content search with `regex:` flag |
| `ext:pdf invoice` | Search PDFs for "invoice" |
| `in:report.docx grep:budget` | Search specific document |

### Keys

| Key | Action |
|---|---|
| `Up/Down` | Navigate results |
| `PgUp/PgDown` | Scroll by page |
| `Home/End` | Jump to first/last result |
| `Tab` | Complete `in:` with selected path (file or dir) |
| `Ctrl+Left/Right` | Jump cursor by word |
| `Ctrl+Home/End` | Jump cursor to start/end |
| `Ctrl+U` | Clear search line |
| `Ctrl+K` | Delete to end of line |
| `Ctrl+Backspace` | Delete word before cursor |
| `Ctrl+Delete` | Delete word after cursor |
| `Enter` | Show file in folder |
| `Ctrl+O` | Open file directly |
| `Ctrl+R` | Rebuild index |
| `Ctrl+B` | Run benchmark |
| `F1` | Toggle help screen |
| `Esc` | Quit |

> **Tip:** Type `in:downloads` to see matching paths, navigate to the one you want, press `Tab` to lock in that exact path, then continue typing your search. Works for both files and directories — e.g. Tab onto a `.docx` file, then add `grep:term` to search its contents.

## How it works

First run crawls the filesystem in parallel using all CPU cores and builds an in-memory index. The index gets serialized to a binary cache file so subsequent launches load in under a second. Search uses the nucleo fuzzy matching algorithm (same engine as Helix editor) with parallel scoring via rayon. Extension filters use a pre-built HashMap for O(1) lookup.

**Content search** reads files from disk in parallel (via rayon), skipping binary files and anything over 10MB. Returns up to 5 matching lines per file (unlimited when a single file is targeted via `in:`). Combine with `ext:` or `in:` to narrow the search scope.

**Document search** supports PDF, DOCX, XLSX, PPTX, ODT/ODS/ODP, and RTF. Use `ext:docx grep:term` to search all documents of a type, or `in:file.docx grep:term` to search a specific document without needing `ext:`.

**Regex search** uses the `regex` crate for both filename and content matching. Case-insensitive by default.

## Dependencies

`rayon` `walkdir` `nucleo` `regex` `crossterm` `postcard` `serde` `dirs` `zip` `pdf-extract`

## License

MIT
