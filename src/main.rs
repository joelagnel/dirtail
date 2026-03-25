use clap::Parser;
use colored::Colorize;
use glob::Pattern;
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Tail all files in a directory matching a glob pattern.
#[derive(Parser, Debug)]
#[command(
    name = "dirtail",
    about = "Tail files matching a glob pattern; watch for new files",
    override_usage = "dirtail [OPTIONS] [DIR] [PATTERN]\n       dirtail --dir <DIR> [--pattern <PATTERN>]"
)]
struct Cli {
    /// Directory to watch (positional)
    #[arg(value_name = "DIR")]
    dir_pos: Option<String>,

    /// Glob pattern to match files (positional, only if DIR positional is also given)
    #[arg(value_name = "PATTERN")]
    pattern_pos: Option<String>,

    /// Directory to watch (named flag)
    #[arg(long = "dir", value_name = "DIR")]
    dir_flag: Option<String>,

    /// Glob pattern to match files (named flag)
    #[arg(long = "pattern", value_name = "PATTERN")]
    pattern_flag: Option<String>,
}

impl Cli {
    fn resolve_dir(&self) -> String {
        self.dir_flag
            .clone()
            .or_else(|| self.dir_pos.clone())
            .unwrap_or_else(|| ".".to_string())
    }

    fn resolve_pattern(&self) -> String {
        self.pattern_flag
            .clone()
            .or_else(|| {
                if self.dir_pos.is_some() {
                    self.pattern_pos.clone()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "*".to_string())
    }
}

// ---------------------------------------------------------------------------
// Message from tail threads to printer
// ---------------------------------------------------------------------------

struct LogLine {
    path: PathBuf,
    line: String,
    color_idx: usize,
}

// ---------------------------------------------------------------------------
// Color palette for file prefixes
// ---------------------------------------------------------------------------

fn color_prefix(text: &str, idx: usize) -> String {
    match idx % 5 {
        0 => format!("{}", text.cyan().bold()),
        1 => format!("{}", text.magenta().bold()),
        2 => format!("{}", text.blue().bold()),
        3 => format!("{}", text.bright_yellow().bold()),
        _ => format!("{}", text.bright_green().bold()),
    }
}

// ---------------------------------------------------------------------------
// Content colorizer: detect log-level keywords
// ---------------------------------------------------------------------------

fn colorize_content(line: &str) -> String {
    let upper = line.to_uppercase();
    if upper.contains("ERROR") || upper.contains("FATAL") || upper.contains("CRITICAL") {
        format!("{}", line.red().bold())
    } else if upper.contains("WARN") {
        format!("{}", line.yellow())
    } else if upper.contains("INFO") {
        format!("{}", line.green())
    } else if upper.contains("DEBUG") {
        format!("{}", line.cyan())
    } else if upper.contains("TRACE") {
        format!("{}", line.dimmed())
    } else {
        line.to_string()
    }
}

// ---------------------------------------------------------------------------
// File registry: tracks which files are being tailed
// ---------------------------------------------------------------------------

struct FileRegistry {
    by_path: HashMap<PathBuf, usize>,
    next_idx: usize,
}

impl FileRegistry {
    fn new() -> Self {
        Self { by_path: HashMap::new(), next_idx: 0 }
    }

    /// Returns Some(color_index) if newly inserted, None if already tracked.
    fn insert(&mut self, path: PathBuf) -> Option<usize> {
        if self.by_path.contains_key(&path) {
            return None;
        }
        let idx = self.next_idx;
        self.next_idx += 1;
        self.by_path.insert(path, idx);
        Some(idx)
    }

    /// Remove a path from the active set (e.g. on rename/delete for log rotation).
    fn remove(&mut self, path: &Path) {
        self.by_path.remove(path);
    }

    #[allow(dead_code)]
    fn index_of(&self, path: &Path) -> usize {
        self.by_path.get(path).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Tail thread
// ---------------------------------------------------------------------------

const TAIL_LINES: usize = 10;

/// Read the last TAIL_LINES lines of `path` synchronously, send them to `tx`,
/// and return the file's EOF position (used as `start_pos` for the follow thread).
fn read_initial_tail(path: &Path, color_idx: usize, tx: &Sender<LogLine>) -> u64 {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dirtail: cannot open {:?}: {}", path, e);
            return 0;
        }
    };
    let mut reader = BufReader::new(file);
    // Ring buffer: keep only the last TAIL_LINES lines without reading all into memory.
    let mut ring: [String; TAIL_LINES] = Default::default();
    let mut count: usize = 0;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                ring[count % TAIL_LINES] =
                    line.trim_end_matches(&['\n', '\r'][..]).to_string();
                count += 1;
            }
            Err(e) => {
                eprintln!("dirtail: read error on {:?}: {}", path, e);
                break;
            }
        }
    }
    let emit_count = count.min(TAIL_LINES);
    let start_slot = if count <= TAIL_LINES { 0 } else { count % TAIL_LINES };
    for i in 0..emit_count {
        let slot = (start_slot + i) % TAIL_LINES;
        let _ = tx.send(LogLine {
            path: path.to_path_buf(),
            line: ring[slot].clone(),
            color_idx,
        });
    }
    reader.stream_position().unwrap_or(0)
}

