use anyhow::Result;
use appimg_core::clean::{self, Kind, Leftover};
use appimg_core::{fs_util, list, InstalledApp, Paths};

use crate::cli::CleanArgs;
use crate::ui::Ui;
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &CleanArgs) -> Result<Outcome> {
    let apps = list::list(paths)?;
    let (recent, stale): (Vec<Leftover>, Vec<Leftover>) =
        clean::of_apps(paths, &apps).into_iter().partition(|leftover| leftover.recent);

    if stale.is_empty() {
        show_recent(ui, &apps, &recent);
        ui.info("Nothing to clean.");
        return Ok(Outcome::NothingToDo);
    }

    ui.info("appimg keeps these files that are no installed AppImage:");
    for leftover in &stale {
        show(ui, &apps, leftover);
    }
    ui.info(&summary(&stale));
    show_recent(ui, &apps, &recent);

    if args.dry_run {
        ui.info("Nothing was changed (dry run).");
        return Ok(Outcome::Done);
    }
    if !ui.confirm("Delete these files?", false)? {
        ui.info("Nothing was changed.");
        return Ok(Outcome::NothingToDo);
    }

    let removed = clean::remove(paths, &stale)?;
    ui.info(&format!("Removed {}.", summary(&removed).trim_end_matches('.')));
    Ok(Outcome::Done)
}

/// The staging files an update may still be writing, which are left alone.
fn show_recent(ui: &Ui, apps: &[InstalledApp], recent: &[Leftover]) {
    if recent.is_empty() {
        return;
    }
    let minutes = clean::IN_USE_WINDOW.as_secs() / 60;
    ui.info(&format!(
        "Left alone, written in the last {minutes} minutes, an update may still be writing them:"
    ));
    for leftover in recent {
        show(ui, apps, leftover);
    }
}

/// One leftover: whose it is, what it is, its size and where. A backup
/// also says what removing it gives up.
fn show(ui: &Ui, apps: &[InstalledApp], leftover: &Leftover) {
    let name = apps
        .iter()
        .find(|app| app.slug == leftover.slug)
        .map_or(leftover.slug.as_str(), |app| app.name.as_str());

    ui.info(&format!(
        "  {}: {}, {}",
        ui.bold(name),
        leftover.kind.describe(),
        fs_util::human_size(leftover.size)
    ));
    ui.info(&format!("    {}", ui.dim(&leftover.path.display().to_string())));
    if leftover.kind == Kind::Backup {
        ui.info(&format!(
            "    removing it ends the rollback of {name} to the version before its last update"
        ));
    }
}

fn summary(leftovers: &[Leftover]) -> String {
    let bytes = leftovers.iter().map(|leftover| leftover.size).sum();
    let files = if leftovers.len() == 1 { "file" } else { "files" };
    format!("{} {files}, {} in all.", leftovers.len(), fs_util::human_size(bytes))
}
