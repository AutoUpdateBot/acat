# acat — ultra-lightweight syntax-highlighted `cat`

A flat `cat` replacement that streams true-color highlighted source to
the terminal scrollback (stdout) and exits immediately. No pager, no TUI,
no alternate screen.

- **Library-owned language knowledge:** 220 grammars via syntect
  (Sublime Text syntaxes) plus two-face extras (TypeScript, Docker, TOML,
  PowerShell, Zig, Vue, …) — **compiled into the binary**. No grammar
  files are read, downloaded, or touched at runtime, ever.
- **Multi-platform:** macOS Apple Silicon (ARM64), Linux x86_64, Linux ARM64.
  Every release binary is built **natively** on its own runner.
- **Aggressively optimized:** minimal feature flags, Oniguruma regex engine
  (measured smaller *and* 4x faster than the pure-Rust alternative),
  single built-in theme (`base16-ocean.dark`), full LTO, `opt-level="z"`,
  `panic="abort"`, stripped — ~1.9 MB total for all 220 languages.
- **Cat-compatible:** `-n` / `-b` / `-s`, multiple files, `-` for stdin,
  broken-pipe silence (`| head`), `NO_COLOR` + `--color=always|never|auto`.

## Detection: filename → extension → first line → plain

`Makefile`/`Dockerfile`/dotfiles are recognized by name, everything else by
extension (220 grammars), stdin and extensionless files additionally by
shebang-style first-line matching. Anything unrecognized falls back to
byte-exact plain `cat` output — and when color is off entirely, syntect is
never even loaded (pure `cat` throughput, zero startup cost).

## Install

### Homebrew

```sh
brew tap autoupdatebot/tap
brew install autoupdatebot/tap/acat
```

### From source

```sh
# Minimal toolchain + a C compiler (for the Oniguruma regex engine) is enough.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
cargo install --path .
# or: cargo build --release   # ~1.9 MB binary at target/release/acat
```

### Prebuilt tarballs

Attached to each GitHub Release (`acat-<tag>-<target>.tar.gz` + SHA-256):

- `aarch64-apple-darwin` (Apple Silicon Mac)
- `x86_64-unknown-linux-gnu` (Linux x86_64)
- `aarch64-unknown-linux-gnu` (Linux ARM64)

## Usage

```sh
acat main.rs
acat -n src/main.rs
printf '#!/bin/sh\necho hi\n' | acat --color=always -   # shebang detection
git diff | acat -l diff --color=always | less -R
acat --help
```

## Supported targets

| OS | Arch | Target triple | Built on |
|----|------|---------------|----------|
| macOS (Apple Silicon) | ARM64 | `aarch64-apple-darwin` | `macos-latest` |
| Linux | x86_64 (baseline, runs on any 64-bit Intel/AMD CPU) | `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| Linux | ARM64 (Pi, Graviton, Ampere…) | `aarch64-unknown-linux-gnu` | `ubuntu-26.04-arm` |

## Release

Push a tag; `release.yml` (in `.github/workflows`) builds natively on all
three runners, verifies each binary matches its target triple, packages
`acat-<tag>-<target>.tar.gz` with SHA-256 checksums, and publishes
everything to a single GitHub Release:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

## Credits

Highlighting grammars: Sublime Text syntax definitions via
[syntect](https://github.com/trishume/syntect) +
[two-face](https://codeberg.org/CosmicHarper/two-face).
Theme: `base16-ocean.dark`.
