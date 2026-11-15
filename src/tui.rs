//! Interactive terminal interface (`worldpruner --tui`).
//!
//! A form for the same options as the command line, a dry-run preview, a
//! confirmation step and live progress. It only needs a terminal, so it
//! works over SSH on a headless server.

use crate::job::{Job, RunningJob, format_clock};
use crate::{capture_console_log, captured_console_log, describe};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, Padding, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use worldpruner::duration::parse_duration;
use worldpruner::pruner::{MAX_RADIUS, PruneOptions, TICKS_PER_SECOND};
use worldpruner::selection::Selection;
use worldpruner::world::resolve_dimensions;

/// Initial contents of the form, taken from the command line.
pub(crate) struct Prefill {
    pub world: String,
    pub dimensions: String,
    pub time: String,
    pub radius: u32,
    pub whitelist: String,
    pub whitelist_only: bool,
    pub continue_on_error: bool,
    pub threads: usize,
    pub force: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    World,
    Dimensions,
    Time,
    Radius,
    Whitelist,
    WhitelistOnly,
    ContinueOnError,
    Threads,
    Preview,
    Prune,
    Quit,
}

/// All focusable items, in navigation order.
const FIELDS: [Field; 11] = [
    Field::World,
    Field::Dimensions,
    Field::Time,
    Field::Radius,
    Field::Whitelist,
    Field::WhitelistOnly,
    Field::ContinueOnError,
    Field::Threads,
    Field::Preview,
    Field::Prune,
    Field::Quit,
];
/// The first eight fields have a row each; the three buttons share one.
const FIELD_ROWS: usize = 8;
const LABEL_WIDTH: usize = 14;
const MAX_THREADS: usize = 1024;
const LOG_LINES: usize = 6;

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::World => "World",
            Field::Dimensions => "Dimensions",
            Field::Time => "InhabitedTime",
            Field::Radius => "Radius",
            Field::Whitelist => "Whitelist",
            Field::WhitelistOnly => "Prune everything except the whitelist",
            Field::ContinueOnError => "Continue on errors",
            Field::Threads => "Threads",
            Field::Preview => "Preview (dry run)",
            Field::Prune => "Prune",
            Field::Quit => "Quit",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Field::World => {
                "The folder containing your world files (the one with level.dat). A single dimension folder such as world/DIM-1 works too. Tab completes the path."
            }
            Field::Dimensions => {
                "Which dimensions of the world to prune, separated by commas: the_nether, the_end, mymod:mine, or all. Leave empty for the Overworld. The same whitelist is applied to every dimension."
            }
            Field::Time => {
                "The minimum InhabitedTime of a chunk for it to be kept, e.g. \"5 minutes\", \"1h30m\" or \"1 day 3 hours\". InhabitedTime is the accumulated time all players have spent in a chunk."
            }
            Field::Radius => "The radius of additional chunks preserved around chunks that match the InhabitedTime condition (0 to 128).",
            Field::Whitelist => {
                "Optional CSV file with chunks and regions to keep in any case. Export one from MCA Selector with Selection > Export selection as .csv. Tab completes the path."
            }
            Field::WhitelistOnly => "Keep only the whitelist and structures touching it, ignoring InhabitedTime and radius. Space toggles.",
            Field::ContinueOnError => {
                "Keep going if a file or chunk cannot be processed; unreadable chunks are kept. Otherwise the run stops before anything is changed. Space toggles."
            }
            Field::Threads => "The number of worker threads. 0 uses all cores.",
            Field::Preview => "Scan the world and show what would be removed. Nothing is changed.",
            Field::Prune => "Prune the world. You will be asked to confirm. This cannot be undone, so make sure you have a backup.",
            Field::Quit => "Leave without changing anything.",
        }
    }

    fn is_text(self) -> bool {
        matches!(self, Field::World | Field::Dimensions | Field::Time | Field::Radius | Field::Whitelist | Field::Threads)
    }

    fn is_button(self) -> bool {
        matches!(self, Field::Preview | Field::Prune | Field::Quit)
    }
}

/// The values the user is editing.
struct Form {
    world: String,
    dimensions: String,
    time: String,
    radius: String,
    whitelist: String,
    threads: String,
    whitelist_only: bool,
    continue_on_error: bool,
    /// Index into [`FIELDS`].
    focus: usize,
    /// Path completion candidates or the reason a run could not start.
    hint: Option<(String, bool)>,
}

