//! Desktop front end, the counterpart of the original Swing window.
//!
//! Built only with the `gui` cargo feature. Starting the program without
//! arguments opens this window.

use crate::describe;
use crate::job::{Job, RunningJob, format_clock};
use eframe::egui;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use worldpruner::duration::parse_duration;
use worldpruner::pruner::{MAX_RADIUS, PruneOptions, TICKS_PER_SECOND};
use worldpruner::selection::Selection;
use worldpruner::world::{Dimension, resolve_dimensions};

const WORLD_HELP: &str = "The folder containing your world files.\n\n\
    Select the world folder itself to prune the Overworld. For other dimensions select their folder: \
    DIM-1 (The Nether) or DIM1 (The End) in worlds saved by 1.21 and older, or the folder below \
    \"dimensions\" (e.g. dimensions/minecraft/the_nether) in worlds saved by 26.1 and newer.";
const TIME_HELP: &str = "The minimum InhabitedTime of a chunk for WorldPruner to not delete it.\n\n\
    Example: 1 day 3 hours 5 minutes 30 seconds\n\n\
    InhabitedTime is the accumulated time of all players who have spent any amount of time in that chunk.";
const RADIUS_HELP: &str = "The radius of additional chunks preserved around matching chunks.\n\n\
    The maximum allowed radius is 128. The radius only affects chunks that match the InhabitedTime condition.";
const WHITELIST_HELP: &str = "The whitelist contains chunks and regions that need to be kept in any case.\n\n\
    Whitelists can be created in MCA Selector by using Selection --> Export selection as .csv.";

struct App {
    world: String,
    time: String,
    radius: u32,
    whitelist: String,
    continue_on_error: bool,
    dry_run: bool,
    confirm: Option<Job>,
    running: Option<RunningJob>,
    /// Outcome of the last run: text and whether it is an error.
    outcome: Option<(String, bool)>,
}

impl App {
    fn new() -> Self {
        App {
            world: String::new(),
            time: "5 minutes".to_string(),
            radius: 0,
            whitelist: String::new(),
            continue_on_error: false,
            dry_run: false,
            confirm: None,
            running: None,
            outcome: None,
        }
    }

    fn dimensions(&self) -> Result<Vec<Dimension>, String> {
        if self.world.trim().is_empty() {
            return Err("Select a world folder".to_string());
        }
        resolve_dimensions(Path::new(self.world.trim()), &[]).map_err(|_| "Not a valid Minecraft world folder".to_string())
    }

    fn ticks(&self) -> Result<i64, String> {
        let seconds = parse_duration(&self.time).map_err(|e| e.to_string())?;
        seconds.checked_mul(TICKS_PER_SECOND).and_then(|t| i64::try_from(t).ok()).ok_or_else(|| "The duration is too long".to_string())
    }

    fn whitelist_path(&self) -> Result<Option<PathBuf>, String> {
        let text = self.whitelist.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let path = PathBuf::from(text);
        if path.is_file() { Ok(Some(path)) } else { Err("Not a file".to_string()) }
    }

    /// Validate the inputs and ask for confirmation.
    fn prepare(&mut self, whitelist_only: bool) {
        let job = (|| -> Result<Job, String> {
            let dimensions = self.dimensions()?;
            let whitelist = match self.whitelist_path()? {
                Some(path) => Selection::from_csv_file(&path).map_err(|e| format!("Invalid whitelist: {e:#}"))?,
                None => Selection::new(),
            };
            let options = PruneOptions {
                inhabited_time: if whitelist_only { 0 } else { self.ticks()? },
                radius: if whitelist_only { 0 } else { self.radius },
                whitelist,
                whitelist_only,
                continue_on_error: self.continue_on_error,
                dry_run: self.dry_run,
                threads: 0,
            };
            Ok(Job { dimensions, options, world: PathBuf::from(self.world.trim()), force: false })
        })();
        match job {
            Ok(job) => self.confirm = Some(job),
            Err(message) => self.outcome = Some((message, true)),
        }
    }

    fn start(&mut self, job: Job, ctx: &egui::Context) {
        let ctx = ctx.clone();
        self.outcome = None;
        self.running = Some(RunningJob::start(job, move || ctx.request_repaint()));
    }
}

/// A label with an info marker that explains the field on hover.
fn field_label(ui: &mut egui::Ui, text: &str, help: &str) {
    ui.label(format!("{text}: \u{24D8}")).on_hover_text(help);
}

