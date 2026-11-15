//! Command line front end, with an optional terminal UI and desktop window.

#![forbid(unsafe_code)]

#[cfg(feature = "gui")]
mod gui;
#[cfg(any(feature = "gui", feature = "tui"))]
mod job;
#[cfg(feature = "tui")]
mod tui;

use anyhow::{Context, Result, bail};
use clap::Parser;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use worldpruner::duration::{format_duration, parse_duration};
use worldpruner::pruner::{MAX_RADIUS, Progress, PruneOptions, Report, TICKS_PER_SECOND, prune};
use worldpruner::selection::Selection;
use worldpruner::world::{Dimension, LockResult, lock_world, resolve_dimensions};

const AFTER_HELP: &str = "\
Examples:
  Prune all chunks that players spent at most 1 minute in:
    worldpruner --world /path/to/world --time \"1 minute\"

  Keep 2 chunks around kept chunks, and keep going if a file is damaged:
    worldpruner -w /path/to/world -t \"5 minutes\" -r 2 --continue-on-error

  Prune the Nether and the End, but only show what would happen:
    worldpruner -w /path/to/world -t 1h -D the_nether -D the_end --dry-run

  Set everything up interactively in the terminal:
    worldpruner --tui

Make sure you have a backup of your world, and that it is not open in the
game or on a running server.";

/// Prune chunks from a Minecraft Java Edition world.
#[derive(Parser, Debug)]
#[command(name = "worldpruner", version, after_help = AFTER_HELP)]
struct Args {
    /// The path to the world folder, or to a dimension folder such as world/DIM-1
    #[arg(short = 'w', long, value_name = "PATH", required_unless_present = "tui")]
    world: Option<PathBuf>,

    /// Open the interactive terminal interface. Other options given on the command line fill in its form
    #[arg(short = 'i', long, visible_alias = "interactive")]
    tui: bool,

    /// The minimum time players must have spent in a chunk for it to be kept, e.g. "5 minutes" or "1h30m"
    #[arg(short = 't', long, value_name = "DURATION", default_value = "0s", value_parser = parse_time)]
    time: u64,

    /// The radius of additional chunks preserved around matching chunks
    #[arg(short = 'r', long, value_name = "CHUNKS", default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=MAX_RADIUS as i64))]
    radius: u32,

    /// The path to a whitelist CSV file (a selection exported from MCA Selector) of chunks to keep in any case
    #[arg(short = 'W', long = "white-list", visible_alias = "whitelist", value_name = "FILE")]
    white_list: Option<PathBuf>,

    /// Prune everything except the whitelist, ignoring --time and --radius
    #[arg(short = 'l', long = "white-list-only", visible_alias = "whitelist-only", requires = "white_list")]
    white_list_only: bool,

    /// The dimension to prune when --world is a world folder: an id such as "the_nether" or "mymod:mine",
    /// or "all". Can be given several times. The same whitelist is applied to every dimension [default: overworld]
    #[arg(short = 'D', long = "dimension", value_name = "ID")]
    dimensions: Vec<String>,

    /// Continue if a file or chunk cannot be processed. Unreadable chunks are kept
    #[arg(short = 'c', long)]
    continue_on_error: bool,

    /// Only report what would be pruned, without changing anything
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// The number of worker threads [default: all cores]
    #[arg(short = 'j', long, value_name = "N", default_value_t = 0, hide_default_value = true)]
    threads: usize,

    /// Prune even if the world seems to be open in the game or on a server (this can corrupt the world)
    #[arg(long)]
    force: bool,

    /// Write a log to this file
    #[arg(long, value_name = "FILE")]
    log_file: Option<PathBuf>,

    /// Enables debug logging (to the log file if there is one, to the terminal otherwise)
    #[arg(short = 'd', long)]
    debug: bool,
}

fn parse_time(s: &str) -> Result<u64, String> {
    let seconds = parse_duration(s).map_err(|e| e.to_string())?;
    match seconds.checked_mul(TICKS_PER_SECOND) {
        Some(ticks) if i64::try_from(ticks).is_ok() => Ok(seconds),
        _ => Err("the duration is too long".to_string()),
    }
}