impl Form {
    fn new(prefill: &Prefill) -> Form {
        Form {
            world: prefill.world.clone(),
            dimensions: prefill.dimensions.clone(),
            time: prefill.time.clone(),
            radius: prefill.radius.to_string(),
            whitelist: prefill.whitelist.clone(),
            threads: prefill.threads.to_string(),
            whitelist_only: prefill.whitelist_only,
            continue_on_error: prefill.continue_on_error,
            focus: 0,
            hint: None,
        }
    }

    fn field(&self) -> Field {
        FIELDS[self.focus]
    }

    fn text(&self, field: Field) -> &str {
        match field {
            Field::World => &self.world,
            Field::Dimensions => &self.dimensions,
            Field::Time => &self.time,
            Field::Radius => &self.radius,
            Field::Whitelist => &self.whitelist,
            Field::Threads => &self.threads,
            _ => "",
        }
    }

    fn text_mut(&mut self, field: Field) -> Option<&mut String> {
        match field {
            Field::World => Some(&mut self.world),
            Field::Dimensions => Some(&mut self.dimensions),
            Field::Time => Some(&mut self.time),
            Field::Radius => Some(&mut self.radius),
            Field::Whitelist => Some(&mut self.whitelist),
            Field::Threads => Some(&mut self.threads),
            _ => None,
        }
    }

    fn dimension_ids(&self) -> Vec<String> {
        self.dimensions.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    fn ticks(&self) -> Result<i64, String> {
        let seconds = parse_duration(&self.time).map_err(|e| e.to_string())?;
        seconds.checked_mul(TICKS_PER_SECOND).and_then(|t| i64::try_from(t).ok()).ok_or_else(|| "the duration is too long".to_string())
    }

    fn radius(&self) -> Result<u32, String> {
        self.radius.trim().parse().ok().filter(|r| *r <= MAX_RADIUS).ok_or_else(|| format!("enter a number from 0 to {MAX_RADIUS}"))
    }

    fn threads(&self) -> Result<usize, String> {
        self.threads.trim().parse().ok().filter(|t| *t <= MAX_THREADS).ok_or_else(|| format!("enter a number from 0 to {MAX_THREADS} (0 = all cores)"))
    }

    /// What is wrong with a field, if anything.
    fn problem(&self, field: Field) -> Option<String> {
        let world = expand_home(self.world.trim());
        let ids = self.dimension_ids();
        match field {
            Field::World if world.is_empty() => Some("required".to_string()),
            Field::World if !Path::new(&world).is_dir() => Some("not a folder".to_string()),
            // with dimension ids, problems are reported next to them instead
            Field::World if ids.is_empty() => resolve_dimensions(Path::new(&world), &[]).err().map(|e| e.to_string()),
            Field::Dimensions if !ids.is_empty() && Path::new(&world).is_dir() => resolve_dimensions(Path::new(&world), &ids).err().map(|e| e.to_string()),
            Field::Time if !self.whitelist_only => self.ticks().err(),
            Field::Radius if !self.whitelist_only => self.radius().err(),
            Field::Whitelist => {
                let path = expand_home(self.whitelist.trim());
                if path.is_empty() {
                    self.whitelist_only.then(|| "required to prune everything except the whitelist".to_string())
                } else if !Path::new(&path).is_file() {
                    Some("not a file".to_string())
                } else {
                    None
                }
            }
            Field::Threads => self.threads().err(),
            _ => None,
        }
    }

    /// Turn the form into a job, or explain what has to be fixed first.
    fn job(&self, dry_run: bool, force: bool) -> Result<Job, String> {
        if let Some((field, problem)) = FIELDS.iter().find_map(|&f| self.problem(f).map(|p| (f, p))) {
            return Err(format!("{}: {problem}", field.label()));
        }
        let world = PathBuf::from(expand_home(self.world.trim()));
        let dimensions = resolve_dimensions(&world, &self.dimension_ids()).map_err(|e| format!("World: {e}"))?;
        let whitelist_path = expand_home(self.whitelist.trim());
        let whitelist = if whitelist_path.is_empty() {
            Selection::new()
        } else {
            Selection::from_csv_file(Path::new(&whitelist_path)).map_err(|e| format!("Whitelist: {e:#}"))?
        };
        let options = PruneOptions {
            inhabited_time: if self.whitelist_only { 0 } else { self.ticks()? },
            radius: if self.whitelist_only { 0 } else { self.radius()? },
            whitelist,
            whitelist_only: self.whitelist_only,
            continue_on_error: self.continue_on_error,
            dry_run,
            threads: self.threads()?,
        };
        Ok(Job { world, dimensions, options, force })
    }

    fn move_focus(&mut self, delta: isize) {
        self.focus = (self.focus as isize + delta).rem_euclid(FIELDS.len() as isize) as usize;
        self.hint = None;
    }

    /// Complete the path in the focused field. Returns false if there was
    /// nothing to complete.
    fn complete(&mut self) -> bool {
        let field = self.field();
        let dirs_only = field == Field::World;
        let Some(text) = self.text_mut(field) else { return false };
        let (completed, candidates) = complete_path(text, dirs_only);
        let changed = completed != *text;
        *text = completed;
        self.hint = (candidates.len() > 1).then(|| (candidates.join("  "), false));
        changed || candidates.len() > 1
    }
}

/// Replace a leading `~` with the home directory.
fn expand_home(path: &str) -> String {
    match (path.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(home)) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

/// Shell-style completion of the last component of `input`. Returns the
/// (possibly longer) input and the names that match.
fn complete_path(input: &str, dirs_only: bool) -> (String, Vec<String>) {
    let expanded = expand_home(input);
    let split = expanded.rfind(['/', std::path::MAIN_SEPARATOR]).map_or(0, |i| i + 1);
    let (dir_part, prefix) = expanded.split_at(split);
    let dir = if dir_part.is_empty() { Path::new(".") } else { Path::new(dir_part) };
    let Ok(entries) = std::fs::read_dir(dir) else { return (input.to_string(), Vec::new()) };
    let mut matches: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let is_dir = entry.path().is_dir();
            let hidden = name.starts_with('.') && !prefix.starts_with('.');
            (name.starts_with(prefix) && !hidden && (is_dir || !dirs_only)).then(|| if is_dir { format!("{name}/") } else { name })
        })
        .collect();
    matches.sort();
    let Some(first) = matches.first() else { return (input.to_string(), matches) };
    // extend to the longest prefix all matches share
    let mut common = first.len();
    for name in &matches[1..] {
        common = first.char_indices().zip(name.chars()).take_while(|((_, a), b)| a == b).last().map_or(0, |((i, a), _)| i + a.len_utf8()).min(common);
    }
    (format!("{dir_part}{}", &first[..common]), matches)
}

