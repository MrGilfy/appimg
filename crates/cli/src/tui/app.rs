//! State of the terminal interface and what the keys do to it.

use std::path::PathBuf;

use anyhow::Result;
use appimg_core::list::InstalledApp;
use appimg_core::{install, list, metadata, remove, update, Paths};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::browser::Browser;
use super::form::{Field, InstallForm};

/// Work that takes long enough to deserve a redraw before it starts, or that
/// needs the terminal back, so the event loop runs it, not the key handler.
pub enum Action {
    Install(Box<InstallForm>),
    Inspect(PathBuf),
    UpdateOne(String),
    UpdateAll,
    Remove(String),
    Edit(String),
}

pub enum Mode {
    List,
    Search,
    Details,
    Help,
    Browse(Box<Browser>),
    Form(Box<InstallForm>),
    Preview(Box<InstallForm>),
    Confirm { question: String, action: Box<Action> },
}

pub struct App {
    pub paths: Paths,
    pub apps: Vec<InstalledApp>,
    pub visible: Vec<usize>,
    pub selected: usize,
    pub filter: String,
    pub mode: Mode,
    pub status: Option<String>,
    pub quit: bool,
    pending: Option<Action>,
}

impl App {
    pub fn new(paths: Paths) -> Result<Self> {
        let mut app = Self {
            paths,
            apps: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            filter: String::new(),
            mode: Mode::List,
            status: None,
            quit: false,
            pending: None,
        };
        app.reload()?;
        Ok(app)
    }

    pub fn reload(&mut self) -> Result<()> {
        self.apps = list::list(&self.paths)?;
        self.apply_filter();
        Ok(())
    }