/// The progress bar currently on screen, so log output can be printed above it.
static PROGRESS_BAR: OnceLock<ProgressBar> = OnceLock::new();

/// While the terminal interface owns the screen, messages for the console
/// are collected here instead of being printed.
static CAPTURED_LOG: Mutex<Option<VecDeque<String>>> = Mutex::new(None);
const CAPTURED_LOG_LINES: usize = 500;

/// Collect console messages instead of printing them (or stop doing so).
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub(crate) fn capture_console_log(capture: bool) {
    *CAPTURED_LOG.lock().unwrap_or_else(|e| e.into_inner()) = capture.then(VecDeque::new);
}

/// The most recent `count` captured console messages, oldest first.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub(crate) fn captured_console_log(count: usize) -> Vec<String> {
    let log = CAPTURED_LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.as_ref().map(|lines| lines.iter().skip(lines.len().saturating_sub(count)).cloned().collect()).unwrap_or_default()
}

struct Logger {
    start: Instant,
    console_level: log::LevelFilter,
    file: Option<(log::LevelFilter, Mutex<BufWriter<File>>)>,
}

impl Logger {
    fn install(debug: bool, log_file: Option<&PathBuf>) -> Result<()> {
        let file = match log_file {
            Some(path) => {
                let file = File::create(path).with_context(|| format!("failed to create log file {}", path.display()))?;
                let level = if debug { log::LevelFilter::Debug } else { log::LevelFilter::Info };
                Some((level, Mutex::new(BufWriter::new(file))))
            }
            None => None,
        };
        let console_level = if debug && file.is_none() { log::LevelFilter::Debug } else { log::LevelFilter::Warn };
        let max = file.as_ref().map_or(console_level, |(level, _)| (*level).max(console_level));
        log::set_boxed_logger(Box::new(Logger { start: Instant::now(), console_level, file })).context("failed to set up logging")?;
        log::set_max_level(max);
        Ok(())
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.console_level || self.file.as_ref().is_some_and(|(level, _)| metadata.level() <= *level)
    }

    fn log(&self, record: &log::Record) {
        if record.level() <= self.console_level {
            let line = format!("{}: {}", record.level().as_str().to_lowercase(), record.args());
            let mut captured = CAPTURED_LOG.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(lines) = captured.as_mut() {
                if lines.len() == CAPTURED_LOG_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
            } else {
                drop(captured);
                match PROGRESS_BAR.get() {
                    Some(bar) => bar.suspend(|| eprintln!("{line}")),
                    None => eprintln!("{line}"),
                }
            }
        }
        if let Some((level, file)) = &self.file
            && record.level() <= *level
        {
            let mut file = file.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writeln!(file, "[{:>10.3}s] {:<5} {}", self.start.elapsed().as_secs_f64(), record.level(), record.args());
        }
    }

    fn flush(&self) {
        if let Some((_, file)) = &self.file {
            let _ = file.lock().unwrap_or_else(|e| e.into_inner()).flush();
        }
    }
}

struct CliProgress {
    bar: ProgressBar,
}

impl CliProgress {
    fn new() -> Self {
        let bar = ProgressBar::with_draw_target(Some(0), ProgressDrawTarget::stderr_with_hz(10));
        bar.set_style(
            ProgressStyle::with_template("{msg:<16} {bar:30} {pos}/{len} {percent:>3}%  elapsed {elapsed_precise}  ETA {eta_precise}")
                .expect("valid template")
                .progress_chars("=> "),
        );
        let _ = PROGRESS_BAR.set(bar.clone());
        CliProgress { bar }
    }
}

impl Progress for CliProgress {
    fn begin(&self, message: &str, total: u64) {
        self.bar.reset();
        self.bar.set_length(total);
        self.bar.set_message(message.to_string());
        self.bar.enable_steady_tick(Duration::from_millis(250));
    }

    fn advance(&self, steps: u64) {
        self.bar.inc(steps);
    }