/// The end of `text` that fits into `width` columns.
fn tail(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    let skip = count - width.saturating_sub(1);
    format!("\u{2026}{}", text.chars().skip(skip).collect::<String>())
}

/// The result of a finished run.
struct Outcome {
    text: String,
    is_error: bool,
    was_dry_run: bool,
    scroll: u16,
}

enum Screen {
    Form,
    Confirm(Job),
    Running(RunningJob),
    Done(Outcome),
}

struct App {
    form: Form,
    screen: Screen,
    force: bool,
    /// What the current or last run does, shown while it is running.
    description: String,
    quit: bool,
}

impl App {
    fn new(prefill: Prefill) -> App {
        App { form: Form::new(&prefill), screen: Screen::Form, force: prefill.force, description: String::new(), quit: false }
    }

    /// Validate the form and start a preview, or ask before a real run.
    fn request_run(&mut self, dry_run: bool) {
        match self.form.job(dry_run, self.force) {
            Ok(job) if dry_run => self.start(job),
            Ok(job) => self.screen = Screen::Confirm(job),
            Err(problem) => {
                self.form.hint = Some((problem, true));
                self.screen = Screen::Form;
            }
        }
    }

    fn start(&mut self, job: Job) {
        self.description = format!("{}: {}", job.world.display(), describe(&job.options));
        self.form.hint = None;
        // the screen is redrawn regularly, so the worker does not have to wake it up
        self.screen = Screen::Running(RunningJob::start(job, || {}));
    }

    /// Pick up the result of a finished run.
    fn tick(&mut self) {
        if let Screen::Running(running) = &self.screen
            && let Some(result) = running.poll()
        {
            let was_dry_run = running.dry_run;
            self.screen = Screen::Done(match result {
                Ok(text) => Outcome { text, is_error: false, was_dry_run, scroll: 0 },
                Err(text) => Outcome { text, is_error: true, was_dry_run, scroll: 0 },
            });
        }
    }

