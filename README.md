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
git clone https://github.com/ahsodex/turbofind.git
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

Or grab the pre-built `.exe` from [Releases](https://github.com/ahsodex/turbofind/releases).

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

# Show CLI help
turbofind --help

# Combine flags with custom roots
turbofind --reindex C:\Projects
```

### CLI flags

| Flag | What it does |
|---|---|
| `--help`, `-h` | Show usage and exit |
| `--reindex` | Force rebuild index (ignore cache) |
| `--no-cache` | Skip reading and writing cache |
| `[DIRECTORIES...]` | Index specific roots (default: current directory) |

The commands above are startup options only. Once TurboFind is running, you can use interactive query filters and keyboard controls in the TUI:

### Search filters

| Filter | What it does |
|---|---|
| `dir:` | Only directories |
| `ext:<extension>` | Only files with that extension |
| `in:<path>` | Limit search to paths containing that value (or exact-prefix full path) |
| `regex:<pattern>` | Regex match filenames |
| `regex:` | Regex mode flag (filename regex with remaining terms, or regex content search with `grep:` / `content:`) |
| `grep:<text-or-pattern>` | Search file contents |
| `content:<text-or-pattern>` | Alias for grep search |

#### Examples

| Query | What it does |
|---|---|
| `budget` | Fuzzy match all filenames |
| `ext:rs config` | Only .rs filenames matching "config" |
| `in:Projects config` | Fuzzy match filenames under paths containing "Projects" |
| `in:C:\Downloads` | Prefix match on full path |
| `regex:\.test\.` | Regex match filenames |
| `in:src\utils ext:rs` | .rs files under paths with "src\utils" |
| `ext:rs regex:^lib` | Regex match .rs filenames starting with "lib" |
| `grep:TODO` | Search file contents for "TODO" |
| `content:fixme ext:py` | Search .py file contents for "fixme" |
| `grep:fn\s+\w+ regex:` | Regex search file contents |
| `content:fn\s+main ext:rs regex:` | Regex search .rs file contents |
| `ext:pdf invoice` | Only .pdf filenames matching "invoice" |
| `in:report.docx grep:budget` | Search contents of a specific document |

> **Supported content types:** `grep:` / `content:` search plain-text files by default. Document extraction is supported for `pdf`, `docx`, `xlsx`, `pptx`, `odt`, `ods`, `odp`, and `rtf` only when targeted with `ext:` or `in:` scope.

> **Filter syntax:** Most filters carry their value directly after the colon (`ext:rs`, `grep:TODO`, `regex:\.test\.`). `dir:` and standalone `regex:` are mode flags — they take no value and change how the rest of the query is interpreted. For filename regex, use `regex:<pattern>` (or `regex:` followed by pattern terms). With `grep:` / `content:`, bare `regex:` enables regex content search.

### Keys

| Key | Action |
|---|---|
| `Up/Down` | In results: move selection up/down |
| `PgUp/PgDown` | In results: scroll by page |
| `Home/End` | In results: jump to first/last result |
| `Tab` | Fill the `in:` filter in the search line with the selected result |
| `Ctrl+Left/Right` | In search line: jump cursor by word |
| `Ctrl+Home/End` | In search line: jump cursor to start/end |
| `Ctrl+U` | In search line: clear entire input |
| `Ctrl+K` | In search line: delete to end of input |
| `Ctrl+Backspace` | In search line: delete word before cursor |
| `Ctrl+Delete` | In search line: delete word after cursor |
| `Enter` | Show selected result in file manager (Windows: `explorer /select,`) |
| `Ctrl+O` | Open selected file with default application |
| `Ctrl+R` | Rebuild index |
| `Ctrl+N` | Add new path to index |
| `Ctrl+B` | Run benchmark |
| `F1` | Toggle help screen |
| `Esc / Ctrl+C` | Quit |

> **Tip:** Type `in:downloads` to see matching paths, navigate to the one you want, press `Tab` to lock in that exact path, then continue typing your search. Works for both files and directories — e.g. Tab onto a `.docx` file, then add `grep:term` to search its contents.

## How it works

First run crawls the filesystem in parallel using all CPU cores and builds an in-memory index. The index gets serialized to a binary cache file so subsequent launches load in under a second. Search uses the nucleo fuzzy matching algorithm (same engine as Helix editor) with parallel scoring via rayon. Extension filters use a pre-built HashMap for O(1) lookup.

**Content search** reads files from disk in parallel (via rayon), skipping binary files and anything over 10MB. Returns up to 5 matching lines per file (unlimited when a single file is targeted via `in:`). Combine with `ext:` or `in:` to narrow the search scope.

**Document search** supports PDF, DOCX, XLSX, PPTX, ODT/ODS/ODP, and RTF. Use `ext:docx grep:term` to search all documents of a type, or `in:file.docx grep:term` to search a specific document without needing `ext:`.

**Regex search** uses the `regex` crate for both filename and content matching. Case-insensitive by default.

## Dependencies

`rayon` `walkdir` `nucleo` `regex` `crossterm` `postcard` `serde` `dirs` `zip` `pdf-extract`

Dev dependency: `tempfile` (tests)

## Origin

This repository is an adapted fork of [fulong97/turbofind](https://github.com/fulong97/turbofind).
This fork adds ongoing feature and UX updates while preserving upstream attribution under the MIT license.

## License

MIT
