//! The terminal interface. Owns the terminal: raw mode, the alternate
//! screen and a panic hook that gives both back whatever happens.

mod app;
mod browser;
mod form;
mod theme;
mod view;

use std::io::{self, Stdout};
use std::panic;

use anyhow::{bail, Context, Result};
use appimg_core::{list, Paths};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::{execute, ExecutableCommand};
use ratatui::Terminal;

use crate::ui::Ui;
use crate::Outcome;

use self::app::{Action, App};

type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn run(paths: &Paths, ui: &Ui) -> Result<Outcome> {
    if !ui.is_interactive() {
        bail!("the terminal interface needs a terminal, try `appimg list`");
    }

    let mut app = App::new(paths.clone())?;
    let mut terminal = enter().context("cannot set up the terminal")?;
    let result = event_loop(&mut terminal, &mut app);
    leave(&mut terminal)?;

    result?;
    Ok(Outcome::Done)
}

fn event_loop(terminal: &mut Tui, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|frame| view::draw(frame, app))?;
        if app.quit {
            return Ok(());
        }

        match event::read()? {
            Event::Key(key) => app.on_key(key),
            // A resize redraws on the next pass, everything is laid out fresh.
            Event::Resize(_, _) => continue,
            _ => continue,
        }

        if let Some(action) = app.take_pending() {
            // Long work deserves a redraw first, so the status line explains
            // why nothing reacts for a moment.
            terminal.draw(|frame| view::draw(frame, app))?;

            match action {
                Action::Edit(slug) => run_editor(terminal, app, &slug)?,
                action => {
                    if let Err(error) = app.run_action(action) {
                        app.status = Some(format!("{error:#}"));
                    }
                }
            }
        }
    }
}

/// Hands the terminal to `$EDITOR` and takes it back afterwards. Raw mode
/// and the alternate screen are given up before the editor starts and taken
/// again once it is gone, whether it succeeded or not.
fn run_editor(terminal: &mut Tui, app: &mut App, slug: &str) -> Result<()> {
    let installed = list::find(&app.paths, slug)?;

    leave(terminal)?;
    let result = crate::commands::edit::edit_entry(&app.paths, &installed.desktop_entry_path);
    *terminal = enter()?;
    terminal.clear()?;

    app.reload()?;
    app.select_slug(slug);
    app.status = Some(match result {
        Ok(edited) if !edited.changed => "Nothing changed.".to_string(),
        Ok(edited) => match edited.warnings.first() {
            Some(warning) => {
                format!("Updated {}. desktop-file-validate: {warning}", installed.name)
            }
            None => format!("Updated the entry of {}.", installed.name),
        },
        Err(error) => format!("{error:#}"),
    });
    Ok(())
}

fn enter() -> Result<Tui> {
    install_panic_hook();
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok(terminal)
}

fn leave(terminal: &mut Tui) -> Result<()> {
    restore();
    terminal.show_cursor()?;
    Ok(())
}

/// Puts the terminal back. Safe to call twice, and it must never fail.
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// A panic must not leave a wrecked terminal behind.
fn install_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}