    fn on_paste(&mut self, text: &str) {
        if matches!(self.screen, Screen::Form) {
            let field = self.form.field();
            if let Some(value) = self.form.text_mut(field) {
                value.extend(text.chars().filter(|c| !c.is_control()));
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        match &mut self.screen {
            Screen::Form => self.on_form_key(key, ctrl_c),
            Screen::Confirm(_) => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    if let Screen::Confirm(job) = std::mem::replace(&mut self.screen, Screen::Form) {
                        self.start(job);
                    }
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.screen = Screen::Form,
                _ if ctrl_c => self.screen = Screen::Form,
                _ => {}
            },
            Screen::Running(running) => {
                if ctrl_c || matches!(key.code, KeyCode::Esc | KeyCode::Char('c' | 'C')) {
                    running.cancel();
                }
            }
            Screen::Done(outcome) => match key.code {
                _ if ctrl_c => self.quit = true,
                KeyCode::Char('q' | 'Q') => self.quit = true,
                KeyCode::Up => outcome.scroll = outcome.scroll.saturating_sub(1),
                KeyCode::Down => outcome.scroll = outcome.scroll.saturating_add(1),
                KeyCode::PageUp => outcome.scroll = outcome.scroll.saturating_sub(10),
                KeyCode::PageDown => outcome.scroll = outcome.scroll.saturating_add(10),
                KeyCode::Char('p' | 'P') if outcome.was_dry_run && !outcome.is_error => self.request_run(false),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('b' | 'B') => self.screen = Screen::Form,
                _ => {}
            },
        }
    }

    fn on_form_key(&mut self, key: KeyEvent, ctrl_c: bool) {
        let field = self.form.field();
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            _ if ctrl_c => self.quit = true,
            KeyCode::Esc => self.quit = true,
            KeyCode::F(2) => self.request_run(true),
            KeyCode::F(10) => self.request_run(false),
            KeyCode::Up | KeyCode::BackTab => self.form.move_focus(-1),
            KeyCode::Down => self.form.move_focus(1),
            KeyCode::Left if field.is_button() && field != Field::Preview => self.form.move_focus(-1),
            KeyCode::Right if field.is_button() && field != Field::Quit => self.form.move_focus(1),
            KeyCode::Tab => {
                let is_path = matches!(field, Field::World | Field::Whitelist);
                if !(is_path && self.form.complete()) {
                    self.form.move_focus(1);
                }
            }
            KeyCode::Enter => match field {
                Field::Preview => self.request_run(true),
                Field::Prune => self.request_run(false),
                Field::Quit => self.quit = true,
                Field::WhitelistOnly => self.form.whitelist_only = !self.form.whitelist_only,
                Field::ContinueOnError => self.form.continue_on_error = !self.form.continue_on_error,
                _ => self.form.move_focus(1),
            },
            KeyCode::Char(' ') if field == Field::WhitelistOnly => self.form.whitelist_only = !self.form.whitelist_only,
            KeyCode::Char(' ') if field == Field::ContinueOnError => self.form.continue_on_error = !self.form.continue_on_error,
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(text) = self.form.text_mut(field) {
                    text.clear();
                }
            }
            KeyCode::Char(c) if plain => {
                if let Some(text) = self.form.text_mut(field) {
                    text.push(c);
                    self.form.hint = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(text) = self.form.text_mut(field) {
                    text.pop();
                    self.form.hint = None;
                }
            }
            _ => {}
        }
    }

    // ---- drawing ---------------------------------------------------------

    fn draw(&self, frame: &mut Frame) {
        let log = captured_console_log(LOG_LINES);
        let log_height = if log.is_empty() { 0 } else { log.len() as u16 + 2 };
        let [main, log_area, footer] = Layout::vertical([Constraint::Min(6), Constraint::Length(log_height), Constraint::Length(1)]).areas(frame.area());

        let keys = match &self.screen {
            Screen::Form => "Up/Down move  Tab complete  Enter select  F2 preview  F10 prune  Esc quit",
            Screen::Confirm(_) => "y prune  n cancel",
            Screen::Running(_) => "Esc cancel",
            Screen::Done(outcome) if outcome.was_dry_run && !outcome.is_error => "p prune now  Enter back  Up/Down scroll  q quit",
            Screen::Done(_) => "Enter back  Up/Down scroll  q quit",
        };
        frame.render_widget(Paragraph::new(keys).style(Style::new().fg(Color::DarkGray)), footer);
        if !log.is_empty() {
            let lines: Vec<Line> = log.iter().map(|l| Line::from(tail(l, log_area.width.saturating_sub(2) as usize))).collect();
            frame.render_widget(Paragraph::new(lines).style(Style::new().fg(Color::Yellow)).block(Block::bordered().title(" Messages ")), log_area);
        }

        match &self.screen {
            Screen::Form => self.draw_form(frame, main, true),
            Screen::Confirm(job) => {
                self.draw_form(frame, main, false);
                draw_confirm(frame, main, job);
            }
            Screen::Running(running) => self.draw_running(frame, main, running),
            Screen::Done(outcome) => draw_outcome(frame, main, outcome),
        }
    }