/// Seek to `start_pos` and follow new content. Use `start_pos = 0` for new files
/// (follow from the beginning); pass the value returned by `read_initial_tail`
/// for pre-existing files (follow from EOF).
fn spawn_tail_thread(path: PathBuf, start_pos: u64, color_idx: usize, tx: Sender<LogLine>) {
    thread::spawn(move || {
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("dirtail: cannot open {:?}: {}", path, e);
                return;
            }
        };
        let mut reader = BufReader::new(file);
        if start_pos > 0 {
            if let Err(e) = reader.seek(SeekFrom::Start(start_pos)) {
                eprintln!("dirtail: seek error on {:?}: {}", path, e);
                return;
            }
        }

        // Follow loop: poll for new data every 100ms
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    thread::sleep(Duration::from_millis(100));
                    // Handle file truncation (log rotation)
                    if let (Ok(pos), Ok(meta)) = (
                        reader.stream_position(),
                        reader.get_ref().metadata(),
                    ) {
                        if pos > meta.len() {
                            let _ = reader.seek(SeekFrom::Start(0));
                        }
                    }
                }
                Ok(_) => {
                    let trimmed =
                        line.trim_end_matches(&['\n', '\r'][..]).to_string();
                    if tx
                        .send(LogLine { path: path.clone(), line: trimmed, color_idx })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("dirtail: read error on {:?}: {}", path, e);
                    return;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let dir_str = cli.resolve_dir();
    let pattern_str = cli.resolve_pattern();
    let dir = PathBuf::from(&dir_str);

    if !dir.is_dir() {
        eprintln!("dirtail: {:?} is not a directory", dir);
        std::process::exit(1);
    }

    let glob_pattern = match Pattern::new(&pattern_str) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("dirtail: invalid glob pattern {:?}: {}", pattern_str, e);
            std::process::exit(1);
        }
    };

    let (tx, rx) = mpsc::channel::<LogLine>();
    let registry = Arc::new(Mutex::new(FileRegistry::new()));

    // Scan existing files, sorted by name for deterministic color assignment
    let mut existing: Vec<PathBuf> = dir
        .read_dir()
        .expect("cannot read directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| glob_pattern.matches(n))
                .unwrap_or(false)
        })
        .collect();
    existing.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    // Startup banner
    println!("{}", "=== dirtail ===".bold());
    println!("  Dir     : {}", dir.display().to_string().cyan());
    println!("  Pattern : {}", pattern_str.cyan());
    if existing.is_empty() {
        println!("  {}", "No existing files match. Watching for new files...".yellow());
    } else {
        println!("  Tailing {} existing file(s):", existing.len());
        for p in &existing {
            println!("    {}", p.display().to_string().green());
        }
    }
    println!("{}", "===============".bold());

    // Phase A: name-sorted (above) → assign color indices
    let mut file_colors: Vec<(PathBuf, usize)> = Vec::new();
    for path in existing {
        let mut reg = registry.lock().unwrap();
        if let Some(idx) = reg.insert(path.clone()) {
            drop(reg);
            file_colors.push((path, idx));
        }
    }

    // Phase B: mtime-sort → read initial lines in mtime order (oldest first),
    // then spawn follow threads. This ensures the visual "age" of output decreases
    // downward: oldest-modified files appear first, most-recently-modified last.
    // Cache mtimes before sorting to avoid redundant stat() calls in the comparator.
    let mut file_colors_mtime: Vec<(PathBuf, usize, Option<std::time::SystemTime>)> =
        file_colors
            .into_iter()
            .map(|(p, idx)| {
                let mtime = p.metadata().ok().and_then(|m| m.modified().ok());
                (p, idx, mtime)
            })
            .collect();
    file_colors_mtime.sort_by(|(_, _, a), (_, _, b)| a.cmp(b));
    for (path, idx, _) in file_colors_mtime {
        let start_pos = read_initial_tail(&path, idx, &tx);
        spawn_tail_thread(path, start_pos, idx, tx.clone());
    }

    // Spawn directory watcher thread
    {
        let dir_clone = dir.clone();
        let registry_clone = Arc::clone(&registry);
        let tx_clone = tx.clone();
        let pattern_clone = glob_pattern.clone();

        thread::spawn(move || {
            let (wtx, wrx) = mpsc::channel();
            let mut watcher = match notify::recommended_watcher(wtx) {
                Ok(w) => w,
                Err(e) => { eprintln!("dirtail: watcher error: {}", e); return; }
            };
            if let Err(e) = watcher.watch(&dir_clone, RecursiveMode::NonRecursive) {
                eprintln!("dirtail: watch error: {}", e);
                return;
            }
            for res in wrx {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Create(_) => {
                            for path in event.paths {
                                if !path.is_file() { continue; }
                                let matches = path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .map(|n| pattern_clone.matches(n))
                                    .unwrap_or(false);
                                if matches {
                                    let mut reg = registry_clone.lock().unwrap();
                                    if let Some(idx) = reg.insert(path.clone()) {
                                        drop(reg);
                                        println!(
                                            "{} {}",
                                            "[dirtail] new file:".bold(),
                                            path.display().to_string().green()
                                        );
                                        spawn_tail_thread(path, 0, idx, tx_clone.clone());
                                    }
                                }
                            }
                        }
                        // On rename-out or delete, clear the path from the registry so that
                        // a new file created at the same path (log rotation) can be tailed.
                        EventKind::Remove(_)
                        | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                            for path in event.paths {
                                registry_clone.lock().unwrap().remove(&path);
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    // Print loop (main thread): color_idx is stored in each message, no lock needed
    for msg in rx {
        let name = msg.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let prefix = color_prefix(&format!("[{}]", name), msg.color_idx);
        let content = colorize_content(&msg.line);
        println!("{} {}", prefix, content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp(suffix: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("dirtail_{}_{}.{}", std::process::id(), n, suffix))
    }

    /// Strip ANSI escape codes so we can test colorize_content text content.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&ch) = chars.peek() {
                    chars.next();
                    if ch == 'm' { break; }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    // ── FileRegistry ─────────────────────────────────────────────────────────

    #[test]
    fn registry_new_insert_returns_index_zero() {
        let mut reg = FileRegistry::new();
        assert_eq!(reg.insert(PathBuf::from("/tmp/a.log")), Some(0));
    }

    #[test]
    fn registry_duplicate_insert_returns_none() {
        let mut reg = FileRegistry::new();
        let p = PathBuf::from("/tmp/a.log");
        reg.insert(p.clone());
        assert_eq!(reg.insert(p), None);
    }

    #[test]
    fn registry_sequential_indices() {
        let mut reg = FileRegistry::new();
        assert_eq!(reg.insert(PathBuf::from("/a")), Some(0));
        assert_eq!(reg.insert(PathBuf::from("/b")), Some(1));
        assert_eq!(reg.insert(PathBuf::from("/c")), Some(2));
    }

    #[test]
    fn registry_index_of_known() {
        let mut reg = FileRegistry::new();
        let p0 = PathBuf::from("/a");
        let p1 = PathBuf::from("/b");
        reg.insert(p0.clone());
        reg.insert(p1.clone());
        assert_eq!(reg.index_of(&p0), 0);
        assert_eq!(reg.index_of(&p1), 1);
    }

    #[test]
    fn registry_index_of_unknown_returns_zero() {
        let reg = FileRegistry::new();
        assert_eq!(reg.index_of(&PathBuf::from("/unknown")), 0);
    }

    #[test]
    fn registry_remove_allows_reinsert() {
        let mut reg = FileRegistry::new();
        let p = PathBuf::from("/tmp/app.log");
        assert_eq!(reg.insert(p.clone()), Some(0));
        // Duplicate insert blocked
        assert_eq!(reg.insert(p.clone()), None);
        // After remove (log rotation), same path can be inserted again
        reg.remove(&p);
        assert_eq!(reg.insert(p.clone()), Some(1));
    }

    // ── colorize_content ─────────────────────────────────────────────────────

    // colored skips ANSI codes when stdout isn't a TTY (e.g. in tests).
    // Force them on at the start of each colorize test.

    #[test]
    fn colorize_error_applies_ansi_and_preserves_text() {
        colored::control::set_override(true);
        let line = "2024-01-01 ERROR: crash";
        let out = colorize_content(line);
        assert_ne!(out, line, "expected ANSI codes to be added");
        assert!(strip_ansi(&out).contains("ERROR: crash"));
    }

    #[test]
    fn colorize_fatal_same_path_as_error() {
        colored::control::set_override(true);
        let out = colorize_content("FATAL: out of memory");
        assert_ne!(out, "FATAL: out of memory");
        assert!(strip_ansi(&out).contains("FATAL: out of memory"));
    }

    #[test]
    fn colorize_warn_applies_ansi() {
        colored::control::set_override(true);
        let out = colorize_content("WARN: disk 90% full");
        assert_ne!(out, "WARN: disk 90% full");
        assert!(strip_ansi(&out).contains("WARN: disk 90% full"));
    }

    #[test]
    fn colorize_warning_matches_warn_check() {
        colored::control::set_override(true);
        let out = colorize_content("WARNING: slow response");
        assert_ne!(out, "WARNING: slow response");
    }

    #[test]
    fn colorize_info_applies_ansi() {
        colored::control::set_override(true);
        let out = colorize_content("INFO: server started");
        assert_ne!(out, "INFO: server started");
        assert!(strip_ansi(&out).contains("INFO: server started"));
    }

    #[test]
    fn colorize_debug_applies_ansi() {
        colored::control::set_override(true);
        let out = colorize_content("DEBUG: processing request");
        assert_ne!(out, "DEBUG: processing request");
    }

    #[test]
    fn colorize_trace_applies_ansi() {
        colored::control::set_override(true);
        let out = colorize_content("TRACE: entering function");
        assert_ne!(out, "TRACE: entering function");
    }

    #[test]
    fn colorize_plain_text_unchanged() {
        // plain text has no keywords — no ANSI regardless of color setting
        let line = "plain log line with no level keyword";
        assert_eq!(colorize_content(line), line);
    }

    #[test]
    fn colorize_case_insensitive() {
        colored::control::set_override(true);
        // lowercase "error" should still trigger red
        let out = colorize_content("error: lowercase");
        assert_ne!(out, "error: lowercase");
    }

    // ── CLI resolution ────────────────────────────────────────────────────────

    fn make_cli(
        dir_pos: Option<&str>,
        pattern_pos: Option<&str>,
        dir_flag: Option<&str>,
        pattern_flag: Option<&str>,
    ) -> Cli {
        Cli {
            dir_pos: dir_pos.map(String::from),
            pattern_pos: pattern_pos.map(String::from),
            dir_flag: dir_flag.map(String::from),
            pattern_flag: pattern_flag.map(String::from),
        }
    }

    #[test]
    fn cli_resolve_dir_defaults_to_dot() {
        assert_eq!(make_cli(None, None, None, None).resolve_dir(), ".");
    }

    #[test]
    fn cli_resolve_dir_from_positional() {
        assert_eq!(make_cli(Some("/tmp"), None, None, None).resolve_dir(), "/tmp");
    }

    #[test]
    fn cli_resolve_dir_flag_wins_over_positional() {
        assert_eq!(
            make_cli(Some("/tmp"), None, Some("/var/log"), None).resolve_dir(),
            "/var/log"
        );
    }

    #[test]
    fn cli_resolve_pattern_defaults_to_star() {
        assert_eq!(make_cli(Some("/tmp"), None, None, None).resolve_pattern(), "*");
    }

    #[test]
    fn cli_resolve_pattern_from_second_positional() {
        assert_eq!(
            make_cli(Some("/tmp"), Some("*.log"), None, None).resolve_pattern(),
            "*.log"
        );
    }

    #[test]
    fn cli_resolve_pattern_flag_wins_over_positional() {
        assert_eq!(
            make_cli(Some("/tmp"), Some("*.log"), None, Some("*.txt")).resolve_pattern(),
            "*.txt"
        );
    }

    #[test]
    fn cli_resolve_pattern_pos_ignored_without_dir_pos() {
        // pattern_pos alone (no dir_pos) should fall back to "*"
        assert_eq!(make_cli(None, Some("*.log"), None, None).resolve_pattern(), "*");
    }

    // ── Tail thread integration ───────────────────────────────────────────────

    #[test]
    fn tail_existing_file_emits_last_10_of_15_lines() {
        let path = unique_temp("log");
        let content: String = (1..=15).map(|i| format!("line {}\n", i)).collect();
        std::fs::write(&path, content).unwrap();

        let (tx, rx) = mpsc::channel::<LogLine>();
        read_initial_tail(&path, 0, &tx);
        drop(tx);

        let received: Vec<String> = rx.iter().map(|m| m.line).collect();
        let _ = std::fs::remove_file(&path);

        assert_eq!(received.len(), 10);
        assert_eq!(received[0], "line 6");
        assert_eq!(received[9], "line 15");
    }

    #[test]
    fn tail_existing_file_fewer_than_10_lines_emits_all() {
        let path = unique_temp("log");
        std::fs::write(&path, "line 1\nline 2\nline 3\n").unwrap();

        let (tx, rx) = mpsc::channel::<LogLine>();
        read_initial_tail(&path, 0, &tx);
        drop(tx);

        let received: Vec<String> = rx.iter().map(|m| m.line).collect();
        let _ = std::fs::remove_file(&path);

        assert_eq!(received, vec!["line 1", "line 2", "line 3"]);
    }

    #[test]
    fn tail_new_file_follows_from_beginning() {
        let path = unique_temp("log");
        std::fs::File::create(&path).unwrap(); // empty

        let (tx, rx) = mpsc::channel::<LogLine>();
        spawn_tail_thread(path.clone(), 0, 0, tx);
        thread::sleep(Duration::from_millis(150));

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "INFO: hello from new file").unwrap();
        f.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).expect("expected a line");
        let _ = std::fs::remove_file(&path);

        assert_eq!(msg.line, "INFO: hello from new file");
    }

    #[test]
    fn tail_follows_appended_lines() {
        let path = unique_temp("log");
        std::fs::write(&path, "existing\n").unwrap();

        let (tx, rx) = mpsc::channel::<LogLine>();
        let start_pos = read_initial_tail(&path, 0, &tx);
        // drain the initial line
        let _ = rx.recv_timeout(Duration::from_secs(2));
        spawn_tail_thread(path.clone(), start_pos, 0, tx);
        thread::sleep(Duration::from_millis(200));

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "ERROR: appended error").unwrap();
        f.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).expect("expected appended line");
        let _ = std::fs::remove_file(&path);

        assert_eq!(msg.line, "ERROR: appended error");
    }

    #[test]
    fn tail_empty_file_then_write_is_picked_up() {
        let path = unique_temp("log");
        std::fs::write(&path, "").unwrap();

        let (tx, rx) = mpsc::channel::<LogLine>();
        let start_pos = read_initial_tail(&path, 0, &tx); // existing but empty
        spawn_tail_thread(path.clone(), start_pos, 0, tx);
        thread::sleep(Duration::from_millis(150));

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "DEBUG: written after start").unwrap();
        f.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).expect("expected a line");
        let _ = std::fs::remove_file(&path);

        assert_eq!(msg.line, "DEBUG: written after start");
    }

    #[test]
    fn startup_initial_lines_emitted_in_mtime_ascending_order() {
        let old_path = unique_temp("log");
        let mid_path = unique_temp("log");
        let new_path = unique_temp("log");
        std::fs::write(&old_path, "old line\n").unwrap();
        std::fs::write(&mid_path, "mid line\n").unwrap();
        std::fs::write(&new_path, "new line\n").unwrap();

        // Set mtimes: old (2020) < mid (2023) < new (2026)
        std::process::Command::new("touch")
            .args(["-t", "202001010000", old_path.to_str().unwrap()])
            .status().unwrap();
        std::process::Command::new("touch")
            .args(["-t", "202301010000", mid_path.to_str().unwrap()])
            .status().unwrap();
        std::process::Command::new("touch")
            .args(["-t", "202601010000", new_path.to_str().unwrap()])
            .status().unwrap();

        // Simulate Phase A: color assignment (intentionally not in mtime order)
        let file_colors: Vec<(PathBuf, usize)> = vec![
            (new_path.clone(), 0),
            (old_path.clone(), 1),
            (mid_path.clone(), 2),
        ];

        // Simulate Phase B: cache mtimes, sort ascending (same logic as main())
        let mut file_colors_mtime: Vec<(PathBuf, usize, Option<std::time::SystemTime>)> =
            file_colors
                .into_iter()
                .map(|(p, idx)| {
                    let mtime = p.metadata().ok().and_then(|m| m.modified().ok());
                    (p, idx, mtime)
                })
                .collect();
        file_colors_mtime.sort_by(|(_, _, a), (_, _, b)| a.cmp(b));

        let (tx, rx) = mpsc::channel::<LogLine>();
        for (path, idx, _) in &file_colors_mtime {
            read_initial_tail(path, *idx, &tx);
        }
        drop(tx);

        let received: Vec<String> = rx.iter().map(|m| m.line).collect();
        let _ = std::fs::remove_file(&old_path);
        let _ = std::fs::remove_file(&mid_path);
        let _ = std::fs::remove_file(&new_path);

        // Lines must arrive oldest-first: old, mid, new
        assert_eq!(received, vec!["old line", "mid line", "new line"]);
    }
}
