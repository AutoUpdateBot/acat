//! acat — ultra-lightweight, syntax-highlighted `cat` replacement.
//!
//! Language knowledge is fully library-owned: syntect's Sublime Text
//! grammars plus two-face's extras (220 syntaxes total), compiled into
//! the binary as compressed dumps. No grammar files are read, downloaded,
//! or otherwise touched at runtime.
//!
//! Design:
//! * Assets load once per invocation behind a lock-free OnceLock,
//!   then output streams line-by-line to stdout with true-color ANSI
//!   escapes and the tool exits immediately (no pager, no TUI).
//! * Only foreground color (+bold/italic) is emitted: transparent
//!   background, scrollback-friendly, no per-line String allocation —
//!   spans are written straight into the 128 KiB buffered stdout.
//! * Unknown extensions / undetectable input fall back to byte-exact
//!   plain `cat` passthrough (zero escape overhead).

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, IsTerminal, Write};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Single built-in theme (one code path, no theme probing at runtime).
const THEME_NAME: &str = "base16-ocean.dark";
const RESET: &[u8] = b"\x1b[0m";

// ---------------------------------------------------------------------------
// Compiled-in assets (decompressed + regex-compiled exactly once)
// ---------------------------------------------------------------------------

fn assets() -> &'static (SyntaxSet, ThemeSet) {
    static A: OnceLock<(SyntaxSet, ThemeSet)> = OnceLock::new();
    A.get_or_init(|| {
        (
            two_face::syntax::extra_newlines(),
            ThemeSet::load_defaults(),
        )
    })
}

#[inline(always)]
fn syntax_set() -> &'static SyntaxSet {
    &assets().0
}

fn theme() -> &'static Theme {
    assets()
        .1
        .themes
        .get(THEME_NAME)
        .expect("built-in theme missing from syntect defaults")
}

/// The Plain Text syntax: highlighting with it is a no-op, so callers use
/// the byte-exact plain fast path instead (pointer-compared, no names).
fn plain_syntax(ss: &SyntaxSet) -> &SyntaxReference {
    ss.find_syntax_by_extension("txt")
        .or_else(|| ss.find_syntax_by_name("Plain Text"))
        .expect("Plain Text syntax missing from syntect defaults")
}

// ---------------------------------------------------------------------------
// Zero-alloc ANSI writer (foreground + bold/italic only, transparent bg)
// ---------------------------------------------------------------------------

#[inline(always)]
fn write_u8(mut out: impl Write, mut v: u8) -> io::Result<()> {
    let mut buf = [0u8; 3];
    let mut n = 0;
    if v >= 100 {
        buf[n] = b'0' + v / 100;
        v %= 100;
        n += 1;
    }
    if v >= 10 || n > 0 {
        buf[n] = b'0' + v / 10;
        v %= 10;
        n += 1;
    }
    buf[n] = b'0' + v;
    n += 1;
    out.write_all(&buf[..n])
}

