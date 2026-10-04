use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use appimg_core::export::{self, ExportedApp, Fetch};
use appimg_core::install::InstallRequest;
use appimg_core::remote::Remote;
use appimg_core::update::{self, ReleaseAsset, UpdateSource};
use appimg_core::{digest, install, list, metadata, Paths};

use crate::cli::ImportArgs;
use crate::commands::install::download_appimage;
use crate::commands::update::update_one;
use crate::ui::Ui;
use crate::Outcome;

/// Installs every application of an export that is not installed yet,
/// downloading each again, with what the export decided for its entry. One
/// that fails does not stop the others. Those with nothing to download are
/// listed at the end with what they need.
pub fn run(paths: &Paths, ui: &Ui, args: &ImportArgs) -> Result<Outcome> {
    let file = args.file.display().to_string();
    let text = fs::read_to_string(&args.file).with_context(|| format!("cannot read {file}"))?;
    let apps = export::from_json(&text).with_context(|| file.clone())?;

    let mut taken: HashSet<String> = list::list(paths)?.into_iter().map(|app| app.slug).collect();
    let mut skipped = 0;
    let mut manual = Vec::new();
    let mut imported = 0;
    let mut failed = Vec::new();
    let mut not_updated = Vec::new();

    if args.dry_run {
        ui.info(&format!("Would import from {file}:"));
    }
    for app in &apps {
        let label = format!("{} ({})", ui.bold(&app.name), app.slug);
        if !taken.insert(app.slug.clone()) {
            ui.info(&format!("{label} is already installed, skipped."));
            skipped += 1;
            continue;
        }
        let fetch = app.fetch();
        if fetch == Fetch::Nothing {
            manual.push(app);
            continue;
        }
        if args.dry_run {
            ui.info(&format!("  {label}: {}", describe(&fetch)));
            imported += 1;
            continue;
        }

        ui.info(&format!("Importing {label}"));
        match import_one(paths, ui, app, &fetch) {
            Ok(Imported::Current) => imported += 1,
            Ok(Imported::NotUpdated(why)) => {
                imported += 1;
                ui.warn(&format!("{}: {why}", app.name));
                not_updated.push(app.name.as_str());
            }
            Err(error) => {
                ui.warn(&format!("{}: {error:#}", app.name));
                failed.push(app.name.as_str());
            }
        }
    }

    if !manual.is_empty() {
        ui.info("");
        ui.info(&format!(
            "Nothing to download for {}, {} its AppImage file:",
            count(manual.len()),
            if manual.len() == 1 { "it needs" } else { "each needs" }
        ));
        for app in &manual {
            ui.info(&format!("  {} ({}): {}", ui.bold(&app.name), app.slug, needs(app)));
        }
    }

    ui.info("");
    let mut parts = Vec::new();
    if skipped > 0 {
        parts.push(format!("{skipped} already installed"));
    }
    if !manual.is_empty() {
        let need = if manual.len() == 1 { "needs its" } else { "need their" };
        parts.push(format!("{} {need} AppImage file", manual.len()));
    }
    if !not_updated.is_empty() {
        parts.push(format!(
            "{} not brought up to date: {}",
            not_updated.len(),
            not_updated.join(", ")
        ));
    }
    if !failed.is_empty() {
        parts.push(format!("{} failed: {}", failed.len(), failed.join(", ")));
    }
    let rest = if parts.is_empty() { String::new() } else { format!(", {}", parts.join(", ")) };
    let verb = if args.dry_run { "Would import" } else { "Imported" };
    ui.info(&format!("{verb} {imported} of {}{rest}.", count(apps.len())));

    let mut problems = Vec::new();
    if !failed.is_empty() {
        problems.push(format!("{} of {} could not be imported", failed.len(), count(apps.len())));
    }
    if !not_updated.is_empty() {
        problems.push(format!(
            "{} {} imported but could not be brought up to date",
            count(not_updated.len()),
            if not_updated.len() == 1 { "was" } else { "were" }
        ));
    }
    if !problems.is_empty() {
        bail!("{}", problems.join(", "));
    }
    Ok(if imported > 0 { Outcome::Done } else { Outcome::NothingToDo })
}

/// How an import that did not fail went.
enum Imported {
    /// Installed, and as current as anything can tell.
    Current,
    /// Installed at the version the URL it was downloaded from holds, and
    /// only bringing it up to date failed, for this reason.
    NotUpdated(String),
}

