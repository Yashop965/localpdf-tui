//! Terminal lifecycle for LocalPDF-TUI: raw mode, alternate screen,
//! panic-safe restoration, and the event loop that feeds `App`.

use crossterm::cursor::{Hide, Show};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use std::collections::VecDeque;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::engine::{Engine, parse_ranges};
use crate::error::{AppError, AppResult};
use crate::ui;

/// Which operations the dashboard offers, in display order.
pub const OPS: &[&str] = &[
    "Merge",
    "Split",
    "Compress",
    "Rotate",
    "Delete",
    "Info",
    "Watermark",
    "PageNum",
    "Metadata",
    "Extract Text",
    "Extract Images",
];
pub const OPS_DESC: &[&str] = &[
    "concatenate the whole file queue into one document",
    "extract page ranges from the highlighted file",
    "rebuild + flate-compress the highlighted file",
    "set /Rotate on pages — input `ranges:degrees`, e.g. 1,3:90",
    "remove page ranges from the highlighted file",
    "report pages, size, encryption and metadata",
    "stamp a diagonal text watermark on every page",
    "print the 1-based page number on each page",
    "set one Info key — input `Key=Value`, e.g. Title=Q3",
    "pull text out of pages — input ranges optional (blank = all)",
    "pull embedded images into `<name>-images/`",
];

/// Operations whose context input line is a page range spec.
pub const HAS_RANGE_INPUT: &[usize] = &[1, 4, 9]; // Split, Delete, Extract Text
/// The rotate op takes `ranges:degrees` in the input line.
pub const ROTATE_INPUT: usize = 3;
/// The watermark and metadata ops take free-text input lines.
pub const TEXT_INPUT: &[usize] = &[6, 8]; // Watermark, Metadata

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Queue,
    Ops,
}

/// Top-level input mode: either the dashboard, or the add-file path
/// prompt started with `[f]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    AddFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Ok,
    Error,
}

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub path: PathBuf,
    pub size_kb: u64,
}

/// All interactive state. The render pass (`ui::render`) is a pure reader of
/// this struct; all mutation happens in `handle_key`, so the draw loop never
/// allocates beyond the widget text it pre-computes here.
pub struct App {
    pub engine: Engine,
    pub queue: Vec<QueueEntry>,
    pub queue_lines: Vec<String>,
    pub q_index: usize,
    pub ops_index: usize,
    pub focus: Panel,
    pub ctx_input: String,
    pub mode: Mode,
    pub file_input: String,
    pub log: VecDeque<String>,
    sample_counter: u32,
    should_quit: bool,
}

impl App {
    pub fn new(engine: Engine) -> Self {
        let mut app = Self {
            engine,
            queue: Vec::new(),
            queue_lines: Vec::new(),
            q_index: 0,
            ops_index: 0,
            focus: Panel::Queue,
            ctx_input: String::new(),
            mode: Mode::Normal,
            file_input: String::new(),
            log: VecDeque::new(),
            sample_counter: 0,
            should_quit: false,
        };
        app.set_log(
            StatusKind::Info,
            "ready — [a] add sample · [f] add file · [←][→] focus · [↑][↓] move · [enter] run · [q] quit",
        );
        app
    }

    fn set_log(&mut self, kind: StatusKind, msg: &str) {
        let mark = match kind {
            StatusKind::Ok => "✓",
            StatusKind::Error => "✗",
            StatusKind::Info => "·",
        };
        self.log.push_back(format!("{mark} {msg}"));
        while self.log.len() > 3 {
            self.log.pop_front();
        }
    }