fn write_ranges(out: &mut impl Write, ranges: &[(Style, &str)]) -> io::Result<()> {
    for (style, text) in ranges {
        out.write_all(b"\x1b[")?;
        let mut sep = false;
        if style.font_style.contains(FontStyle::BOLD) {
            out.write_all(b"1")?;
            sep = true;
        }
        if style.font_style.contains(FontStyle::ITALIC) {
            if sep {
                out.write_all(b";")?;
            }
            out.write_all(b"3")?;
            sep = true;
        }
        if style.font_style.contains(FontStyle::UNDERLINE) {
            if sep {
                out.write_all(b";")?;
            }
            out.write_all(b"4")?;
            sep = true;
        }
        if sep {
            out.write_all(b";")?;
        }
        let fg = style.foreground;
        out.write_all(b"38;2;")?;
        write_u8(&mut *out, fg.r)?;
        out.write_all(b";")?;
        write_u8(&mut *out, fg.g)?;
        out.write_all(b";")?;
        write_u8(&mut *out, fg.b)?;
        out.write_all(b"m")?;
        out.write_all(text.as_bytes())?;
        out.write_all(RESET)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Syntax resolution (all library-owned: extension, name, first line)
// ---------------------------------------------------------------------------

/// Extensionless convention files have no extension for the library to
/// match; map them to an equivalent one. This is detection plumbing only —
/// every highlighting rule still comes from the library.
fn filename_extension(base: &str) -> Option<&'static str> {
    match base {
        "Makefile" | "makefile" | "GNUmakefile" => Some("makefile"),
        "Dockerfile" | "dockerfile" => Some("dockerfile"),
        "Rakefile" | "Gemfile" | "Vagrantfile" | "Berksfile" => Some("rb"),
        ".gitignore" | ".dockerignore" => Some("gitignore"),
        ".vimrc" | "vimrc" => Some("vim"),
        ".bashrc" | ".zshrc" | ".bash_profile" | ".profile" => Some("sh"),
        _ => {
            if base.starts_with("Dockerfile.") {
                Some("dockerfile")
            } else {
                None
            }
        }
    }
}

/// Resolve which syntax highlights `path`, or None for byte-exact plain
/// passthrough. `first_line` enables shebang/`<?xml`-style detection for
/// stdin and extensionless files.
fn resolve_syntax<'a>(
    ss: &'a SyntaxSet,
    plain: &'a SyntaxReference,
    override_syn: Option<&'a SyntaxReference>,
    path: &str,
    first_line: Option<&str>,
) -> &'a SyntaxReference {
    if let Some(s) = override_syn {
        return s;
    }
    if path != "-" {
        let base = path.rsplit('/').next().unwrap_or(path);
        if let Some(mapped) = filename_extension(base) {
            if let Some(s) = ss.find_syntax_by_extension(mapped) {
                return s;
            }
        } else if let Some(ext) = base.rsplit('.').next() {
            if ext.len() != base.len() {
                if let Some(s) = ss.find_syntax_by_extension(ext) {
                    return s;
                }
            }
        }
    }
    if let Some(line) = first_line {
        if let Some(s) = ss.find_syntax_by_first_line(line) {
            return s;
        }
    }
    plain
}

// ---------------------------------------------------------------------------
// cat core: concatenate files / stdin with optional numbering + squeezing
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Opts {
    number_all: bool,
    number_nonblank: bool,
    squeeze_blank: bool,
    force_color: bool,
    no_color: bool,
    lang_override: Option<String>,
    files: Vec<String>,
}

struct CatState {
    lineno: u64,
    prev_blank: bool,
}

fn emit_line(
    out: &mut BufWriter<impl Write>,
    hl: Option<&mut HighlightLines>,
    raw: &[u8],
    cs: &mut CatState,
    opts: &Opts,
) -> io::Result<()> {
    let has_nl = raw.ends_with(b"\n");
    let mut content: &[u8] = raw;
    if has_nl {
        content = &content[..content.len() - 1];
        if content.ends_with(b"\r") {
            content = &content[..content.len() - 1];
        }
    }
    let blank = content
        .iter()
        .all(|&b| b == b' ' || b == b'\t' || b == b'\r');
    if opts.squeeze_blank && blank && cs.prev_blank {
        return Ok(());
    }
    cs.prev_blank = blank;

    if opts.number_all || (opts.number_nonblank && !blank) {
        cs.lineno += 1;
        write!(out, "{:>6}\t", cs.lineno)?;
    }

    match hl {
        // Blank lines stay byte-exact (no escape noise in scrollback).
        Some(h) if !content.is_empty() => {
            // The newlines-variant syntax set expects the trailing '\n'.
            // Non-UTF8 lines can't be highlighted: emit them raw so `cat`
            // never corrupts binary-ish input.
            match std::str::from_utf8(raw) {
                Ok(s) => {
                    let ranges = h.highlight_line(s, syntax_set()).map_err(syntect_err)?;
                    write_ranges(out, &ranges)?;
                }
                Err(_) => {
                    out.write_all(content)?;
                    if has_nl {
                        out.write_all(b"\n")?;
                    }
                }
            }
        }
        _ => {
            out.write_all(content)?;
            if has_nl {
                out.write_all(b"\n")?;
            }
        }
    }
    Ok(())
}

