use anyhow::{bail, Result};
use appimg_core::json::escape;
use appimg_core::list::InstalledApp;
use appimg_core::update::{UpdateOutcome, UpdateSource, UpdateStatus};
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
    let mut manual = 0;
    let mut held = 0;

    for app in &targets {
        if app.hold.is_some() && args.all {
            pass_over(ui, app);
            held += 1;
            continue;
        }
        if update::source_for(app) == UpdateSource::Manual {
            // Asked for by name, nothing to update from is the answer.
            if !args.all {
                return Err(Error::NoUpdateSource(app.slug.clone()).into());
            }
            ui.info(&format!(
                "{} is updated manually, skipped. Set an update source with: appimg \
                 update-source {} <URL|github:owner/repo>",
                app.name, app.slug
            ));
            manual += 1;
            continue;
        }
        // Asked for by name, the one target there is.
        if app.hold.is_some() {
            return update_held(paths, ui, app);
        }
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
        match (manual > 0, held > 0) {
            (_, true) => ui.info("Everything else is up to date."),
            (true, false) => ui.info("Everything with an update source is up to date."),
            (false, false) => ui.info("Everything is up to date."),
        }
        return Ok(Outcome::NothingToDo);
    }
    Ok(Outcome::Done)
}

/// `update --all` passes a held application over. It is checked all the
/// same, so the hold never hides an update, and a check that fails fails
/// nothing: nothing was going to be updated.
fn pass_over(ui: &Ui, app: &InstalledApp) {
    let found = match update::check(app) {
        Ok(status) if status.available => format!(
            ": {} is available, appimg update {} takes it anyway",
            status.latest_version.as_deref().unwrap_or("an update"),
            app.slug
        ),
        Ok(status) if status.nothing_to_do() => ", it is up to date".to_string(),
        Ok(status) => format!(", {}", status.note.as_deref().unwrap_or("nothing to tell")),
        Err(error) => format!(", its check failed: {error:#}"),
    };
    ui.info(&format!("{} is held, skipped{found}.", app.name));
}

/// Updating a held application by name: only when there is something to
/// update, and only once the user said so, `--yes` included. The hold stays.
fn update_held(paths: &Paths, ui: &Ui, app: &InstalledApp) -> Result<Outcome> {
    let status = update::check(app).ok();
    if up_to_date(ui, app, status.as_ref()) {
        return Ok(Outcome::NothingToDo);
    }
    let at = app.version.as_deref().map(|version| format!(" at {version}")).unwrap_or_default();
    let to = status
        .as_ref()
        .and_then(|status| status.latest_version.as_deref())
        .map(|latest| format!(" to {latest}"))
        .unwrap_or_default();
    if !ui.confirm(&format!("{} is held{at}. Update it{to} anyway?", app.name), false)? {
        ui.info(&format!("Nothing was changed, {} stays held.", app.name));
        return Ok(Outcome::NothingToDo);
    }
    apply(paths, ui, app)?;
    ui.info(&format!("  {} stays held, release it with: appimg unhold {}", app.name, app.slug));
    Ok(Outcome::Done)
}

fn targets(paths: &Paths, args: &UpdateArgs) -> Result<Vec<InstalledApp>> {
    match (&args.name, args.all) {
        (Some(name), _) => Ok(vec![list::find(paths, name)?]),
        (None, true) => Ok(list::list(paths)?),
        // clap already rejects neither, this only keeps the match total.
        (None, false) => Ok(Vec::new()),
    }
}

/// Checks every application, with a warning for each check that fails.
/// Returns what the others found, and how many failed.
pub(crate) fn statuses(ui: &Ui, apps: &[InstalledApp]) -> (Vec<UpdateStatus>, usize) {
    let mut statuses = Vec::new();
    let mut failed = 0;
    for app in apps {
        match update::check(app) {
            Ok(status) => statuses.push(status),
            Err(error) => {
                failed += 1;
                ui.warn(&format!("{}: {error:#}", app.name));
            }
        }
    }
    (statuses, failed)
}

fn check(ui: &Ui, apps: &[InstalledApp], json: bool) -> Result<Outcome> {
    let (statuses, _) = statuses(ui, apps);

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
                    describe_status(ui, status),
                ]
            })
            .collect();
        ui.info(&table(ui, &["NAME", "CURRENT", "LATEST", "SOURCE", "STATUS"], &rows));
    }

    // What `update --all` would do: a held application it passes over.
    if statuses.iter().any(|status| status.available && !status.held) {
        Ok(Outcome::Done)
    } else {
        Ok(Outcome::NothingToDo)
    }
}

/// The STATUS column of `update --check`. A held application says so,
/// next to what its check found.
fn describe_status(ui: &Ui, status: &UpdateStatus) -> String {
    let found = if status.available {
        "update available"
    } else {
        status.note.as_deref().unwrap_or("up to date")
    };
    match (status.held, status.available) {
        (true, true) => ui.accent(&format!("held, {found}")),
        (true, false) => ui.dim(&format!("held, {found}")),
        (false, true) => ui.accent(found),
        (false, false) => ui.dim(found),
    }
}

/// Updates one application. Returns `None` when there was nothing to do.
pub(crate) fn update_one(
    paths: &Paths,
    ui: &Ui,
    app: &InstalledApp,
) -> Result<Option<UpdateOutcome>> {
    // A source that cannot be checked can still be re-downloaded.
    let status = update::check(app).ok();
    if up_to_date(ui, app, status.as_ref()) {
        return Ok(None);
    }
    apply(paths, ui, app).map(Some)
}

/// Whether a check found nothing to update, which it then says.
fn up_to_date(ui: &Ui, app: &InstalledApp, status: Option<&UpdateStatus>) -> bool {
    let Some(status) = status.filter(|status| status.nothing_to_do()) else {
        return false;
    };
    match &status.note {
        Some(note) => ui.info(&format!("{} is up to date: {note}.", app.name)),
        None => ui.info(&format!("{} is up to date.", app.name)),
    }
    true
}

/// Updates one application, whatever a check said, and confirms the update
/// once the new version runs.
fn apply(paths: &Paths, ui: &Ui, app: &InstalledApp) -> Result<UpdateOutcome> {
    ui.info(&format!("Updating {}...", ui.bold(&app.name)));
    let mut progress = ui.progress();
    let outcome =
        update::update(paths, app, Some(&mut |done, total| progress.update(done, total)))?;
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
    // Which path the update took, and what it cost. Every source reports
    // one, so this line is always there.
    ui.info(&format!("  {}", ui.dim(&outcome.path.describe())));
    // An update out of a GitHub release also says what its digest said.
    if let Some(verified) = &outcome.digest {
        ui.info(&format!("  {}", ui.dim(&verified.describe())));
    }
    Ok(outcome)
}

fn statuses_to_json(statuses: &[UpdateStatus]) -> String {
    let items: Vec<String> = statuses
        .iter()
        .map(|status| {
            format!(
                concat!(
                    "{{\"slug\":\"{slug}\",\"name\":\"{name}\",\"current_version\":{current},",
                    "\"latest_version\":{latest},\"available\":{available},",
                    "\"source\":\"{source}\",\"note\":{note},\"held\":{held}}}"
                ),
                slug = escape(&status.slug),
                name = escape(&status.name),
                current = optional(status.current_version.as_deref()),
                latest = optional(status.latest_version.as_deref()),
                available = status.available,
                source = escape(&status.source.describe()),
                note = optional(status.note.as_deref()),
                held = status.held,
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