    fn finish(&self) {
        self.bar.disable_steady_tick();
        self.bar.finish_and_clear();
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.2} {}", UNITS[unit]) }
}

/// Human-readable summary of a run.
pub(crate) fn summarize(dimension: &Dimension, report: &Report, elapsed: Duration) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let (removed, deleted, rewritten) = if report.dry_run { ("would remove", "would delete", "would rewrite") } else { ("removed", "deleted", "rewrote") };
    let _ = writeln!(out, "{}{}", dimension.name, if report.dry_run { " (dry run, nothing was changed)" } else { "" });
    let _ =
        writeln!(out, "  scanned {} chunks in {} region files, keeping {} structures", report.chunks_scanned, report.regions_scanned, report.structures.kept);
    for dir in &report.dirs {
        let _ = writeln!(
            out,
            "  {:<9} {removed} {} chunks; {deleted} {} and {rewrote} {} of {} files; {} -> {}",
            format!("{}:", dir.name),
            dir.chunks_removed,
            dir.files_deleted,
            dir.files_rewritten,
            dir.files,
            format_bytes(dir.bytes_before),
            format_bytes(dir.bytes_after),
            rewrote = rewritten,
        );
    }
    let (before, after) = (report.bytes_before(), report.bytes_after());
    let _ = writeln!(
        out,
        "  {} {} ({:.1}%) in {:.1}s",
        if report.dry_run { "would free" } else { "freed" },
        format_bytes(before.saturating_sub(after)),
        if before == 0 { 0.0 } else { before.saturating_sub(after) as f64 * 100.0 / before as f64 },
        elapsed.as_secs_f64()
    );
    if report.chunks_empty > 0 {
        let _ = writeln!(
            out,
            "  {} chunks have space reserved but hold no data; the game regenerates them anyway, so they were treated as never visited",
            report.chunks_empty
        );
    }
    if report.chunks_unreadable > 0 {
        let _ = writeln!(out, "  {} chunks could not be read and were kept", report.chunks_unreadable);
    }
    if report.errors > 0 {
        let _ = writeln!(out, "  {} errors were skipped; see the messages above or the log file", report.errors);
    }
    if report.cancelled {
        let _ = writeln!(out, "  cancelled before everything was processed");
    }
    out
}

/// Describes what is about to happen, for the log and confirmation dialogs.
pub(crate) fn describe(options: &PruneOptions) -> String {
    if options.whitelist_only {
        "pruning everything except the whitelist and structures touching it".to_string()
    } else {
        format!(
            "pruning chunks with an InhabitedTime of at most {} ({} ticks), keeping a radius of {} chunks{}",
            format_duration(options.inhabited_time as u64 / TICKS_PER_SECOND),
            options.inhabited_time,
            options.radius,
            if options.whitelist.is_empty() { "" } else { " and the whitelist" }
        )
    }
}

fn run(args: Args, cancel: &AtomicBool) -> Result<bool> {
    let world = args.world.as_deref().context("the --world argument is required")?;
    let whitelist = match &args.white_list {
        Some(path) => Selection::from_csv_file(path)?,
        None => Selection::new(),
    };
    let dimensions = resolve_dimensions(world, &args.dimensions)?;
    let options = PruneOptions {
        inhabited_time: (args.time * TICKS_PER_SECOND) as i64,
        radius: args.radius,
        whitelist,
        whitelist_only: args.white_list_only,
        continue_on_error: args.continue_on_error,
        dry_run: args.dry_run,
        threads: args.threads,
    };

    let _lock = match lock_world(world) {
        LockResult::Acquired(lock) => Some(lock),
        LockResult::InUse(path) if args.force || args.dry_run => {
            log::warn!("the world is in use by another program ({} is locked); continuing anyway", path.display());
            None
        }
        LockResult::InUse(path) => bail!(
            "the world is open in the game or on a running server ({} is locked). Close it first; pruning a world that is in use corrupts it. Use --force to prune anyway",
            path.display()
        ),
        LockResult::Unknown(path, e) => {
            log::warn!("could not check whether the world is in use ({}: {e})", path.display());
            None
        }
    };

    eprintln!("{}", describe(&options));
    let progress = CliProgress::new();
    let mut complete = true;
    for dimension in &dimensions {
        let start = Instant::now();
        let report = prune(dimension, &options, &progress, cancel).with_context(|| format!("failed to prune {}", dimension.name))?;
        print!("{}", summarize(dimension, &report, start.elapsed()));
        if report.cancelled {
            complete = false;
            break;
        }
    }
    Ok(complete)
}

