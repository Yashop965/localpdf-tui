//! Visual layer for LocalPDF-TUI.
//!
//! One `render` entry point that lays out the dashboard: a title strip on
//! top, a left "File Queue" column, a right "Operations" column with the
//! split-range input, and a status log banner along the bottom. All text
//! content is pre-computed in `App`; this module only assembles widgets.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListState, Paragraph};
use ratatui::Frame;

use crate::tui::{App, Mode, OPS, OPS_DESC, Panel, ROTATE_INPUT};

/// Per-operation accent colors, indexed by position in `OPS`.
fn op_color(i: usize) -> Color {
    match i {
        0 | 1 => Color::Rgb(255, 159, 67), // merge / split — orange
        2 => Color::Rgb(86, 212, 145), // compress — green
        3 => Color::Rgb(199, 126, 255), // rotate — purple
        4 => Color::Rgb(248, 81, 93), // delete — red
        5 => Color::Rgb(79, 174, 255), // info — blue
        6 => Color::Rgb(245, 213, 71), // watermark — yellow
        7 => Color::Rgb(64, 196, 230), // page numbers — cyan
        8 => Color::Rgb(228, 108, 242), // metadata — magenta
        9 | 10 => Color::Rgb(108, 192, 151), // extract — teal
        _ => Color::White,
    }
}

/// Palette. Tweak here and the whole dashboard re-skins.
const ACCENT: Color = Color::Cyan;
const BORDER: Color = Color::Rgb(58, 75, 98);
const PANEL_BG: Color = Color::Rgb(13, 17, 23);
const SEL_BG: Color = Color::Rgb(34, 48, 64);
const DIM: Color = Color::Rgb(110, 118, 129);

fn panel_title(text: &str) -> Span<'_> {
    Span::styled(text, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
}

fn bordered_block(title_text: &str) -> Block<'_> {
    Block::bordered()
        .title(panel_title(title_text))
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL_BG))
}

/// Top level frame. `render` is called once per terminal refresh.
pub fn render(f: &mut Frame, app: &mut App) {
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(15),
        Constraint::Length(5),
    ])
    .split(f.area());

    render_title(f, app, outer[0]);

    let main = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(outer[1]);
    render_queue(f, main[0], app);
    render_ops(f, main[1], app);
    render_status(f, outer[2], app);
}

fn render_title(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let hint = "  [a] add sample · [f] add file · [←][→] focus · [↑][↓] move · [enter] run · [q] quit  ";
    let width = area.width.saturating_sub(hint.len() as u16);
    let mut spans = vec![
        Span::styled(" ◆ ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled("LocalPDF-TUI ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("v0.4", Style::default().fg(DIM).add_modifier(Modifier::BOLD)),
    ];
    if app.mode == Mode::AddFile {
        spans.push(Span::styled(
            " · typing path…",
            Style::default().fg(op_color(6)).add_modifier(Modifier::ITALIC),
        ));
    }
    spans.push(Span::styled(
        " · private pdf toolkit ",
        Style::default().fg(DIM),
    ));
    spans.push(Span::raw("·".repeat(width.max(1) as usize)));
    spans.push(Span::styled(hint, Style::default().fg(DIM)));
    let line = Line::from(spans);
    let widget = Paragraph::new(line).style(Style::default().bg(PANEL_BG));
    f.render_widget(widget, area);
}

fn render_queue(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let block = bordered_block(" FILE QUEUE ");
    if app.queue.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled("  empty", Style::default().fg(DIM))),
            Line::from(Span::styled(
                "  [a] queue a sample document · [f] add your own file",
                Style::default().fg(DIM),
            )),
        ])
        .block(block);
        f.render_widget(empty, area);
        return;
    }

    let total_kb: u64 = app.queue.iter().map(|e| e.size_kb).sum();
    // List fills the panel; a one-line footer reports the queue totals.
    let split = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(area);

    let mut state = ListState::default();
    state.select(Some(app.q_index));
    let list = List::new(app.queue_lines.clone())
        .block(block)
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().bg(SEL_BG).fg(Color::White).add_modifier(Modifier::BOLD));
    f.render_widget(list, split[0]);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled("  Σ ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{} file{} · {} KB", app.queue.len(), if app.queue.len() == 1 { "" } else { "s" }, total_kb),
            Style::default().fg(DIM),
        ),
        Span::styled("   [a] sample · [f] add file", Style::default().fg(DIM)),
    ]))
    .style(Style::default().bg(PANEL_BG));
    f.render_widget(footer, split[1]);
}