    fn draw_form(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        let block = panel(format!(" WorldPruner {} ", env!("CARGO_PKG_VERSION")));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [rows, _, buttons, _, help] =
            Layout::vertical([Constraint::Length(FIELD_ROWS as u16), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
                .areas(inner);

        let focused = self.form.field();
        let highlight = Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan);
        let dim = Style::new().fg(Color::DarkGray);
        let error = Style::new().fg(Color::Red);
        let value_width = (inner.width as usize).saturating_sub(2 + LABEL_WIDTH + 1);

        let mut lines = Vec::new();
        for &field in &FIELDS[..FIELD_ROWS] {
            let is_focused = field == focused;
            let style = if is_focused { highlight } else { Style::new() };
            let mut spans = vec![Span::styled(if is_focused { "> " } else { "  " }, highlight)];
            if field.is_text() {
                spans.push(Span::styled(format!("{:<LABEL_WIDTH$} ", format!("{}:", field.label())), style));
                let value = self.form.text(field);
                let ignored = self.form.whitelist_only && matches!(field, Field::Time | Field::Radius);
                let placeholder = match field {
                    _ if ignored => "(ignored)",
                    Field::Dimensions => "(overworld)",
                    Field::Whitelist => "(none)",
                    _ => "",
                };
                let note = if field == Field::Threads && value.trim() == "0" { " (all cores)" } else { "" };
                if ignored || (value.is_empty() && !is_focused) {
                    spans.push(Span::styled(placeholder, dim));
                } else {
                    spans.push(Span::raw(tail(value, value_width)));
                }
                if is_focused && show_cursor {
                    let column = 2 + LABEL_WIDTH + 1 + tail(value, value_width).chars().count();
                    frame.set_cursor_position(Position::new(inner.x + column as u16, rows.y + lines.len() as u16));
                }
                spans.push(Span::styled(note, dim));
            } else {
                let checked = if field == Field::WhitelistOnly { self.form.whitelist_only } else { self.form.continue_on_error };
                spans.push(Span::styled(format!("[{}] {}", if checked { 'x' } else { ' ' }, field.label()), style));
            }
            if let Some(problem) = self.form.problem(field) {
                spans.push(Span::styled(format!("  ! {problem}"), error));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), rows);

        let mut button_spans = vec![Span::raw("  ")];
        for &field in &FIELDS[FIELD_ROWS..] {
            let style = if field == focused { highlight.add_modifier(Modifier::REVERSED) } else { Style::new() };
            button_spans.push(Span::styled(format!("[ {} ]", field.label()), style));
            button_spans.push(Span::raw("  "));
        }
        frame.render_widget(Paragraph::new(Line::from(button_spans)), buttons);

        let mut help_lines = vec![Line::styled(focused.help(), dim)];
        // the row may be too narrow for the whole message
        if let Some(problem) = self.form.problem(focused) {
            help_lines.push(Line::raw(""));
            help_lines.push(Line::styled(format!("{}: {problem}", focused.label()), error));
        }
        if let Some((hint, is_error)) = &self.form.hint {
            help_lines.push(Line::raw(""));
            help_lines.push(Line::styled(hint.clone(), if *is_error { error } else { Style::new() }));
        }
        frame.render_widget(Paragraph::new(help_lines).wrap(Wrap { trim: true }), help);
    }

    fn draw_running(&self, frame: &mut Frame, area: Rect, running: &RunningJob) {
        let block = panel(if running.dry_run { " Preview (dry run) " } else { " Pruning " });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [description, _, gauge, _, status] =
            Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        frame.render_widget(Paragraph::new(self.description.as_str()).wrap(Wrap { trim: true }), description);

        let (message, done, total) = running.progress.snapshot();
        let ratio = if total == 0 { 0.0 } else { done as f64 / total as f64 };
        let widget = Gauge::default().gauge_style(Style::new().fg(Color::Cyan)).ratio(ratio.clamp(0.0, 1.0)).label(format!("{message}: {done}/{total}"));
        frame.render_widget(widget, gauge);

        let text = if running.is_cancelling() {
            "Stopping after the files that are being processed...".to_string()
        } else {
            // the number of files to rewrite is only known after the scan,
            // so the estimate is for the step that is running
            let eta = running.progress.eta().map_or("estimating...".to_string(), format_clock);
            format!("Elapsed: {}    ETA for this step: {eta}", format_clock(running.started.elapsed()))
        };
        frame.render_widget(Paragraph::new(text), status);
    }
}

/// A bordered block with a title and a little room on the sides.
fn panel<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::bordered().title(title).padding(Padding::horizontal(1))
}