    fn rebuild_queue_lines(&mut self) {
        self.queue_lines = self
            .queue
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let name = e
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("{:>2}. {}  ·  {} KB", i + 1, name, e.size_kb)
            })
            .collect();
    }

    /// `[a]` — generate a real one-page sample PDF in the workspace and queue
    /// it, so merge/split can be demonstrated with zero external input.
    fn add_sample(&mut self) {
        self.sample_counter += 1;
        let n = self.sample_counter;
        let name = format!("sample_{n}.pdf");
        match self.engine.generate_sample(&name, &format!("Sample PDF {n}")) {
            Ok(path) => {
                let size_kb = path.metadata().map(|m| m.len() / 1024).unwrap_or(0);
                self.queue.push(QueueEntry { path, size_kb });
                self.rebuild_queue_lines();
                self.q_index = self.queue.len() - 1;
                self.focus = Panel::Queue;
                self.set_log(StatusKind::Ok, &format!("queued {name} ({size_kb} KB)"));
            }
            Err(e) => self.set_log(StatusKind::Error, &format!("sample generation failed: {e}")),
        }
    }

    /// `[f]` — start the add-file prompt: type any path (workspace-relative
    /// or absolute) into the bottom input line, confirm with [enter].
    fn start_add_file(&mut self) {
        self.mode = Mode::AddFile;
        self.file_input.clear();
        self.focus = Panel::Queue;
        self.set_log(
            StatusKind::Info,
            "add file: type a path (relative to the workspace, or absolute) — [enter] add · [esc] cancel",
        );
    }

    /// Commit the typed path: resolve it through the sandbox engine and
    /// queue it. Rejected paths (outside the workspace, missing) log an
    /// error and drop back to the dashboard.
    fn commit_add_file(&mut self) {
        let spec = self.file_input.trim().to_string();
        self.mode = Mode::Normal;
        self.file_input.clear();
        if spec.is_empty() {
            self.set_log(StatusKind::Error, "empty path — nothing added");
            return;
        }
        let path = match self.engine.resolve(Path::new(&spec)) {
            Ok(p) => p,
            Err(e) => {
                self.set_log(StatusKind::Error, &format!("cannot add '{spec}': {e}"));
                return;
            }
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size_kb = path.metadata().map(|m| m.len() / 1024).unwrap_or(0);
        self.queue.push(QueueEntry { path, size_kb });
        self.rebuild_queue_lines();
        self.q_index = self.queue.len() - 1;
        self.focus = Panel::Queue;
        self.set_log(StatusKind::Ok, &format!("added {name} ({size_kb} KB)"));
    }

    /// The highlighted queue file (bounds-checked) — the source for every
    /// single-file operation.
    fn active_source(&self) -> Option<&PathBuf> {
        if self.queue.is_empty() {
            return None;
        }
        let idx = self.q_index.min(self.queue.len() - 1);
        Some(&self.queue[idx].path)
    }

    /// Parse the rotate input line `ranges:degrees` (degrees optional,
    /// defaults to 90).
    fn parse_rotate_input(&self) -> Result<(String, u32), String> {
        let spec = self.ctx_input.trim();
        if spec.is_empty() {
            return Err("type a page range for rotate, e.g. 1,3:90".into());
        }
        let (range_part, deg_part) = match spec.split_once(':') {
            Some((r, d)) => (r.trim(), d.trim()),
            None => (spec, "90"),
        };
        if range_part.is_empty() {
            return Err("missing page range before ':'".into());
        }
        let degrees = deg_part
            .parse::<u32>()
            .map_err(|_| format!("'{}' is not a number (use 90, 180, 270)", deg_part))?;
        Ok((range_part.to_string(), degrees))
    }

    fn run_selected_op(&mut self) {
        match OPS[self.ops_index] {
            "Merge" => {
                if self.queue.is_empty() {
                    self.set_log(StatusKind::Error, "queue is empty — press [a] to add a sample file");
                    return;
                }
                let paths: Vec<PathBuf> = self.queue.iter().map(|e| e.path.clone()).collect();
                let n = paths.len();
                match self.engine.merge(&paths, Path::new("merged.pdf")) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!("merged {n} file{} → {} ({} pages)", if n == 1 { "" } else { "s" }, r.output_file, r.pages),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("merge failed: {e}")),
                }
            }
            "Split" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                let spec = self.ctx_input.trim();
                if spec.is_empty() {
                    self.set_log(StatusKind::Error, "type a page range for split, e.g. 1-3,5");
                    return;
                }
                let ranges = match parse_ranges(spec) {
                    Ok(r) => r,
                    Err(e) => {
                        self.set_log(StatusKind::Error, &e.to_string());
                        return;
                    }
                };
                match self.engine.split(&source, &ranges, Path::new("split.pdf")) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!("split '{spec}' → {} ({} pages)", r.output_file, r.pages),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("split failed: {e}")),
                }
            }
            "Compress" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                match self.engine.compress(&source, Path::new("compressed.pdf"), 9) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!("compressed → {} ({} pages)", r.output_file, r.pages),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("compress failed: {e}")),
                }
            }
            "Rotate" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                let (spec, degrees) = match self.parse_rotate_input() {
                    Ok(v) => v,
                    Err(msg) => {
                        self.set_log(StatusKind::Error, &msg);
                        return;
                    }
                };
                match self.engine.rotate(&source, &spec, degrees, Path::new("rotated.pdf")) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!("rotated '{spec}' by {degrees}° → {} ({} pages)", r.output_file, r.pages),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("rotate failed: {e}")),
                }
            }
            "Delete" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                let spec = self.ctx_input.trim();
                if spec.is_empty() {
                    self.set_log(StatusKind::Error, "type the page ranges to delete, e.g. 2,4-5");
                    return;
                }
                match self.engine.delete_pages(&source, spec, Path::new("trimmed.pdf")) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!("deleted '{spec}' → {} ({} pages kept)", r.output_file, r.pages),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("delete failed: {e}")),
                }
            }
            "Info" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                match self.engine.info(&source) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!(
                            "{}: {} pages · {} KB · {} · '{}'",
                            r.source_file, r.pages, r.file_size_bytes / 1024,
                            if r.encrypted { "encrypted" } else { "plain" },
                            r.title.clone().unwrap_or_default()
                        ),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("info failed: {e}")),
                }
            }
            "Watermark" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                let text = self.ctx_input.trim();
                if text.is_empty() {
                    self.set_log(StatusKind::Error, "type the watermark text first, e.g. DRAFT");
                    return;
                }
                match self.engine.watermark(&source, text, Path::new("watermarked.pdf")) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!("watermarked '{text}' → {} ({} pages)", r.output_file, r.pages),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("watermark failed: {e}")),
                }
            }
            "PageNum" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                match self.engine.page_numbers(&source, Path::new("numbered.pdf")) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!("numbered → {} ({} pages)", r.output_file, r.pages),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("page-numbers failed: {e}")),
                }
            }
            "Metadata" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                let spec = self.ctx_input.trim();
                let Some((key, value)) = spec
                    .split_once('=')
                    .and_then(|(k, v)| {
                        let k = k.trim();
                        (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
                    })
                else {
                    self.set_log(StatusKind::Error, "type a Key=Value pair, e.g. Title=Q3");
                    return;
                };
                let pairs = vec![(key.clone(), value.clone())];
                match self.engine.set_metadata(&source, &pairs, Path::new("meta.pdf")) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!("set {key}='{value}' → {} ({} pages)", r.output_file, r.pages),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("metadata failed: {e}")),
                }
            }
            "Extract Text" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                let spec = self.ctx_input.trim();
                let spec = if spec.is_empty() { None } else { Some(spec) };
                match self.engine.extract_text(&source, spec, None) {
                    Ok(r) => {
                        let preview = r
                            .text
                            .as_deref()
                            .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
                            .unwrap_or("");
                        self.set_log(
                            StatusKind::Ok,
                            &format!(
                                "extracted {} chars from {} page{} · '{}'",
                                r.characters,
                                r.pages,
                                if r.pages == 1 { "" } else { "s" },
                                preview.chars().take(28).collect::<String>()
                            ),
                        );
                    }
                    Err(e) => self.set_log(StatusKind::Error, &format!("extract-text failed: {e}")),
                }
            }
            "Extract Images" => {
                let source = match self.active_source() {
                    Some(s) => s.clone(),
                    None => {
                        self.set_log(StatusKind::Error, "no file selected — press [a] to queue one first");
                        return;
                    }
                };
                let out_dir = format!(
                    "{}-images",
                    source.file_stem().and_then(|s| s.to_str()).unwrap_or("out")
                );
                match self.engine.extract_images(&source, Path::new(&out_dir)) {
                    Ok(r) => self.set_log(
                        StatusKind::Ok,
                        &format!(
                            "pulled {} image{} → {} ({} unsupported)",
                            r.found,
                            if r.found == 1 { "" } else { "s" },
                            out_dir,
                            r.skipped
                        ),
                    ),
                    Err(e) => self.set_log(StatusKind::Error, &format!("extract-images failed: {e}")),
                }
            }
            _ => unreachable!("OPS must match OPS_DESC"),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            Panel::Queue => {
                if self.queue.is_empty() {
                    self.q_index = 0;
                    return;
                }
                if delta < 0 {
                    self.q_index = self.q_index.saturating_sub(1);
                } else {
                    self.q_index = self.q_index.saturating_add(1).min(self.queue.len() - 1);
                }
            }
            Panel::Ops => {
                if delta < 0 {
                    self.ops_index = self.ops_index.saturating_sub(1);
                } else {
                    self.ops_index = self.ops_index.saturating_add(1).min(OPS.len() - 1);
                }
            }
        }
    }

    fn switch_focus(&mut self, panel: Panel) {
        if self.focus == panel {
            return;
        }
        self.focus = panel;
        if panel == Panel::Queue && self.queue.is_empty() {
            self.q_index = 0;
        }
    }

    /// Whether the currently selected operation has a context input line.
    fn op_takes_input(&self) -> bool {
        HAS_RANGE_INPUT.contains(&self.ops_index)
            || self.ops_index == ROTATE_INPUT
            || TEXT_INPUT.contains(&self.ops_index)
    }

    /// Whether `c` is a valid context-input character for the current op.
    fn op_accepts(&self, c: char) -> bool {
        if !self.op_takes_input() || self.ctx_input.len() >= 24 {
            return false;
        }
        match self.ops_index {
            ROTATE_INPUT => {
                c.is_ascii_digit() || c == ',' || c == '-' || (c == ':' && !self.ctx_input.contains(':'))
            }
            6 | 8 => (c.is_ascii() && !c.is_ascii_control()) || c == ' ',
            _ => c.is_ascii_digit() || c == ',' || c == '-',
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // The add-file prompt consumes the keyboard until [enter]/[esc].
        if self.mode == Mode::AddFile {
            match key.code {
                KeyCode::Esc => self.start_cancel_add_file(),
                KeyCode::Enter => self.commit_add_file(),
                KeyCode::Backspace => {
                    self.file_input.pop();
                }
                KeyCode::Char(c)
                    if c.is_ascii()
                        && !c.is_ascii_control()
                        && self.file_input.len() < 160 =>
                {
                    self.file_input.push(c);
                }
                _ => {}
            }
            return;
        }

        let plain = key.modifiers == KeyModifiers::NONE;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') if plain => self.should_quit = true,
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('a') if plain => self.add_sample(),
            KeyCode::Char('f') if plain => self.start_add_file(),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Left | KeyCode::BackTab => self.switch_focus(Panel::Queue),
            KeyCode::Right | KeyCode::Tab => self.switch_focus(Panel::Ops),
            KeyCode::Enter if self.focus == Panel::Ops => self.run_selected_op(),
            KeyCode::Backspace if self.focus == Panel::Ops && self.op_takes_input() => {
                self.ctx_input.pop();
            }
            KeyCode::Char(c) if self.focus == Panel::Ops && self.op_accepts(c) => {
                self.ctx_input.push(c);
            }
            _ => {}
        }
    }

    /// [esc] inside the add-file prompt cancels it (not the app).
    fn start_cancel_add_file(&mut self) {
        self.mode = Mode::Normal;
        self.file_input.clear();
        self.set_log(StatusKind::Info, "add-file cancelled");
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }
}

