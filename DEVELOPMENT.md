# TurboFind — Development Notes

## Architecture

Single-binary Rust TUI application. All code lives in `src/main.rs` (~2675 lines).

### Core Data Structures

- **`FileEntry`** — Single indexed file/directory: path, path_lower, name, name_lower (pre-computed), size, is_dir, extension. Serialized with serde.
- **`FileIndex`** — In-memory search index: `Vec<FileEntry>`, `ext_map: HashMap<String, Vec<usize>>` for O(1) extension filtering, roots list, indexed_at timestamp. Serialized to binary cache via postcard.
- **`SearchHit`** — Search result reference: `&FileEntry` + score + optional line_num/line_text for content matches.

### Three-Phase Design

1. **Indexing** — Parallel filesystem crawl via rayon + walkdir. Skips hidden (`.`) and system (`$`) entries. `follow_links(false)` to prevent symlink loops.
2. **Caching** — Postcard serialization to `~/.cache/turbofind/index.bin`. 1-hour TTL. Auto-invalidated when roots change.
3. **Search** — Dispatched by filter type:
   - Default: nucleo fuzzy matching with parallel scoring (rayon `map_init`), exact substring boost (+10000)
   - `regex:`/`re:` — regex crate, case-insensitive filename matching
   - `grep:`/`content:` — parallel file content search, 10MB limit, binary files skipped, up to 5 matches per file (unlimited for single-file via `in:`), sorted by path then line number
   - Document content search: PDF, DOCX, XLSX, PPTX, ODT, ODS, ODP, RTF — requires `ext:` or `in:` filter to avoid broad-search slowdowns. When `in:` points to a supported document type, `ext:` is not required.
   - Filters composable: `ext:`, `dir:`, `in:` (substring or prefix match)

### TUI Rendering

- crossterm alternate screen with single-buffer frame writes (zero flicker)
- `fit()` function pads/truncates every line to terminal width; `fit_styled()` handles ANSI escape codes
- ANSI color highlighting: yellow filenames, bold red matched terms, cyan line numbers, dimmed tags/paths, reverse video selection
- Scroll offset tracking keeps selected item visible
- Cursor position tracked separately from query length for mid-string editing
- Preview pane (bottom 40% of screen) shows file content centered on match line with context
- `visible_len()` counts visible chars excluding ANSI escapes for correct width calculations

## Dependencies

| Crate | Purpose |
|---|---|
| `rayon` | Parallel iteration (crawling, search scoring, content search) |
| `walkdir` | Recursive directory traversal |
| `nucleo` | Fuzzy matching (same engine as Helix editor) |
| `regex` | Regex filename and content search |
| `crossterm` | Terminal TUI (raw mode, alternate screen, key events) |
| `postcard` | Binary serialization for index cache |
| `zip` | Reading ZIP-based document formats (DOCX, XLSX, PPTX, ODF) |
| `pdf-extract` | PDF text extraction |
| `serde` | Serialization framework |
| `dirs` | Platform cache directory lookup |

## Features

### Search Filters (composable, typed in TUI)

| Filter | Behavior |
|---|---|
| **Narrowing** | |
| `dir:` / `folder:` | Directories only |
| `ext:rs` | Extension filter (O(1) via HashMap) |
| `in:text` | Substring path match |
| `in:C:\path` | Prefix path match (absolute paths) |
| **Search modes** | |
| *(default)* | Fuzzy filename match (nucleo) |
| `regex:pattern` / `re:pattern` | Regex filename match |
| `grep:term` / `content:term` | File content search (plain text) |
| `grep:pat regex:` | Regex content search |
| **Document search** | |
| `ext:pdf grep:term` | Search inside PDF files |
| `ext:docx grep:term` | Search inside DOCX files |
| `ext:xlsx grep:term` | Search inside XLSX files |
| `in:file.docx grep:term` | Search specific document (no `ext:` needed) |

### Keybindings