#[inline(always)]
fn syntect_err(e: syntect::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// Everything pump()/dump_one() need beyond the stream itself.
struct Job<'a> {
    ss: &'a SyntaxSet,
    plain: &'a SyntaxReference,
    override_syn: Option<&'a SyntaxReference>,
    color: bool,
    opts: &'a Opts,
}

fn pump<R: BufRead>(
    reader: &mut R,
    out: &mut BufWriter<impl Write>,
    job: &Job,
    path: &str,
    cs: &mut CatState,
) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(128 * 1024);
    // Prime with the first line: stdin / extensionless files get
    // first-line (shebang-style) detection from the library.
    if reader.read_until(b'\n', &mut buf)? == 0 {
        return Ok(());
    }
    let first_line = std::str::from_utf8(&buf).ok();
    let syntax = resolve_syntax(job.ss, job.plain, job.override_syn, path, first_line);
    // Plain syntax (or no color) takes the zero-overhead path: highlighting
    // it would only wrap lines in the default fg color for no benefit.
    let mut hl = if job.color && !std::ptr::eq(syntax, job.plain) {
        Some(HighlightLines::new(syntax, theme()))
    } else {
        None
    };
    loop {
        emit_line(out, hl.as_mut(), &buf, cs, job.opts)?;
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
    }
    Ok(())
}

fn color_enabled(opts: &Opts) -> bool {
    // Explicit CLI flags beat the environment; the environment beats
    // terminal auto-detection. --color=always overrides NO_COLOR/TERM.
    if opts.no_color {
        return false;
    }
    if opts.force_color {
        return true;
    }
    if env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
        return false;
    }
    io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

fn help() -> &'static str {
    "acat — ultra-lightweight syntax-highlighted cat\n\
     \n\
     Usage: acat [OPTION]... [FILE]...\n\
     Concatenate FILE(s) to standard output with true-color highlighting.\n\
     With no FILE, or when FILE is -, read standard input.\n\
     \n\
     Highlighting is library-owned: 220 syntect/two-face (Sublime Text)\n\
     grammars compiled into the binary — no grammar files are read or\n\
     downloaded, ever. Detection is by filename, extension, then by\n\
     first line (shebang etc.).\n\
     \n\
     Options:\n\
       -n, --number               number all output lines (like cat -n)\n\
       -b, --number-nonblank      number nonempty output lines (like cat -b)\n\
       -s, --squeeze-blank        suppress repeated empty lines (like cat -s)\n\
       -l, --language LANG        force language (extension or syntax name,\n\
           --language=LANG         e.g. -l py, -l typescript, -l Dockerfile)\n\
           --color=WHEN           always, never, or auto (default: auto)\n\
       -f, --force-color          alias for --color=always\n\
           --no-color             alias for --color=never (also honors NO_COLOR)\n\
       -h, --help                 display this help and exit\n\
       -V, --version              output version information and exit\n\
     \n\
     Colors are true-color ANSI (base16-ocean.dark) for Terminal/iTerm\n\
     scrollback; output streams to stdout and the tool exits immediately\n\
     (no pager, no TUI). Piped output auto-disables color unless\n\
     --color=always is given.\n\
     \n\
     Examples:\n\
       acat main.rs\n\
       acat -n src/main.rs\n\
       git diff | acat -l diff --color=always | less -R\n"
}