fn render_ops(f: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    // Two border lines + one line per operation.
    let split =
        Layout::vertical([Constraint::Length((OPS.len() + 2) as u16), Constraint::Length(1)])
            .split(area);

    let ops_block = bordered_block(" OPERATIONS ");
    let active = app.focus == Panel::Ops && app.mode == Mode::Normal;
    let lines: Vec<Line> = (0..OPS.len())
        .map(|i| {
            let marker = if active && i == app.ops_index { "▸" } else { " " };
            let selected = active && i == app.ops_index;
            let op_style = Style::default()
                .fg(if selected { Color::White } else { op_color(i) })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
            let desc_style = Style::default().fg(if selected {
                Color::Rgb(229, 231, 235)
            } else {
                Color::Rgb(139, 148, 158)
            });
            Line::from(vec![
                Span::styled(format!("{marker}  {}", OPS[i]), op_style),
                Span::raw("  "),
                Span::styled(OPS_DESC[i], desc_style),
            ])
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.ops_index));
    let list = List::new(lines)
        .block(ops_block)
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().bg(SEL_BG).fg(Color::White).add_modifier(Modifier::BOLD));
    f.render_widget(list, split[0]);

    // Context input line: label + editable value, active only for ops that
    // take input (ranges / rotate / free-text / metadata). In add-file mode
    // the same line becomes the file-path prompt.
    let op = app.ops_index;
    let label = if app.mode == Mode::AddFile {
        "ADD FILE PATH    "
    } else {
        match op {
            1 => "SPLIT RANGES    ",
            ROTATE_INPUT => "ROTATE RANGES:DEG",
            4 => "DELETE PAGES    ",
            6 => "WATERMARK TEXT  ",
            8 => "METADATA K=V    ",
            9 => "EXTRACT PAGES   ",
            _ => " CONTEXT        ",
        }
    };
    let value = if app.mode == Mode::AddFile {
        format!("[ {}▏ ]", app.file_input)
    } else {
        match op {
            1 | 4 | 9 => format!("[ {}▏ ]", app.ctx_input),
            ROTATE_INPUT | 6 | 8 => format!("[ {}▏ ]", app.ctx_input),
            _ => "[        ]".to_string(),
        }
    };
    let help = if app.mode == Mode::AddFile {
        "   workspace-relative or absolute — [enter] add · [esc] cancel"
    } else {
        match op {
            1 => "   1-based, e.g. 1-3,5 — applied to the highlighted file",
            ROTATE_INPUT => "   e.g. 1,3:90 — pages and angle for the highlighted file",
            4 => "   1-based, e.g. 2,4-5 — removes pages from the highlighted file",
            6 => "   stamped diagonally on every page of the highlighted file",
            8 => "   e.g. Title=Q3 Report — one Info key per run",
            9 => "   ranges optional — blank extracts every page",
            _ => "   no input — [enter] runs on the highlighted file",
        }
    };
    let input_mode_active = app.mode == Mode::AddFile;
    let label_style = Style::default()
        .fg(if input_mode_active { op_color(6) } else { ACCENT })
        .add_modifier(Modifier::BOLD);
    let value_style =
        Style::default().fg(if input_mode_active { Color::White } else { DIM });
    let input = Paragraph::new(Line::from(vec![
        Span::styled(label, label_style),
        Span::styled(value, value_style),
        Span::styled(help, Style::default().fg(DIM)),
    ]))
    .style(Style::default().bg(PANEL_BG));
    f.render_widget(input, split[1]);
}

fn render_status(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let block = bordered_block(" STATUS LOG ");
    let lines: Vec<Line> = app
        .log
        .iter()
        .map(|entry| {
            let (mark, rest) = entry.split_once(' ').unwrap_or((entry, ""));
            let color = if mark.starts_with('✓') {
                Color::Green
            } else if mark.starts_with('✗') {
                Color::Red
            } else {
                ACCENT
            };
            Line::from(vec![
                Span::styled(mark, Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(rest, Style::default().fg(Color::White)),
            ])
        })
        .collect();
    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}