fn status(ui: &mut egui::Ui, problem: Option<&String>) {
    if let Some(problem) = problem {
        ui.colored_label(ui.visuals().error_fg_color, "\u{26A0}").on_hover_text(problem);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // collect the result of a finished run
        if let Some(running) = &self.running
            && let Some(result) = running.poll()
        {
            self.outcome = Some(match result {
                Ok(text) => (text, false),
                Err(message) => (message, true),
            });
            self.running = None;
        }

        let busy = self.running.is_some() || self.confirm.is_some();
        let world_problem = self.dimensions().err();
        let time_problem = self.ticks().err();
        let whitelist_problem = self.whitelist_path().err();
        let has_whitelist = !self.whitelist.trim().is_empty();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("options").num_columns(3).spacing([8.0, 8.0]).show(ui, |ui| {
                    field_label(ui, "World", WORLD_HELP);
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.world).desired_width(360.0));
                        if ui.button("Open World").clicked()
                            && let Some(folder) = rfd::FileDialog::new().set_title("Open World").pick_folder()
                        {
                            self.world = folder.display().to_string();
                        }
                    });
                    status(ui, world_problem.as_ref());
                    ui.end_row();

                    field_label(ui, "InhabitedTime", TIME_HELP);
                    ui.add(egui::TextEdit::singleline(&mut self.time).desired_width(360.0));
                    status(ui, time_problem.as_ref());
                    ui.end_row();

                    field_label(ui, "Radius", RADIUS_HELP);
                    ui.add(egui::DragValue::new(&mut self.radius).range(0..=MAX_RADIUS));
                    ui.end_row();

                    field_label(ui, "Whitelist", WHITELIST_HELP);
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.whitelist).desired_width(360.0));
                        if ui.button("Open Whitelist").clicked()
                            && let Some(file) = rfd::FileDialog::new().set_title("Open Whitelist").add_filter("Selection", &["csv"]).pick_file()
                        {
                            self.whitelist = file.display().to_string();
                        }
                    });
                    status(ui, whitelist_problem.as_ref());
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.checkbox(&mut self.continue_on_error, "Continue if a file or chunk cannot be processed (unreadable chunks are kept)");
                ui.checkbox(&mut self.dry_run, "Dry run: only report what would be pruned");
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    let can_prune = world_problem.is_none() && time_problem.is_none() && whitelist_problem.is_none();
                    let prune_button = ui
                        .add_enabled(can_prune, egui::Button::new("Prune"))
                        .on_hover_text("Prune everything below InhabitedTime with the given radius and except the whitelist");
                    if prune_button.clicked() {
                        self.prepare(false);
                    }
                    let can_prune_whitelist = world_problem.is_none() && whitelist_problem.is_none() && has_whitelist;
                    let whitelist_button = ui
                        .add_enabled(can_prune_whitelist, egui::Button::new("Prune except whitelist"))
                        .on_hover_text("Prune everything except the whitelist, ignoring InhabitedTime and radius");
                    if whitelist_button.clicked() {
                        self.prepare(true);
                    }
                });
            });

            ui.add_space(8.0);
            if let Some(running) = &self.running {
                let (message, done, total) = running.progress.snapshot();
                let fraction = if total == 0 { 0.0 } else { done as f32 / total as f32 };
                let eta = running.progress.eta().map_or(String::new(), |eta| format!(" - about {} left", format_clock(eta)));
                ui.add(egui::ProgressBar::new(fraction).text(format!("{message}: {done}/{total}{eta}")));
                let cancelling = running.is_cancelling();
                if ui.add_enabled(!cancelling, egui::Button::new(if cancelling { "Stopping..." } else { "Cancel" })).clicked() {
                    running.cancel();
                }
            }
            if let Some((text, is_error)) = &self.outcome {
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let color = if *is_error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                    ui.label(egui::RichText::new(text).monospace().color(color));
                });
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                ui.horizontal(|ui| {
                    ui.hyperlink_to("Source code", env!("CARGO_PKG_REPOSITORY"));
                    ui.label(format!("WorldPruner {}", env!("CARGO_PKG_VERSION")));
                });
            });
        });

        let mut decision = None;
        if let Some(job) = &self.confirm {
            egui::Window::new("Start pruning?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                if job.options.dry_run {
                    ui.label("Dry run: nothing will be changed.");
                } else {
                    ui.strong("You are about to prune chunks from your world.");
                    ui.strong("This process can NOT be undone, make sure you backed up your world!");
                }
                ui.add_space(4.0);
                ui.label(describe(&job.options));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        }
        if let Some(confirmed) = decision
            && let Some(job) = self.confirm.take()
            && confirmed
        {
            self.start(job, ctx);
        }
    }
}

/// Open the window and block until it is closed.
pub fn run() -> ExitCode {
    let mut viewport = egui::ViewportBuilder::default().with_title("WorldPruner").with_inner_size([640.0, 420.0]).with_min_inner_size([560.0, 300.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/128x128.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    match eframe::run_native("WorldPruner", options, Box::new(|_| Ok(Box::new(App::new())))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: failed to open the window: {e}");
            ExitCode::FAILURE
        }
    }
}