| Key | Action |
|---|---|
| Typing | Fuzzy search (insert at cursor position) |
| Left/Right | Move cursor in search query |
| Backspace/Delete | Delete before/at cursor |
| Up/Down | Navigate results |
| PgUp/PgDown | Scroll results by page |
| Home/End | Jump to first/last result |
| Tab | Complete `in:` with selected path (file or dir) |
| Ctrl+Left/Right | Jump cursor by word |
| Ctrl+Home/End | Jump cursor to start/end |
| Ctrl+U | Clear search line |
| Ctrl+K | Delete to end of line |
| Ctrl+Backspace | Delete word before cursor |
| Ctrl+Delete | Delete word after cursor |
| Enter | Show file in folder (`explorer /select,` on Windows) |
| Ctrl+O | Open file directly with default application |
| Ctrl+R | Rebuild index from scratch |
| Ctrl+N | Add new path to index (with filesystem Tab completion) |
| Ctrl+B | Run benchmark (100 iterations × 5 queries) |
| F1 | Toggle help overlay |
| Esc / Ctrl+C | Quit |

### CLI Flags

| Flag | Behavior |
|---|---|
| `--help` / `-h` | Print usage and exit |
| `--reindex` | Force rebuild, ignore cache |
| `--no-cache` | Don't read or write cache |
| `[DIRECTORIES...]` | Custom roots (default: current directory) |

## Security Considerations

### Addressed

- **Command injection**: File open uses `explorer.exe` directly, not `cmd /C start`, preventing shell metacharacter injection from crafted filenames.
- **UTF-8 panics**: `fit()` uses char-based truncation, not byte slicing, preventing panics on multi-byte characters.
- **Symlink following**: Disabled (`follow_links(false)`) to prevent symlink-based traversal attacks.

### Mitigated

- **Regex size limit**: `RegexBuilder` uses `.size_limit(1 << 20)` (1MB compiled limit) to prevent excessive memory use from complex patterns.
- **Cache allocation bomb**: 512MB file size guard before `postcard::from_bytes()` prevents a tampered `index.bin` from triggering unbounded allocation.
- **System clock panic**: `SystemTime::now().duration_since(UNIX_EPOCH)` uses `.unwrap_or_default()` instead of `.unwrap()` to avoid panics if the system clock is before epoch.

## Security Guidelines

When adding or modifying code, review for security and vulnerability issues. Common concerns for this type of application include command injection, unbounded allocation from untrusted input, unsafe string operations, and symlink traversal. See the Security Considerations section above for specific mitigations already in place.

## Code Efficiency Considerations

### Addressed

- **Pre-computed `path_lower`**: `FileEntry` stores a pre-lowercased path to avoid per-entry `to_lowercase()` allocation on every search with `in:` filter.
- **Scoped `search_lower`**: Only computed in the fuzzy search path where it's used, not for regex or content search dispatch.
- **No double-lowercasing**: `search()` normalizes `path_filter` once; `search_content()` no longer repeats it.
- **Removed unused `modified` field**: Was stored and serialized but never displayed or searched on.
- **Removed unused derives**: `Clone` and `Debug` on `FileEntry` were never used.
- **Extracted search-after-edit logic**: Backspace, Delete, Char, and Tab handlers set a `needs_search` flag; common reset/search/timing block runs once after the match.

## Code Efficiency Guidelines

When adding or modifying code, check for redundancy, unnecessary allocations, and unused fields or derives. Hot paths (search scoring, filtering) should avoid per-call allocations that can be pre-computed at index time. Duplicated logic across match arms should be extracted into shared code.

## Build

```bash
cargo build --release
# Binary: target/release/turbofind.exe

cargo test
# Runs 47 tests: unit + integration + color/highlight/multi-match/document extraction tests

cargo install --path .
# Installs to ~/.cargo/bin/
```

**Note:** Cargo.toml uses `edition = "2024"`.

## Future Ideas

- Configurable cache TTL / settings file
- Persistent roots configuration (e.g., `~/.config/turbofind/config.toml`)
- Consider `opener` crate for cross-platform file opening
- Manual preview pane scrolling (Ctrl+Up/Down)
- File type icons (nerd fonts)
- EPUB content search
