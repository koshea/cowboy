//! Curses-style terminal UI for cowboy (ratatui + crossterm).
//!
//! This crate holds the renderable [`app::App`] state and a pure [`app::draw`]
//! function so rendering is snapshot-testable. The CLI owns the event loop and
//! the async agent integration.

pub mod app;

pub use app::{
    draw, App, Completion, LineKind, Mode, ModelChoice, ModelForm, ModelPicker, TranscriptLine,
    REASONING_OPTS,
};