fn parse_args(argv: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    let mut end_opts = false;
    while i < argv.len() {
        let a = argv[i].as_str();
        if end_opts {
            o.files.push(argv[i].clone());
            i += 1;
            continue;
        }
        match a {
            "--" => end_opts = true,
            "--number" => o.number_all = true,
            "--number-nonblank" => o.number_nonblank = true,
            "--squeeze-blank" => o.squeeze_blank = true,
            "--force-color" => {
                o.force_color = true;
                o.no_color = false;
            }
            "--no-color" => {
                o.no_color = true;
                o.force_color = false;
            }
            "--help" => {
                print!("{}", help());
                std::process::exit(0);
            }
            "--version" => {
                println!("acat {VERSION}");
                std::process::exit(0);
            }
            _ if a.starts_with("--language=") => {
                o.lang_override = Some(a["--language=".len()..].to_string());
            }
            _ if a.starts_with("--color=") => match &a["--color=".len()..] {
                "always" => {
                    o.force_color = true;
                    o.no_color = false;
                }
                "never" => {
                    o.no_color = true;
                    o.force_color = false;
                }
                "auto" => {
                    o.force_color = false;
                    o.no_color = false;
                }
                other => {
                    return Err(format!(
                        "invalid --color value: {other} (use always|never|auto)"
                    ));
                }
            },
            "--language" | "--color" => {
                return Err(format!("{a} requires a value (use {a}=VALUE)"));
            }
            _ if a.starts_with("--") => return Err(format!("unknown option: {a}")),
            _ if a.starts_with('-') && a.len() > 1 => {
                // Combined short flags, e.g. -ns. -l consumes the next arg.
                let bytes = a.as_bytes();
                let mut k = 1;
                while k < bytes.len() {
                    match bytes[k] {
                        b'n' => o.number_all = true,
                        b'b' => o.number_nonblank = true,
                        b's' => o.squeeze_blank = true,
                        b'f' => {
                            o.force_color = true;
                            o.no_color = false;
                        }
                        b'h' => {
                            print!("{}", help());
                            std::process::exit(0);
                        }
                        b'V' => {
                            println!("acat {VERSION}");
                            std::process::exit(0);
                        }
                        b'l' => {
                            let rest = &a[k + 1..];
                            let val = if !rest.is_empty() {
                                rest.to_string()
                            } else {
                                i += 1;
                                argv.get(i).ok_or("-l requires a value")?.clone()
                            };
                            let v = val.strip_prefix('=').unwrap_or(&val);
                            o.lang_override = Some(v.to_string());
                            break;
                        }
                        other => return Err(format!("unknown option: -{}", other as char)),
                    }
                    k += 1;
                }
            }
            _ => o.files.push(argv[i].clone()),
        }
        i += 1;
    }
    if o.files.is_empty() {
        o.files.push("-".to_string());
    }
    Ok(o)
}

fn dump_one(
    path: &str,
    out: &mut BufWriter<impl Write>,
    job: &Job,
    cs: &mut CatState,
) -> io::Result<bool> {
    // Returns Ok(true) when the path was missing (caller sets exit code 1).
    if path == "-" {
        let stdin = io::stdin();
        let mut r = BufReader::with_capacity(128 * 1024, stdin.lock());
        pump(&mut r, out, job, path, cs)?;
        return Ok(false);
    }
    match File::open(path) {
        Ok(f) => {
            let mut r = BufReader::with_capacity(128 * 1024, f);
            pump(&mut r, out, job, path, cs)?;
            Ok(false)
        }
        Err(e) => {
            eprintln!("acat: {path}: {e}");
            Ok(true)
        }
    }
}

