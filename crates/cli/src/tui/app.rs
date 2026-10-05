//! State of the terminal interface and what the keys do to it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use appimg_core::list::InstalledApp;
use appimg_core::update::UpdateSource;
use appimg_core::{archive, install, list, metadata, remove, update, Error, Paths};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::browser::Browser;
use super::form::{Field, InstallForm};

#[cfg(test)]
#[path = "../../tests/marker/mod.rs"]
mod marker;

#[cfg(test)]
#[path = "../../tests/pack/mod.rs"]
mod pack;

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
                    if app.hold.is_some() {
                        // Held: only once the user said so, and it stays held.
                        self.mode = Mode::Confirm {
                            question: format!("{name} is held. Update it anyway? It stays held."),
                            action: Box::new(Action::UpdateOne(slug)),
                        };
                    } else {
                        self.schedule(Action::UpdateOne(slug), &format!("Updating {name}..."));
                    }
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
        // An archive, by its bytes whatever its name says, is unpacked the
        // way an install from the command line unpacks it: the one AppImage
        // in it goes into a temporary directory the form keeps alive, and
        // the archive is where it came from.
        let (file, unpacked, took) = if archive::Kind::of(&path).is_some() {
            let scratch = tempfile::Builder::new()
                .prefix("appimg-unpack-")
                .tempdir()
                .context("cannot create a temporary directory to unpack the archive into")?;
            let (file, extracted) = install::unpack_archive(&path, scratch.path())?;
            (file, Some(scratch), Some(format!("Took {} out of the archive.", extracted.entry)))
        } else {
            // The same checks a download gets, before the metadata is read.
            install::check_file(&path)?;
            (path.clone(), None, None)
        };
        // Nothing is confirmed while the form is filled in, so the AppImage
        // is never run for it: without unsquashfs nothing is read, and the
        // install goes by what the form says once it is confirmed.
        let info = metadata::inspect(
            &file,
            appimg_core::current_locale().as_deref(),
            metadata::Reading::WithoutRunning,
        )?;
        // Not reading it is not fatal here, the form asks for name and icon
        // anyway, but the reason belongs on screen.
        let problem = info.extract_root().is_none().then(|| {
            if info.extract_problems.is_empty() {
                "Metadata not read, name and icon are guesses.".to_string()
            } else {
                format!(
                    "Metadata not read, name and icon are guesses: {}",
                    info.extract_problems.join("; ")
                )
            }
        });
        let origin = path.to_string_lossy().into_owned();
        self.mode = Mode::Form(Box::new(InstallForm::new(&file, &origin, info, unpacked)));
        self.status = match (took, problem) {
            (Some(took), Some(problem)) => Some(format!("{took} {problem}")),
            (took, problem) => took.or(problem),
        };
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
        if outcome.held {
            message.push_str(" It stays held.");
        }
        if let Some(source) = &outcome.kept_update_source {
            message.push_str(&format!(" Kept its update source {source}."));
        }
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
                        "Updated {} to {}.{}",
                        app.name,
                        outcome.to_version.as_deref().unwrap_or("the latest version"),
                        if app.hold.is_some() { " It stays held." } else { "" }
                    )
                }
            }
            // It names the application and says what to do already.
            Err(error @ Error::NoUpdateSource(_)) => error.to_string(),
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
        let mut manual = 0;
        let mut held = 0;

        for app in &apps {
            // Passed over, and never a failure. Its check still runs, so
            // the list shows whether the hold keeps an update back.
            if app.hold.is_some() {
                let _ = update::check(app);
                held += 1;
                continue;
            }
            // Nothing to update from is not a failure.
            if update::source_for(app) == UpdateSource::Manual {
                manual += 1;
                continue;
            }
            match update::check(app) {
                Ok(status) if status.nothing_to_do() => continue,
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
        let summary = match (updated, failed) {
            (0, 0) if held > 0 => "Everything else is up to date.".to_string(),
            (0, 0) if manual > 0 => "Everything with an update source is up to date.".to_string(),
            (0, 0) => "Everything is up to date.".to_string(),
            (updated, 0) => format!("Updated {updated} applications."),
            (updated, failed) => format!("Updated {updated}, {failed} failed."),
        };
        self.status = Some(match held {
            0 => summary,
            held => format!("{summary} {held} held, skipped."),
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Directories for appimg below `root`, created.
    fn paths_in(root: &std::path::Path) -> Paths {
        let data_home = root.join("data");
        let paths = Paths {
            appimage_dir: data_home.join("appimages"),
            applications_dir: data_home.join("applications"),
            icons_root: data_home.join("icons/hicolor"),
            bin_dir: root.join("bin"),
            config_home: root.join("config"),
            state_home: root.join("state"),
            data_home,
        };
        paths.ensure_dirs().unwrap();
        paths
    }

    /// The install form is filled in before anything is confirmed, so
    /// reading the metadata for it never runs the AppImage, nor makes it
    /// executable. Whether `unsquashfs` is on `PATH` or not, it cannot read
    /// the marker without the stand-in the CLI tests have, so nothing is
    /// read, and the status line says so and why.
    #[test]
    fn prefilling_the_install_form_runs_nothing() {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path();
        let paths = paths_in(root);
        let file = root.join("Marker.AppImage");
        let mark = root.join("it-ran");
        marker::write(&file, &mark, &root.join("payload"), 0o644);

        let mut app = App::new(paths).unwrap();
        app.run_action(Action::Inspect(file.clone())).unwrap();

        assert!(!mark.exists(), "filling in the form ran the AppImage");
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o7777, 0o644);
        let Mode::Form(form) = &app.mode else {
            panic!("no install form");
        };
        assert_eq!(form.request.name, "Marker");
        let status = app.status.clone().unwrap_or_default();
        assert!(status.starts_with("Metadata not read, name and icon are guesses: "), "{status}");
        assert!(status.contains("the AppImage was not run to read it instead"), "{status}");
    }

    /// An archive picked in the browser is unpacked the way the command
    /// line unpacks it: the one AppImage in it, by its bytes, into a
    /// temporary directory the form keeps alive until the install, named
    /// after the archive, which is where the entry says it came from and
    /// which stays as it was. Nothing runs it on the way.
    #[test]
    fn an_archive_installs_the_appimage_in_it_and_stays() {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path();
        let paths = paths_in(root);
        let mark = root.join("it-ran");
        let built = root.join("built");
        marker::write(&built, &mark, &root.join("payload"), 0o755);
        let app_bytes = fs::read(&built).unwrap();
        fs::remove_file(&built).unwrap();
        let archive = root.join("Marker-Tool-Linux.zip");
        let packed = pack::zip(&[("readme.txt", b"x"), ("../bin/marker", &app_bytes)]);
        fs::write(&archive, &packed).unwrap();

        let mut app = App::new(paths.clone()).unwrap();
        app.run_action(Action::Inspect(archive.clone())).unwrap();
        let Mode::Form(form) = std::mem::replace(&mut app.mode, Mode::List) else {
            panic!("no install form");
        };
        assert_eq!(form.request.origin, archive.to_string_lossy());
        assert_eq!(form.request.source.file_name().unwrap(), "Marker-Tool-Linux.AppImage");
        let unpacked = form.request.source.parent().unwrap().to_path_buf();
        assert!(!unpacked.starts_with(root), "{}", unpacked.display());
        assert_eq!(fs::read(&form.request.source).unwrap(), app_bytes);
        let status = app.status.clone().unwrap_or_default();
        assert!(
            status.starts_with("Took ../bin/marker out of the archive. Metadata not read"),
            "{status}"
        );

        app.run_action(Action::Install(form)).unwrap();
        let installed = list::list(&paths).unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(fs::read(&installed[0].appimage_path).unwrap(), app_bytes);
        let entry = fs::read_to_string(&installed[0].desktop_entry_path).unwrap();
        assert!(entry.contains(&format!("\nX-AppImg-Source={}\n", archive.display())), "{entry}");
        assert_eq!(fs::read(&archive).unwrap(), packed);
        assert!(!unpacked.exists(), "{} is left", unpacked.display());
        assert!(!mark.exists(), "the AppImage ran");
        // Nothing the archive named was written next to it.
        assert!(!root.join("bin").exists());
    }

    /// An archive without exactly one AppImage in it opens no form, and
    /// the reason is what the status line shows.
    #[test]
    fn an_archive_without_exactly_one_appimage_opens_no_form() {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path();
        let paths = paths_in(root);
        let appimage = b"\x7fELF\x02\x01\x01\x00AI\x02 an AppImage";
        let archive = root.join("two.zip");
        fs::write(&archive, pack::zip(&[("a", appimage), ("b", appimage)])).unwrap();

        let mut app = App::new(paths).unwrap();
        let error = app.run_action(Action::Inspect(archive)).unwrap_err();
        assert!(
            format!("{error:#}").contains("it holds 2 AppImages, and appimg takes exactly one"),
            "{error:#}"
        );
        assert!(matches!(app.mode, Mode::List));
    }

    /// An installed application as the entry and file appimg writes,
    /// updating from `source`, held or not.
    fn installed(paths: &Paths, slug: &str, source: &str, held: bool) -> Vec<u8> {
        let bytes = format!("\x7fELF {slug}").into_bytes();
        fs::write(paths.appimage_path(slug), &bytes).unwrap();
        let hold = if held { "X-AppImg-Hold=true\n" } else { "" };
        fs::write(
            paths.desktop_entry_path(slug),
            format!(
                "[Desktop Entry]\nType=Application\nName={slug}\nExec=x\nX-AppImg-Managed=true\n\
                 X-AppImg-Slug={slug}\nX-AppImg-Version=1.0.0\nX-AppImg-UpdateSource={source}\n{hold}"
            ),
        )
        .unwrap();
        bytes
    }

    fn press(app: &mut App, key: char) {
        app.on_key(KeyEvent::from(KeyCode::Char(key)));
    }

    /// Updating everything passes a held application over, and a check of
    /// it that fails, which nothing here can reach, is no failure.
    #[test]
    fn updating_everything_passes_a_held_app_over() {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let paths = paths_in(dir.path());
        let held = installed(&paths, "held", "http://127.0.0.1:1/App.AppImage", true);
        installed(&paths, "manual", "manual", false);

        let mut app = App::new(paths.clone()).unwrap();
        app.run_action(Action::UpdateAll).unwrap();
        assert_eq!(app.status.as_deref(), Some("Everything else is up to date. 1 held, skipped."));
        assert_eq!(fs::read(paths.appimage_path("held")).unwrap(), held);
    }

    /// Updating a held application asks first, and says it stays held.
    #[test]
    fn updating_a_held_app_asks_first() {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let paths = paths_in(dir.path());
        installed(&paths, "held", "manual", true);
        installed(&paths, "plain", "manual", false);

        let mut app = App::new(paths).unwrap();
        app.select_slug("held");
        press(&mut app, 'u');
        let Mode::Confirm { question, action } = &app.mode else {
            panic!("no question asked");
        };
        assert_eq!(question, "held is held. Update it anyway? It stays held.");
        assert!(matches!(**action, Action::UpdateOne(ref slug) if slug == "held"));
        assert!(app.take_pending().is_none());

        // No answers nothing, and updates nothing.
        press(&mut app, 'n');
        assert!(matches!(app.mode, Mode::List));
        assert!(app.take_pending().is_none());

        app.select_slug("plain");
        press(&mut app, 'u');
        assert!(matches!(app.mode, Mode::List));
        assert!(matches!(app.take_pending(), Some(Action::UpdateOne(slug)) if slug == "plain"));
    }
}