fn draw_outcome(frame: &mut Frame, area: Rect, outcome: &Outcome) {
    let (title, style) = match (outcome.is_error, outcome.was_dry_run) {
        (true, _) => (" Error ", Style::new().fg(Color::Red)),
        (false, true) => (" Preview - nothing was changed ", Style::new()),
        (false, false) => (" Done ", Style::new()),
    };
    let widget = Paragraph::new(outcome.text.as_str()).style(style).wrap(Wrap { trim: false }).scroll((outcome.scroll, 0)).block(panel(title));
    frame.render_widget(widget, area);
}

fn draw_confirm(frame: &mut Frame, area: Rect, job: &Job) {
    let width = area.width.saturating_sub(4).min(72);
    let height = area.height.min(11);
    let popup = Rect::new(area.x + (area.width - width) / 2, area.y + (area.height - height) / 2, width, height);
    let warning = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    let dimensions = job.dimensions.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ");
    let lines = vec![
        Line::styled("You are about to prune chunks from your world.", warning),
        Line::styled("This can NOT be undone - make sure you have a backup!", warning),
        Line::raw(""),
        Line::raw(format!("World: {} ({dimensions})", job.world.display())),
        Line::raw(describe(&job.options)),
        Line::raw(""),
        Line::raw("Press y to prune, n to cancel."),
    ];
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::bordered().title(" Start pruning? ").border_style(warning)), popup);
}

fn event_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()>
where
    io::Error: From<B::Error>,
{
    while !app.quit {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Paste(text) => app.on_paste(&text),
                _ => {}
            }
        }
        app.tick();
    }
    Ok(())
}