fn run() -> i32 {
    let argv: Vec<String> = env::args().skip(1).collect();
    let opts = match parse_args(&argv) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("acat: {e}\nTry 'acat --help' for more information.");
            return 1;
        }
    };
    let color = color_enabled(&opts);
    // syntect assets stay untouched unless highlighting (or an explicit
    // -l lookup, which must still be validated) can actually happen:
    // plain `cat`-style usage pays zero decompression/regex cost.
    let need_assets = color || opts.lang_override.is_some();
    let (ss, plain) = if need_assets {
        let ss = syntax_set();
        (Some(ss), Some(plain_syntax(ss)))
    } else {
        (None, None)
    };
    let override_syn = match &opts.lang_override {
        // Unwrap is safe: override implies need_assets.
        Some(name) => match ss
            .unwrap()
            .find_syntax_by_extension(name)
            .or_else(|| ss.unwrap().find_syntax_by_name(name))
        {
            Some(s) => Some(s),
            None => {
                eprintln!("acat: unknown language: {name}");
                return 1;
            }
        },
        None => None,
    };

    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(128 * 1024, stdout.lock());
    let mut cs = CatState {
        lineno: 0,
        prev_blank: false,
    };
    let mut exit = 0;
    // No assets, no highlighting possible: pure-cat fast path.
    if ss.is_none() {
        return run_plain(&opts, &mut out, &mut cs);
    }
    let (ss_ref, plain_ref) = (ss.unwrap(), plain.unwrap());
    let job = Job {
        ss: ss_ref,
        plain: plain_ref,
        override_syn,
        color,
        opts: &opts,
    };
    for path in &opts.files {
        match dump_one(path, &mut out, &job, &mut cs) {
            Ok(missing) => {
                if missing {
                    exit = 1;
                }
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::BrokenPipe {
                    // Downstream closed (e.g. `| head`): cat-like silent exit.
                    let _ = out.flush();
                    std::process::exit(0);
                }
                eprintln!("acat: {path}: {e}");
                exit = 1;
            }
        }
    }
    if let Err(e) = out.flush() {
        if e.kind() == io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("acat: write error: {e}");
        return 1;
    }
    exit
}

/// Asset-free fast path: no color possible and no -l to validate, so
/// syntect is never loaded — pure `cat` throughput.
fn run_plain(opts: &Opts, out: &mut BufWriter<impl Write>, cs: &mut CatState) -> i32 {
    let mut exit = 0;
    for path in &opts.files {
        let r: io::Result<()> = if path == "-" {
            let stdin = io::stdin();
            let mut r = BufReader::with_capacity(128 * 1024, stdin.lock());
            pump_plain(&mut r, out, opts, cs)
        } else {
            match File::open(path) {
                Ok(f) => {
                    let mut r = BufReader::with_capacity(128 * 1024, f);
                    pump_plain(&mut r, out, opts, cs)
                }
                Err(e) => {
                    eprintln!("acat: {path}: {e}");
                    exit = 1;
                    Ok(())
                }
            }
        };
        if let Err(e) = r {
            if e.kind() == io::ErrorKind::BrokenPipe {
                let _ = out.flush();
                std::process::exit(0);
            }
            eprintln!("acat: {path}: {e}");
            exit = 1;
        }
    }
    if let Err(e) = out.flush() {
        if e.kind() == io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("acat: write error: {e}");
        return 1;
    }
    exit
}

