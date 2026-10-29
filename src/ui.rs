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

use crate::tui::{App, OPS, OPS_DESC, Panel};

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
        Constraint::Min(8),
        Constraint::Length(5),
    ])
    .split(f.area());

    render_title(f, outer[0]);

    let main = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(outer[1]);
    render_queue(f, main[0], app);
    render_ops(f, main[1], app);
    render_status(f, outer[2], app);
}

fn render_title(f: &mut Frame, area: ratatui::layout::Rect) {
    let hint = "  [a] add · [←][→] focus · [↑][↓] move · [enter] run · [q] quit  ";
    let width = area.width.saturating_sub(hint.len() as u16);
    let line = Line::from(vec![
        Span::styled(" LocalPDF-TUI ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(" · private pdf toolkit ", Style::default().fg(DIM)),
        Span::raw("·".repeat(width.max(1) as usize)),
        Span::styled(hint, Style::default().fg(DIM)),
    ]);
    let widget = Paragraph::new(line).style(Style::default().bg(PANEL_BG));
    f.render_widget(widget, area);
}

fn render_queue(f: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let block = bordered_block(" FILE QUEUE ");
    if app.queue.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled("  empty", Style::default().fg(DIM))),
            Line::from(Span::styled("  press [a] to queue a sample document", Style::default().fg(DIM))),
        ])
        .block(block);
        f.render_widget(empty, area);
        return;
    }

    let mut state = ListState::default();
    state.select(Some(app.q_index));
    let list = List::new(app.queue_lines.clone())
        .block(block)
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().bg(SEL_BG).fg(Color::White).add_modifier(Modifier::BOLD));
    f.render_widget(list, area);
}

fn render_ops(f: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let split = Layout::vertical([Constraint::Length(5), Constraint::Length(1)]).split(area);

    let ops_block = bordered_block(" OPERATIONS ");
    let active = app.focus == Panel::Ops;
    let lines: Vec<Line> = (0..OPS.len())
        .map(|i| {
            let marker = if active && i == app.ops_index { "▸" } else { " " };
            let unavailable = i == OPS.len() - 1;
            let op_style = Style::default()
                .fg(if unavailable { DIM } else { Color::White })
                .add_modifier(if active && i == app.ops_index { Modifier::BOLD } else { Modifier::empty() });
            let desc_style = Style::default().fg(if unavailable { DIM } else { Color::Rgb(139, 148, 158) });
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

    let spec_selected = active && app.ops_index == 1;
    let caret = if spec_selected { "▏" } else { " " };
    let label = Span::styled(
        "SPLIT RANGES  ",
        Style::default().fg(if spec_selected { ACCENT } else { DIM }).add_modifier(Modifier::BOLD),
    );
    let value = Span::styled(
        format!("[ {}{} ]", app.split_spec, caret),
        Style::default().fg(if spec_selected { Color::White } else { DIM }),
    );
    let help = Span::styled("   1-based, e.g. 1-3,5 — applied to the highlighted queue file", Style::default().fg(DIM));
    let input = Paragraph::new(Line::from(vec![label, value, help])).style(Style::default().bg(PANEL_BG));
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