    pub fn apply_filter(&mut self) {
        let needle = self.filter.trim().to_lowercase();
        self.visible = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, app)| {
                needle.is_empty()
                    || app.name.to_lowercase().contains(&needle)
                    || app.slug.contains(&needle)
                    || app.categories.iter().any(|c| c.to_lowercase().contains(&needle))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    pub fn selected_app(&self) -> Option<&InstalledApp> {
        self.visible.get(self.selected).and_then(|index| self.apps.get(*index))
    }

    pub fn take_pending(&mut self) -> Option<Action> {
        self.pending.take()
    }

    fn schedule(&mut self, action: Action, status: &str) {
        self.status = Some(status.to_string());
        self.pending = Some(action);
    }

    fn move_by(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        self.selected = match delta {
            delta if delta < 0 => self.selected.saturating_sub(delta.unsigned_abs()),
            delta => (self.selected + delta as usize).min(last),
        };
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        self.status = None;

        match &mut self.mode {
            Mode::List => self.on_list_key(key),
            Mode::Search => self.on_search_key(key),
            Mode::Details | Mode::Help => {
                self.mode = Mode::List;
            }
            Mode::Browse(_) => self.on_browse_key(key),
            Mode::Form(_) => self.on_form_key(key),
            Mode::Preview(_) => self.on_preview_key(key),
            Mode::Confirm { .. } => self.on_confirm_key(key),
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.visible.len().saturating_sub(1)
            }
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('/') => {
                self.filter.clear();
                self.apply_filter();
                self.mode = Mode::Search;
            }
            KeyCode::Enter => {
                if self.selected_app().is_some() {
                    self.mode = Mode::Details;
                }
            }
            KeyCode::Char('r') => match self.reload() {
                Ok(()) => self.status = Some("Reloaded.".to_string()),
                Err(error) => self.status = Some(format!("{error:#}")),
            },
            KeyCode::Char('i') => self.mode = Mode::Browse(Box::new(Browser::new())),
            KeyCode::Char('u') => {
                if let Some(app) = self.selected_app() {
                    let (slug, name) = (app.slug.clone(), app.name.clone());
                    self.schedule(Action::UpdateOne(slug), &format!("Updating {name}..."));
                }
            }
            KeyCode::Char('U') => {
                if !self.apps.is_empty() {
                    self.schedule(Action::UpdateAll, "Updating everything...");
                }
            }
            KeyCode::Char('e') => {
                if let Some(app) = self.selected_app() {
                    let slug = app.slug.clone();
                    self.schedule(Action::Edit(slug), "Opening the editor...");
                }
            }
            KeyCode::Char('d') => {
                if let Some(app) = self.selected_app() {
                    let files = remove::plan(&self.paths, &app.slug)
                        .map(|plan| plan.files().len())
                        .unwrap_or(0);
                    self.mode = Mode::Confirm {
                        question: format!("Remove {} and its {files} files?", app.name),
                        action: Box::new(Action::Remove(app.slug.clone())),
                    };
                }
            }
            _ => {}
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.apply_filter();
                self.mode = Mode::List;
            }
            KeyCode::Enter => self.mode = Mode::List,
            KeyCode::Backspace => {
                self.filter.pop();
                self.apply_filter();
            }
            KeyCode::Char(character) => {
                self.filter.push(character);
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn on_browse_key(&mut self, key: KeyEvent) {
        let Mode::Browse(browser) = &mut self.mode else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::List,
            KeyCode::Char('j') | KeyCode::Down => browser.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => browser.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => browser.go_to(0),
            KeyCode::Char('G') | KeyCode::End => browser.go_to(usize::MAX),
            KeyCode::PageDown => browser.move_by(10),
            KeyCode::PageUp => browser.move_by(-10),
            KeyCode::Char('h') | KeyCode::Left => {
                if let Some(parent) = browser.directory.parent().map(std::path::Path::to_path_buf) {
                    browser.directory = parent;
                    browser.reload();
                }
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(path) = browser.activate() {
                    self.schedule(Action::Inspect(path), "Reading the AppImage...");
                }
            }
            _ => {}
        }
    }

    fn on_form_key(&mut self, key: KeyEvent) {
        let Mode::Form(form) = &mut self.mode else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.mode = Mode::List,
            KeyCode::Tab | KeyCode::Down => form.field = form.field.next(),
            KeyCode::BackTab | KeyCode::Up => form.field = form.field.previous(),
            KeyCode::Enter => {
                let mut form = match std::mem::replace(&mut self.mode, Mode::List) {
                    Mode::Form(form) => form,
                    _ => return,
                };
                match form.finish() {
                    Ok(()) => self.mode = Mode::Preview(form),
                    Err(problem) => {
                        self.status = Some(problem);
                        self.mode = Mode::Form(form);
                    }
                }
            }
            KeyCode::Char(' ') if form.field == Field::Categories => form.toggle_category(),
            KeyCode::Char(' ') if form.field == Field::Terminal => {
                form.request.terminal = !form.request.terminal
            }
            KeyCode::Left if form.field == Field::Categories => form.move_category(-1),
            KeyCode::Right if form.field == Field::Categories => form.move_category(1),
            KeyCode::Backspace => {
                if let Some(text) = form.text_mut() {
                    text.pop();
                }
            }
            KeyCode::Char(character) => {
                if let Some(text) = form.text_mut() {
                    text.push(character);
                }
            }
            _ => {}
        }
    }