fn pump_plain<R: BufRead>(
    reader: &mut R,
    out: &mut BufWriter<impl Write>,
    opts: &Opts,
    cs: &mut CatState,
) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(128 * 1024);
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        emit_line(out, None, &buf, cs, opts)?;
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(run() as u8)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hl_ext(ext_file: &str, src: &str) -> String {
        let ss = syntax_set();
        let plain = plain_syntax(ss);
        let syntax = resolve_syntax(ss, plain, None, ext_file, Some(src));
        let mut h = HighlightLines::new(syntax, theme());
        let mut v = Vec::new();
        {
            let mut out = BufWriter::new(&mut v);
            for line in Lines(src) {
                let ranges = h.highlight_line(line, ss).unwrap();
                write_ranges(&mut out, &ranges).unwrap();
            }
            out.flush().unwrap();
        }
        String::from_utf8(v).unwrap()
    }

    /// Split keeping '\n' (what the newlines-variant syntax set expects).
    struct Lines<'a>(&'a str);
    impl<'a> Iterator for Lines<'a> {
        type Item = &'a str;
        fn next(&mut self) -> Option<&'a str> {
            if self.0.is_empty() {
                return None;
            }
            let i = self.0.find('\n').map(|p| p + 1).unwrap_or(self.0.len());
            let (a, b) = self.0.split_at(i);
            self.0 = b;
            Some(a)
        }
    }

    #[test]
    fn library_detects_common_extensions() {
        let ss = syntax_set();
        for (ext, name) in [
            ("rs", "Rust"),
            ("py", "Python"),
            ("go", "Go"),
            ("js", "JavaScript (Babel)"),
            ("ts", "TypeScript"),
            ("tsx", "TypeScriptReact"),
            ("jsx", "JavaScript (Babel)"),
            ("cpp", "C++"),
            ("cs", "C#"),
            ("java", "Java"),
            ("swift", "Swift"),
            ("kt", "Kotlin"),
            ("dart", "Dart"),
            ("zig", "Zig"),
            ("jl", "Julia"),
            ("rb", "Ruby"),
            ("sh", "Bourne Again Shell (bash)"),
            ("ps1", "PowerShell"),
            ("sql", "SQL"),
            ("yaml", "YAML"),
            ("toml", "TOML"),
            ("lua", "Lua"),
            ("md", "Markdown"),
            ("html", "HTML"),
            ("vue", "Vue Component"),
            ("json", "JSON"),
            ("css", "CSS"),
            ("scss", "SCSS"),
            ("ini", "INI"),
            ("vim", "VimL"),
            ("tex", "LaTeX"),
            ("dockerfile", "Dockerfile"),
            ("makefile", "Makefile"),
            ("gitignore", "Git Ignore"),
            ("r", "R"),
            ("hs", "Haskell"),
            ("erl", "Erlang"),
        ] {
            let s = ss
                .find_syntax_by_extension(ext)
                .unwrap_or_else(|| panic!("no syntax for .{ext}"));
            assert_eq!(s.name, name, "extension .{ext}");
        }
        assert!(syntax_set().find_syntax_by_extension("txt").is_some());
        assert!(syntax_set()
            .find_syntax_by_extension("definitelynotarealext")
            .is_none());
    }

    #[test]
    fn filenames_without_extensions_resolve() {
        let ss = syntax_set();
        let plain = plain_syntax(ss);
        for (path, name) in [
            ("Makefile", "Makefile"),
            ("Dockerfile", "Dockerfile"),
            ("Dockerfile.dev", "Dockerfile"),
            ("Gemfile", "Ruby"),
            (".gitignore", "Git Ignore"),
            (".vimrc", "VimL"),
        ] {
            let s = resolve_syntax(ss, plain, None, path, None);
            assert_eq!(s.name, name, "file {path}");
        }
    }

    #[test]
    fn library_detects_shebang_first_line() {
        let ss = syntax_set();
        let s = ss
            .find_syntax_by_first_line("#!/usr/bin/env python3\n")
            .expect("shebang detection");
        assert_eq!(s.name, "Python");
    }

    #[test]
    fn keywords_get_color_from_theme() {
        // Structural: the theme must wrap `fn` in SOME 24-bit fg escape.
        let out = hl_ext("a.rs", "fn main() {}\n");
        assert!(
            out.contains("mfn\x1b[0m") && out.contains("38;2;"),
            "rust kw:\n{out:?}"
        );
        let out = hl_ext("a.py", "def f():\n    pass\n");
        assert!(
            out.contains("mdef\x1b[0m") && out.contains("38;2;"),
            "python kw:\n{out:?}"
        );
    }

    #[test]
    fn unknown_extension_is_byte_exact_plain() {
        let ss = syntax_set();
        let plain = plain_syntax(ss);
        let src = "fn main() {}\n";
        let s = resolve_syntax(ss, plain, None, "a.definitelynotarealext", Some(src));
        assert!(std::ptr::eq(s, plain));
    }

    #[test]
    fn override_accepts_extension_or_name() {
        let ss = syntax_set();
        assert!(ss.find_syntax_by_extension("py").is_some());
        assert!(ss.find_syntax_by_name("Python").is_some());
        assert!(ss.find_syntax_by_name("No Such Language").is_none());
    }

    #[test]
    fn cli_parsing() {
        let args = vec![
            "-ns".to_string(),
            "-l".to_string(),
            "py".to_string(),
            "a.py".to_string(),
        ];
        let o = parse_args(&args).unwrap();
        assert!(o.number_all && o.squeeze_blank);
        assert_eq!(o.lang_override, Some("py".to_string()));
        assert_eq!(o.files, vec!["a.py".to_string()]);
    }
}
