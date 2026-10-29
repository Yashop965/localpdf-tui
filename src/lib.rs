//! Library target for LocalPDF-TUI.
//!
//! The binary (`src/main.rs`) is a thin CLI/entry layer over these modules;
//! integration tests drive the TUI's event loop through this API headlessly.
pub mod engine;
pub mod error;
pub mod tui;
pub mod ui;