    fn on_preview_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('e') => {
                if let Mode::Preview(form) = std::mem::replace(&mut self.mode, Mode::List) {
                    self.mode = Mode::Form(form);
                }
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                if let Mode::Preview(form) = std::mem::replace(&mut self.mode, Mode::List) {
                    let name = form.request.name.clone();
                    self.schedule(Action::Install(form), &format!("Installing {name}..."));
                }
            }
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    fn on_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Mode::Confirm { action, .. } = std::mem::replace(&mut self.mode, Mode::List)
                {
                    self.schedule(*action, "Working...");
                }
            }
            _ => self.mode = Mode::List,
        }
    }

    /// Runs everything that does not need the terminal back.
    pub fn run_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Inspect(path) => self.inspect(path),
            Action::Install(form) => self.install(*form),
            Action::UpdateOne(slug) => self.update_one(&slug),
            Action::UpdateAll => self.update_all(),
            Action::Remove(slug) => self.remove(&slug),
            // The event loop owns the terminal, so it runs the editor itself.
            Action::Edit(_) => Ok(()),
        }
    }

    fn inspect(&mut self, path: PathBuf) -> Result<()> {
        let info = metadata::inspect(&path, appimg_core::current_locale().as_deref())?;
        // Not extracting is not fatal here, the form asks for name and icon
        // anyway, but the reason belongs on screen.
        let problem = info.extract_root().is_none().then(|| match info.extract_problems.first() {
            Some(problem) => format!("Not extracted, name and icon are guesses: {problem}"),
            None => "Not extracted, name and icon are guesses.".to_string(),
        });
        let origin = path.to_string_lossy().into_owned();
        self.mode = Mode::Form(Box::new(InstallForm::new(&path, &origin, info)));
        self.status = problem;
        Ok(())
    }

    fn install(&mut self, mut form: InstallForm) -> Result<()> {
        let plan = install::plan(&self.paths, &form.request)?;
        form.request.overwrite = plan.already_installed;
        let outcome = install::install(&self.paths, &form.request)?;
        self.reload()?;
        self.select_slug(&outcome.slug);

        let what = if outcome.replaced { "Replaced" } else { "Installed" };
        let mut message = format!("{what} {} ({} icons).", outcome.slug, outcome.icons.len());
        if let Some(warning) = outcome.validation_warnings.first() {
            message.push_str(&format!(" desktop-file-validate: {warning}"));
        }
        self.status = Some(message);
        Ok(())
    }

    fn update_one(&mut self, slug: &str) -> Result<()> {
        let app = list::find(&self.paths, slug)?;
        let message = match update::update(&self.paths, &app, None) {
            Ok(outcome) => {
                if outcome.backup_path.is_some()
                    && metadata::extract(&outcome.appimage_path).is_none()
                {
                    update::rollback(&self.paths, slug)?;
                    format!("{}: the new version does not run, rolled back.", app.name)
                } else {
                    update::confirm(&self.paths, slug)?;
                    format!(
                        "Updated {} to {}.",
                        app.name,
                        outcome.to_version.as_deref().unwrap_or("the latest version")
                    )
                }
            }
            Err(error) => format!("{}: {error}", app.name),
        };

        self.reload()?;
        self.select_slug(slug);
        self.status = Some(message);
        Ok(())
    }

    fn update_all(&mut self) -> Result<()> {
        let apps = list::list(&self.paths)?;
        let mut updated = 0;
        let mut failed = 0;

        for app in &apps {
            match update::check(app) {
                Ok(status) if !status.available && status.note.is_none() => continue,
                Ok(_) | Err(_) => {}
            }
            match update::update(&self.paths, app, None) {
                Ok(_) => {
                    let _ = update::confirm(&self.paths, &app.slug);
                    updated += 1;
                }
                Err(_) => failed += 1,
            }
        }

        self.reload()?;
        self.status = Some(match (updated, failed) {
            (0, 0) => "Everything is up to date.".to_string(),
            (updated, 0) => format!("Updated {updated} applications."),
            (updated, failed) => format!("Updated {updated}, {failed} failed."),
        });
        Ok(())
    }

    fn remove(&mut self, slug: &str) -> Result<()> {
        let plan = remove::remove(&self.paths, slug)?;
        self.reload()?;
        self.status = Some(format!("Removed {slug} ({} files).", plan.files().len()));
        Ok(())
    }

    pub fn select_slug(&mut self, slug: &str) {
        if let Some(position) = self.visible.iter().position(|index| self.apps[*index].slug == slug)
        {
            self.selected = position;
        }
    }
}