/// True if a desktop window can be opened from this process.
#[cfg(feature = "gui")]
fn has_display() -> bool {
    !cfg!(all(unix, not(target_os = "macos"))) || std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Open the terminal interface, with its form filled in from `args`.
fn run_tui(args: &Args) -> ExitCode {
    #[cfg(feature = "tui")]
    {
        tui::run(tui::Prefill {
            world: args.world.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            dimensions: args.dimensions.join(", "),
            // without --time the form suggests a more useful value than "0s"
            time: if args.time == 0 { "5 minutes".to_string() } else { format_duration(args.time) },
            radius: args.radius,
            whitelist: args.white_list.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            whitelist_only: args.white_list_only,
            continue_on_error: args.continue_on_error,
            threads: args.threads,
            force: args.force,
        })
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = args;
        eprintln!("error: this build of worldpruner does not include the terminal interface (build with the \"tui\" feature)");
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    // Without arguments, open an interactive front end if there is one.
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let debug_only = raw_args.len() == 1 && (raw_args[0] == "--debug" || raw_args[0] == "-d");
    let interactive = raw_args.is_empty() || debug_only;
    #[cfg(feature = "gui")]
    if interactive && has_display() {
        let _ = Logger::install(debug_only, None);
        return gui::run();
    }
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let args = if interactive && terminal && cfg!(feature = "tui") {
        Args::parse_from(std::iter::once("worldpruner".to_string()).chain(raw_args).chain(["--tui".to_string()]))
    } else {
        Args::parse()
    };
    if let Err(e) = Logger::install(args.debug, args.log_file.as_ref()) {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    if args.tui {
        let code = run_tui(&args);
        log::logger().flush();
        return code;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&cancel);
    let handler = ctrlc::set_handler(move || {
        if handler_flag.swap(true, Ordering::SeqCst) {
            // second Ctrl-C: files are replaced atomically, so exiting is safe
            std::process::exit(130);
        }
        eprintln!("\nstopping after the files that are being processed (press Ctrl-C again to exit immediately)");
    });
    if let Err(e) = handler {
        log::warn!("could not install the Ctrl-C handler: {e}");
    }

    let result = run(args, &cancel);
    log::logger().flush();
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(130),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn parses_original_arguments() {
        let args = Args::try_parse_from(["wp", "--world", "w", "--time", "5 minutes", "--radius", "2", "--white-list", "wl.csv", "-c", "-l", "-d"]).unwrap();
        assert_eq!(args.time, 300);
        assert_eq!(args.radius, 2);
        assert!(args.continue_on_error && args.white_list_only && args.debug);
        let args = Args::try_parse_from(["wp", "-w", "w", "-t", "1d", "-r", "128", "-W", "wl.csv"]).unwrap();
        assert_eq!((args.time, args.radius), (86_400, 128));
        let args = Args::try_parse_from(["wp", "-w", "w"]).unwrap();
        assert_eq!((args.time, args.radius, args.threads), (0, 0, 0));
    }

    #[test]
    fn rejects_invalid_arguments() {
        for bad in [
            vec!["wp"],
            vec!["wp", "-w", "w", "-r", "129"],
            vec!["wp", "-w", "w", "-r", "-1"],
            vec!["wp", "-w", "w", "-t", "soon"],
            vec!["wp", "-w", "w", "-t", "999999999999999999y"],
            vec!["wp", "-w", "w", "--white-list-only"],
        ] {
            assert!(Args::try_parse_from(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
    }
}