/// Run the interactive dashboard. Owns the terminal for its whole lifetime
/// and guarantees the visible terminal is restored on both clean exit and
/// panic.
pub fn run(mut app: App) -> AppResult<()> {
    crossterm::terminal::enable_raw_mode().map_err(|e| AppError::Io("enable raw mode".into(), e))?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, Hide)
        .map_err(|e| AppError::Io("enter alternate screen".into(), e))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    terminal.clear()?;
    install_panic_hook();

    loop {
        terminal.draw(|f| ui::render(f, &mut app))?;
        if app.should_quit() {
            break;
        }
        if crossterm::event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = crossterm::event::read()? {
                // Windows reports every physical keypress as a Press *and*
                // a Release; acting on both double-fired every operation.
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    app.handle_key(key);
                }
            }
        }
    }

    // Reveal the cursor: crossterm's `Show` command (below) handles it; the
    // alternate screen is left via the same sequence on a fresh stdout.
    crossterm::execute!(stdout(), LeaveAlternateScreen, Show)
        .map_err(|e| AppError::Io("leave alternate screen".into(), e))?;
    crossterm::terminal::disable_raw_mode().map_err(|e| AppError::Io("disable raw mode".into(), e))?;
    Ok(())
}

/// If the event loop ever unwinds, drop the alternate screen and raw mode
/// before the original hook runs, so the user's shell is left intact.
fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen, Show);
        original_hook(info);
    }));
}
