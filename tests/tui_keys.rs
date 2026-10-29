//! Drives the TUI's event loop headlessly: synthesizes the exact key
//! events a human would type, then asserts on the files the operations
//! produced. No terminal is opened.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use localpdf_tui::tui;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_str(app: &mut tui::App, s: &str) {
    for c in s.chars() {
        app.handle_key(key(KeyCode::Char(c)));
    }
}

fn sample_workspace(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "localpdf-tui-it-{name}-{}",
        std::process::id()
    ));
    // Fresh per run so assertions don't see files from a previous run.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

#[test]
fn watermark_via_keys_produces_file() {
    let dir = sample_workspace("watermark");
    let engine = localpdf_tui::engine::Engine::new(std::path::Path::new(&dir)).unwrap();
    let mut app = tui::App::new(engine);

    app.handle_key(key(KeyCode::Char('a'))); // queue a sample
    assert!(!app.queue.is_empty(), "sample should be queued");

    app.handle_key(key(KeyCode::Right)); // focus → Ops
    for _ in 0..6 {
        app.handle_key(key(KeyCode::Down)); // → Watermark (index 6)
    }
    assert_eq!(app.ops_index, 6);

    let target = std::path::Path::new(&dir).join("watermarked.pdf");

    // Empty text is refused…
    app.handle_key(key(KeyCode::Enter));
    assert!(!target.exists());

    // …and the same keys again, with text, succeed.
    type_str(&mut app, "DRAFT");
    app.handle_key(key(KeyCode::Enter));
    assert!(target.exists());
}

#[test]
fn metadata_via_keys_writes_info_dict() {
    let dir = sample_workspace("metadata");
    let engine = localpdf_tui::engine::Engine::new(std::path::Path::new(&dir)).unwrap();
    let mut app = tui::App::new(engine);
    app.handle_key(key(KeyCode::Char('a')));
    app.handle_key(key(KeyCode::Right));
    for _ in 0..8 {
        app.handle_key(key(KeyCode::Down)); // → Metadata (index 8)
    }
    assert_eq!(app.ops_index, 8);
    type_str(&mut app, "Title=Hi");
    app.handle_key(key(KeyCode::Enter));
    assert!(std::path::Path::new(&dir).join("meta.pdf").exists());
}

#[test]
fn extract_text_via_keys_reports_chars() {
    let dir = sample_workspace("extracttext");
    let engine = localpdf_tui::engine::Engine::new(std::path::Path::new(&dir)).unwrap();
    let mut app = tui::App::new(engine);
    app.handle_key(key(KeyCode::Char('a')));
    app.handle_key(key(KeyCode::Right));
    for _ in 0..9 {
        app.handle_key(key(KeyCode::Down)); // → Extract Text (index 9)
    }
    assert_eq!(app.ops_index, 9);
    app.handle_key(key(KeyCode::Enter)); // no ranges = all pages
    let last = app.log.iter().last().cloned().unwrap_or_default();
    assert!(last.contains("extracted"), "last log line was: {last}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn add_file_via_keys_queues_real_file() {
    let dir = sample_workspace("addfile");
    let engine = localpdf_tui::engine::Engine::new(std::path::Path::new(&dir)).unwrap();
    let mut app = tui::App::new(engine);
    app.handle_key(key(KeyCode::Char('a'))); // queue sample_1.pdf
    let name = app
        .queue
        .last()
        .unwrap()
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // [f] → type the path → [enter] queues it again.
    app.handle_key(key(KeyCode::Char('f')));
    assert_eq!(app.mode, tui::Mode::AddFile);
    type_str(&mut app, &name);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.mode, tui::Mode::Normal);
    assert_eq!(app.queue.len(), 2);

    // [esc] cancels the prompt without touching the queue.
    app.handle_key(key(KeyCode::Char('f')));
    type_str(&mut app, &name);
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.mode, tui::Mode::Normal);
    assert_eq!(app.queue.len(), 2);

    // A path outside the sandbox is rejected with an error log.
    app.handle_key(key(KeyCode::Char('f')));
    type_str(&mut app, "C:\\Windows\\win.ini");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.mode, tui::Mode::Normal);
    assert_eq!(app.queue.len(), 2);
    let last = app.log.iter().last().cloned().unwrap_or_default();
    assert!(last.contains("cannot add"), "last log line was: {last}");
}
