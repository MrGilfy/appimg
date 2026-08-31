//! Dark, quiet, one accent color. No gradients, no emoji, no glyphs that
//! need a patched font.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;

pub fn base() -> Style {
    Style::default()
}

pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub fn title() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn selected() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

pub fn warning() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// The border of a dialog, so it stays visible without color.
pub fn border() -> Style {
    Style::default().fg(ACCENT)
}
