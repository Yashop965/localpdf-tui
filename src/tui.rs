//! Terminal lifecycle for LocalPDF-TUI: raw mode, alternate screen,
//! panic-safe restoration, and the event loop that feeds `App`.

use crossterm::cursor::{Hide, Show};
use crossterm::event::{Event, KeyCode, KeyModifiers, KeyEvent};
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
pub const OPS: &[&str] = &["Merge", "Split", "Compress"];
pub const OPS_DESC: &[&str] = &[
    "concatenate the whole file queue into one document",
    "extract page ranges from the highlighted queue file",
    "reduce file size — planned for v0.2",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Queue,
    Ops,
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
    pub split_spec: String,
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
            split_spec: String::new(),
            log: VecDeque::new(),
            sample_counter: 0,
            should_quit: false,
        };
        app.set_log(
            StatusKind::Info,
            "ready — [a] add sample · [←][→] focus · [↑][↓] move · [enter] run · [q] quit",
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
                if self.queue.is_empty() {
                    self.set_log(StatusKind::Error, "no file to split — press [a] to queue one first");
                    return;
                }
                let idx = self.q_index.min(self.queue.len() - 1);
                let source = self.queue[idx].path.clone();
                let spec = self.split_spec.trim();
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
            _ => self.set_log(StatusKind::Error, "compress is not in the v0.1 engine yet"),
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

    pub fn handle_key(&mut self, key: KeyEvent) {
        let plain = key.modifiers == KeyModifiers::NONE;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') if plain => self.should_quit = true,
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('a') if plain => self.add_sample(),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Left | KeyCode::BackTab => self.switch_focus(Panel::Queue),
            KeyCode::Right | KeyCode::Tab => self.switch_focus(Panel::Ops),
            KeyCode::Enter if self.focus == Panel::Ops => self.run_selected_op(),
            KeyCode::Backspace if self.focus == Panel::Ops => {
                self.split_spec.pop();
            }
            KeyCode::Char(c) if self.focus == Panel::Ops && "0123456789,-".contains(c) && self.split_spec.len() < 24 => {
                self.split_spec.push(c);
            }
            _ => {}
        }
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
                app.handle_key(key);
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