/// Run the terminal interface until the user quits.
pub(crate) fn run(prefill: Prefill) -> ExitCode {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("error: the terminal interface needs an interactive terminal");
        return ExitCode::FAILURE;
    }
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(e) => {
            eprintln!("error: failed to set up the terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    capture_console_log(true);
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let result = event_loop(&mut terminal, &mut App::new(prefill));
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    capture_console_log(false);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::fs;
    use worldpruner::nbt::write::Compound;
    use worldpruner::pos::RegionPos;
    use worldpruner::region::testing::{Storage, TempDir, read_chunk, write_region};

    fn prefill(world: &str) -> Prefill {
        Prefill {
            world: world.to_string(),
            dimensions: String::new(),
            time: "5 minutes".to_string(),
            radius: 0,
            whitelist: String::new(),
            whitelist_only: false,
            continue_on_error: false,
            threads: 2,
            force: false,
        }
    }

    /// A world with one inhabited chunk (slot 0) and one untouched chunk (slot 1).
    fn test_world(label: &str) -> TempDir {
        let dir = TempDir::new(label);
        let chunk = |time: i64| Compound::new().long("InhabitedTime", time).compound("structures", Compound::new()).into_root();
        write_region(&dir.path().join("region"), RegionPos::new(0, 0), &[(0, chunk(100_000), Storage::Zlib), (1, chunk(0), Storage::Zlib)]);
        fs::write(dir.path().join("level.dat"), b"").unwrap();
        dir
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        buffer.content.chunks(width).map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn wait_until_done(app: &mut App) {
        for _ in 0..2000 {
            app.tick();
            if matches!(app.screen, Screen::Done(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the job did not finish");
    }

    #[test]
    fn typing_and_navigation() {
        let mut app = App::new(prefill(""));
        assert_eq!(app.form.field(), Field::World);
        type_text(&mut app, "/srv/world");
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.form.world, "/srv/worl");
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.form.world, "");
        app.on_paste("/pasted/path\n");
        assert_eq!(app.form.world, "/pasted/path");

        app.on_key(key(KeyCode::Down));
        assert_eq!(app.form.field(), Field::Dimensions);
        type_text(&mut app, "the_nether, the_end");
        assert_eq!(app.form.dimension_ids(), ["the_nether", "the_end"]);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.form.field(), Field::Time);
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.form.field(), Field::Quit, "navigation wraps around");
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.form.field(), Field::Prune);
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.form.field(), Field::Quit);
        // letters do nothing on buttons and check boxes
        type_text(&mut app, "xyz");
        app.on_key(key(KeyCode::BackTab));
        app.on_key(key(KeyCode::BackTab));
        app.on_key(key(KeyCode::BackTab));
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.form.field(), Field::ContinueOnError);
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.form.continue_on_error);
        app.on_key(key(KeyCode::Enter));
        assert!(!app.form.continue_on_error);
        assert!(!app.quit);
        app.on_key(key(KeyCode::Esc));
        assert!(app.quit);
    }

    #[test]
    fn validation() {
        let world = test_world("tui-validation");
        let mut form = Form::new(&prefill(""));
        assert_eq!(form.problem(Field::World).as_deref(), Some("required"));
        assert!(form.job(true, false).unwrap_err().starts_with("World: required"));
        form.world = world.path().join("missing").display().to_string();
        assert_eq!(form.problem(Field::World).as_deref(), Some("not a folder"));
        form.world = world.path().join("region").display().to_string();
        assert!(form.problem(Field::World).unwrap().contains("no region folder"));
        form.world = world.path().display().to_string();
        assert_eq!(form.problem(Field::World), None);

        form.dimensions = "the_nether".to_string();
        assert_eq!(form.problem(Field::World), None);
        assert!(form.problem(Field::Dimensions).unwrap().contains("minecraft:the_nether"));
        form.dimensions = "all".to_string();
        assert_eq!(form.problem(Field::Dimensions), None);

        form.time = "soon".to_string();
        assert!(form.problem(Field::Time).is_some());
        form.radius = "129".to_string();
        assert!(form.problem(Field::Radius).is_some());
        form.threads = "many".to_string();
        assert!(form.problem(Field::Threads).is_some());
        assert!(form.job(false, false).unwrap_err().starts_with("InhabitedTime:"));

        // whitelist-only ignores time and radius but needs a whitelist
        form.whitelist_only = true;
        form.threads = "0".to_string();
        assert_eq!(form.problem(Field::Time), None);
        assert_eq!(form.problem(Field::Radius), None);
        assert!(form.problem(Field::Whitelist).unwrap().contains("required"));
        form.whitelist = world.path().join("nope.csv").display().to_string();
        assert_eq!(form.problem(Field::Whitelist).as_deref(), Some("not a file"));
        let csv = world.path().join("keep.csv");
        fs::write(&csv, "0;0;0;0\n").unwrap();
        form.whitelist = csv.display().to_string();
        let job = form.job(false, true).unwrap();
        assert!(job.options.whitelist_only && job.force && !job.options.dry_run);
        assert_eq!((job.options.inhabited_time, job.options.radius), (0, 0));
        fs::write(&csv, "not a selection\n").unwrap();
        assert!(form.job(false, false).unwrap_err().starts_with("Whitelist:"));

        form.whitelist_only = false;
        form.whitelist.clear();
        form.time = "1h".to_string();
        form.radius = "3".to_string();
        let job = form.job(true, false).unwrap();
        assert_eq!((job.options.inhabited_time, job.options.radius, job.options.threads), (72_000, 3, 0));
        assert!(job.options.dry_run);
        assert_eq!(job.dimensions.len(), 1);
    }

    #[test]
    fn path_completion() {
        let dir = TempDir::new("tui-complete");
        for name in ["world", "world_nether", "backups", ".hidden"] {
            fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        fs::write(dir.path().join("whitelist.csv"), "").unwrap();
        let base = format!("{}/", dir.path().display());

        // unique match: completed, directories get a trailing slash
        assert_eq!(complete_path(&format!("{base}b"), true), (format!("{base}backups/"), vec!["backups/".to_string()]));
        // several matches: extended to the common prefix, candidates listed
        let (completed, candidates) = complete_path(&format!("{base}w"), true);
        assert_eq!(completed, format!("{base}world"));
        assert_eq!(candidates, ["world/", "world_nether/"]);
        // files are offered unless only folders make sense
        let (completed, candidates) = complete_path(&format!("{base}wh"), false);
        assert_eq!((completed, candidates.len()), (format!("{base}whitelist.csv"), 1));
        assert_eq!(complete_path(&format!("{base}wh"), true).1.len(), 0);
        // hidden entries only when asked for, nothing for unknown folders
        assert_eq!(complete_path(&base, true).1, ["backups/", "world/", "world_nether/"]);
        assert_eq!(complete_path(&format!("{base}."), true).1, [".hidden/"]);
        assert_eq!(complete_path(&format!("{base}nope/x"), true), (format!("{base}nope/x"), vec![]));

        let mut app = App::new(prefill(&format!("{base}w")));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.form.world, format!("{base}world"));
        assert_eq!(app.form.hint, Some(("world/  world_nether/".to_string(), false)));
        assert_eq!(app.form.field(), Field::World, "Tab completes instead of moving on");
        type_text(&mut app, "_");
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.form.world, format!("{base}world_nether/"));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.form.field(), Field::Dimensions, "Tab moves on when there is nothing to complete");
    }

    #[test]
    fn helpers() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("/a/very/long/path", 8), "\u{2026}ng/path");
        assert_eq!(expand_home("/abs"), "/abs");
        assert_eq!(expand_home("~user/x"), "~user/x");
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expand_home("~/x"), format!("{home}/x"));
            assert_eq!(expand_home("~"), home);
        }
    }

    #[test]
    fn renders_the_form() {
        let mut app = App::new(prefill(""));
        let screen = render(&app);
        for expected in [
            "WorldPruner",
            "> World:",
            "! required",
            "Dimensions:    (overworld)",
            "InhabitedTime: 5 minutes",
            "[ ] Continue on errors",
            "[ Preview (dry run) ]",
            "F10 prune",
            "Esc quit",
            "World: required",
        ] {
            assert!(screen.contains(expected), "missing {expected:?} in:\n{screen}");
        }
        // an invalid form explains itself instead of starting
        app.on_key(key(KeyCode::F(10)));
        assert!(matches!(app.screen, Screen::Form));
        assert!(render(&app).contains("World: required"));
        // tiny terminals must not panic
        for (w, h) in [(1, 1), (20, 5), (40, 12)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
        }
    }

    #[test]
    fn preview_then_prune() {
        let world = test_world("tui-flow");
        let region = world.path().join("region/r.0.0.mca");
        let before = fs::read(&region).unwrap();
        let mut app = App::new(prefill(&world.path().display().to_string()));

        // F2 runs a dry run that changes nothing
        app.on_key(key(KeyCode::F(2)));
        assert!(matches!(app.screen, Screen::Running(_)));
        let screen = render(&app);
        assert!(screen.contains("Preview (dry run)") && screen.contains("Elapsed: 0:00") && screen.contains("ETA for this step:"), "{screen}");
        wait_until_done(&mut app);
        let screen = render(&app);
        assert!(screen.contains("nothing was changed") && screen.contains("would remove 1 chunks"), "{screen}");
        assert_eq!(fs::read(&region).unwrap(), before);

        // "p" asks for confirmation; "n" goes back without touching anything
        app.on_key(key(KeyCode::Char('p')));
        assert!(matches!(app.screen, Screen::Confirm(_)));
        let screen = render(&app);
        assert!(screen.contains("Start pruning?") && screen.contains("NOT be undone"), "{screen}");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.screen, Screen::Confirm(_)), "Enter must not confirm");
        app.on_key(key(KeyCode::Char('n')));
        assert!(matches!(app.screen, Screen::Form));
        assert_eq!(fs::read(&region).unwrap(), before);

        // the Prune button, confirmed with "y", prunes
        while app.form.field() != Field::Prune {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.screen, Screen::Confirm(_)));
        app.on_key(key(KeyCode::Char('y')));
        wait_until_done(&mut app);
        let screen = render(&app);
        assert!(screen.contains("Done") && screen.contains("removed 1 chunks"), "{screen}");
        assert!(read_chunk(&region, 0).is_some());
        assert!(read_chunk(&region, 1).is_none());
        // after a real run there is nothing to confirm with "p"
        app.on_key(key(KeyCode::Char('p')));
        assert!(matches!(app.screen, Screen::Done(_)));
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.screen, Screen::Form));
    }

    #[test]
    fn errors_are_shown_and_cancel_works() {
        let world = test_world("tui-error");
        let region = world.path().join("region/r.0.0.mca");
        // damage the location table so the scan fails
        let mut bytes = fs::read(&region).unwrap();
        bytes[..4096].fill(0xFF);
        fs::write(&region, &bytes).unwrap();
        let mut app = App::new(prefill(&world.path().display().to_string()));
        app.on_key(key(KeyCode::F(2)));
        wait_until_done(&mut app);
        let Screen::Done(outcome) = &app.screen else { unreachable!() };
        assert!(outcome.is_error && outcome.text.contains("failed to read chunk"), "{}", outcome.text);
        assert!(render(&app).contains("Error"));
        // no "prune now" after a failed preview
        app.on_key(key(KeyCode::Char('p')));
        assert!(matches!(app.screen, Screen::Done(_)));
        assert_eq!(fs::read(&region).unwrap(), bytes);

        // cancelling is requested through the running job
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::F(2)));
        if let Screen::Running(running) = &app.screen {
            assert!(!running.is_cancelling());
        }
        app.on_key(key(KeyCode::Esc));
        if let Screen::Running(running) = &app.screen {
            assert!(running.is_cancelling());
        }
        wait_until_done(&mut app);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }
}
