use anyhow::{bail, Result};
use appimg_core::json::escape;
use appimg_core::list::InstalledApp;
use appimg_core::update::{UpdateOutcome, UpdateStatus};
use appimg_core::{list, metadata, update, Error, Paths};

use crate::cli::UpdateArgs;
use crate::ui::{table, Ui};
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &UpdateArgs) -> Result<Outcome> {
    let targets = targets(paths, args)?;
    if targets.is_empty() {
        ui.info("Nothing installed yet.");
        return Ok(Outcome::NothingToDo);
    }

    if args.check {
        return check(ui, &targets, args.json);
    }

    let mut updated = Vec::new();
    let mut failed = 0;

    for app in &targets {
        match update_one(paths, ui, app) {
            Ok(Some(outcome)) => updated.push(outcome),
            Ok(None) => {}
            Err(error) => {
                failed += 1;
                ui.warn(&format!("{}: {error:#}", app.name));
            }
        }
    }

    if failed > 0 {
        bail!("{failed} of {} updates failed", targets.len());
    }
    if updated.is_empty() {
        ui.info("Everything is up to date.");
        return Ok(Outcome::NothingToDo);
    }
    Ok(Outcome::Done)
}

fn targets(paths: &Paths, args: &UpdateArgs) -> Result<Vec<InstalledApp>> {
    match (&args.name, args.all) {
        (Some(name), _) => Ok(vec![list::find(paths, name)?]),
        (None, true) => Ok(list::list(paths)?),
        (None, false) => {
            bail!("give a name or pass --all")
        }
    }
}

fn check(ui: &Ui, apps: &[InstalledApp], json: bool) -> Result<Outcome> {
    let mut statuses = Vec::new();
    for app in apps {
        match update::check(app) {
            Ok(status) => statuses.push(status),
            Err(error) => ui.warn(&format!("{}: {error:#}", app.name)),
        }
    }

    if json {
        ui.info(&statuses_to_json(&statuses));
    } else {
        let rows: Vec<Vec<String>> = statuses
            .iter()
            .map(|status| {
                vec![
                    status.name.clone(),
                    status.current_version.clone().unwrap_or_else(|| "-".to_string()),
                    status.latest_version.clone().unwrap_or_else(|| "-".to_string()),
                    status.source.describe(),
                    if status.available {
                        ui.accent("update available")
                    } else {
                        ui.dim(status.note.as_deref().unwrap_or("up to date"))
                    },
                ]
            })
            .collect();
        ui.info(&table(ui, &["NAME", "CURRENT", "LATEST", "SOURCE", "STATUS"], &rows));
    }

    if statuses.iter().any(|status| status.available) {
        Ok(Outcome::Done)
    } else {
        Ok(Outcome::NothingToDo)
    }
}

/// Updates one application. Returns `None` when there was nothing to do.
fn update_one(paths: &Paths, ui: &Ui, app: &InstalledApp) -> Result<Option<UpdateOutcome>> {
    // A source that cannot be checked can still be re-downloaded.
    let status = update::check(app).ok();
    if let Some(status) = &status {
        if !status.available && status.note.is_none() {
            ui.info(&format!("{} is up to date.", app.name));
            return Ok(None);
        }
    }

    ui.info(&format!("Updating {}...", ui.bold(&app.name)));
    let mut progress = ui.progress();
    let outcome =
        match update::update(paths, app, Some(&mut |done, total| progress.update(done, total))) {
            Ok(outcome) => outcome,
            Err(Error::NoUpdateSource(_)) => {
                ui.info(&format!("{}: no update source recorded, skipped.", app.name));
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
    progress.finish();

    // The new binary has to run once before the backup goes away.
    if outcome.backup_path.is_some() && metadata::extract(&outcome.appimage_path).is_none() {
        update::rollback(paths, &app.slug)?;
        bail!("the updated AppImage does not run, rolled back to the previous version");
    }
    update::confirm(paths, &app.slug)?;

    ui.info(&format!(
        "  {} -> {}",
        outcome.from_version.as_deref().unwrap_or("unknown"),
        outcome.to_version.as_deref().unwrap_or("unknown")
    ));
    Ok(Some(outcome))
}

fn statuses_to_json(statuses: &[UpdateStatus]) -> String {
    let items: Vec<String> = statuses
        .iter()
        .map(|status| {
            format!(
                concat!(
                    "{{\"slug\":\"{slug}\",\"name\":\"{name}\",\"current_version\":{current},",
                    "\"latest_version\":{latest},\"available\":{available},",
                    "\"source\":\"{source}\",\"note\":{note}}}"
                ),
                slug = escape(&status.slug),
                name = escape(&status.name),
                current = optional(status.current_version.as_deref()),
                latest = optional(status.latest_version.as_deref()),
                available = status.available,
                source = status.source.describe(),
                note = optional(status.note.as_deref()),
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn optional(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{}\"", escape(value)),
        None => "null".to_string(),
    }
}
