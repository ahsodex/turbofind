# TurboFind

Fast file indexer and search for Windows. Indexes 780K+ files in ~25s, then fuzzy-searches them in ~27ms.

Built with Rust using rayon for parallel crawling, skim for fuzzy matching, and crossterm for the TUI.

## Benchmarks

Tested on 781,490 files (RTX 3060 / Ryzen system):

| Operation | Time |
|---|---|
| First index | ~25s (31K files/sec) |
| Cache reload | <1s |
| Fuzzy search (avg) | ~27ms |

Measured over 100 iterations per query using release build.

## Build

```bash
git clone https://github.com/fulong97/turbofind.git
cd turbofind
cargo build --release
```

Binary ends up at `target/release/turbofind.exe`.

Or grab the pre-built `.exe` from [Releases](https://github.com/fulong97/turbofind/releases).

## Usage

```bash
# Index default paths
turbofind

# Index specific directories
turbofind C:\Projects D:\Documents
```

### Search filters

| Query | What it does |
|---|---|
| `budget` | Fuzzy match all files |
| `ext:rs config` | Only .rs files matching "config" |
| `ext:pdf invoice` | Only PDFs matching "invoice" |
| `dir:` | Only directories |

### Keys

| Key | Action |
|---|---|
| `Up/Down` | Navigate results |
| `Enter` | Open file |
| `Ctrl+O` | Open containing folder |
| `Esc` | Quit |

## How it works

First run crawls the filesystem in parallel using all CPU cores and builds an in-memory index. The index gets serialized to a binary cache file so subsequent launches load in under a second. Search uses the skim fuzzy matching algorithm. Extension filters use a pre-built HashMap for O(1) lookup.

## Dependencies

`rayon` `walkdir` `fuzzy-matcher` `crossterm` `bincode` `serde` `dirs`

## License

MIT