/// Downloads one application and installs it the way the export says.
fn import_one(paths: &Paths, ui: &Ui, app: &ExportedApp, fetch: &Fetch) -> Result<Imported> {
    match fetch {
        Fetch::Release { source, asset_hint } => {
            let asset = update::newest_asset(source, asset_hint.as_deref())?;
            // Against what the release publishes, before anything reads the
            // metadata, which can run the file.
            let verify = |file: &Path| digest::verify(file, &asset.url, &asset.published).map(Some);
            let (file, _scratch, _) = download_appimage(ui, &asset.url, &verify)?;
            install_file(paths, ui, app, &file, &asset.url, Some(&asset), None)?;
            Ok(Imported::Current)
        }
        Fetch::UpdateSource(url) | Fetch::Origin(url) => {
            let (file, _scratch, remote) =
                download_appimage(ui, url, &|file| install::verify_download(file, url))?;
            let version = install_file(paths, ui, app, &file, url, None, Some(remote))?;
            match bring_up_to_date(paths, ui, app, url) {
                Ok(()) => Ok(Imported::Current),
                Err(error) => Ok(Imported::NotUpdated(format!(
                    "it is installed at {} from {url}, only bringing it up to date failed: \
                     {error:#}",
                    match &version {
                        Some(version) => format!("version {version}"),
                        None => "the version".to_string(),
                    }
                ))),
            }
        }
        Fetch::Nothing => Ok(Imported::Current),
    }
}

/// Installs the downloaded file with the name, comment, categories,
/// arguments, terminal flag and update source the export holds, under the
/// slug it had. The export already decided all of it, so nothing is asked.
/// Returns the version it recorded.
fn install_file(
    paths: &Paths,
    ui: &Ui,
    app: &ExportedApp,
    file: &Path,
    origin: &str,
    asset: Option<&ReleaseAsset>,
    remote: Option<Remote>,
) -> Result<Option<String>> {
    // Only a real import gets here, never a dry run: it is the install the
    // user asked for.
    let info = metadata::inspect(
        file,
        appimg_core::current_locale().as_deref(),
        metadata::Reading::MayRun,
    )?;
    if info.extract_root().is_none() {
        ui.warn(&format!(
            "{}: the AppImage did not extract, so it gets the generic icon: {}",
            app.name,
            info.extract_problems.join("; ")
        ));
    }

    let mut request = InstallRequest::from_info(file, origin, &info);
    request.remote = remote;
    request.slug = Some(app.slug.clone());
    request.name = app.name.clone();
    request.comment = app.comment.clone();
    request.categories = app.categories.clone();
    request.extra_args = app.arguments.clone();
    request.terminal = app.terminal;
    match app.update_source.as_deref() {
        Some(update::MANUAL) => request.update_source = None,
        Some(source) => request.update_source = Some(source.to_string()),
        // An entry written by 0.2.x followed what it was installed from,
        // which is what a request from a URL does on its own.
        None => {}
    }
    if let Some(asset) = asset {
        request.release = asset.release.clone();
        request.version = asset.version_for(info.version.clone());
    }

    let outcome = install::install(paths, &request)?;
    ui.info(&format!(
        "  installed as {}, version {}",
        ui.accent(&outcome.slug),
        request.version.as_deref().unwrap_or("unknown")
    ));
    for warning in &outcome.validation_warnings {
        ui.warn(warning);
    }
    Ok(request.version)
}

/// Brings an application installed from a URL up to date right away, when
/// it has something to update from: update information in the AppImage, or
/// an update source. A source that is that very URL has nothing newer than
/// what was just downloaded.
fn bring_up_to_date(paths: &Paths, ui: &Ui, app: &ExportedApp, url: &str) -> Result<()> {
    let installed = list::find(paths, &app.slug)?;
    match update::source_for(&installed) {
        UpdateSource::Manual => Ok(()),
        UpdateSource::DirectUrl { url: source } if source == url => Ok(()),
        _ => {
            update_one(paths, ui, &installed)?;
            Ok(())
        }
    }
}

/// What the dry run says about where an application would come from.
fn describe(fetch: &Fetch) -> String {
    match fetch {
        Fetch::Release { source, asset_hint: Some(hint) } => {
            format!("the newest AppImage of {source}, the one like {hint}")
        }
        Fetch::Release { source, asset_hint: None } => format!("the newest AppImage of {source}"),
        Fetch::UpdateSource(url) => format!("{url}, which it updates from"),
        Fetch::Origin(url) => {
            format!("{url}, which it was installed from, then brought up to date if it can be")
        }
        Fetch::Nothing => "nothing to download".to_string(),
    }
}

/// What an application with nothing to download had, which says what it
/// needs.
fn needs(app: &ExportedApp) -> String {
    match app.origin.as_deref() {
        Some(origin) => {
            format!("it was installed from {origin} on the machine it was exported from")
        }
        None => "where it was installed from is not recorded".to_string(),
    }
}

fn count(apps: usize) -> String {
    format!("{apps} {}", if apps == 1 { "app" } else { "apps" })
}
