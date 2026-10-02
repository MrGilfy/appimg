use anyhow::Result;
use appimg_core::list::InstalledApp;
use appimg_core::update::{self, UpdateSource};
use appimg_core::{list, Paths};

use crate::cli::UpdateSourceArgs;
use crate::ui::Ui;
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &UpdateSourceArgs) -> Result<Outcome> {
    let app = list::find(paths, &args.name)?;
    if args.source.is_none() && !args.clear {
        show(ui, &app);
        return Ok(Outcome::Done);
    }

    if !update::set_update_source(&app, args.source.as_deref())? {
        ui.info("Nothing changed.");
        return Ok(Outcome::NothingToDo);
    }
    let app = list::find(paths, &app.slug)?;
    match app.update_source.as_deref() {
        Some(source) if source != update::MANUAL => {
            ui.info(&format!("{} updates from {} from now on.", ui.bold(&app.name), source));
        }
        _ => ui.info(&format!("{} is updated manually from now on.", ui.bold(&app.name))),
    }
    // Setting a source is allowed either way, it is what an update falls
    // back to, but it should be clear that it is not what runs.
    if is_embedded(&update::source_for(&app)) {
        ui.warn(&format!(
            "{} carries update information of its own, which comes first: {}",
            app.name,
            app.update_info.as_deref().unwrap_or_default()
        ));
    }
    Ok(Outcome::Done)
}

fn show(ui: &Ui, app: &InstalledApp) {
    let source = update::source_for(app);
    let set = match app.update_source.as_deref() {
        Some(value) => value.to_string(),
        None => "not set, the entry was written before there was one".to_string(),
    };
    let rows = [
        ("Origin", app.origin.as_deref().unwrap_or("-").to_string()),
        ("Embedded", app.update_info.as_deref().unwrap_or("none").to_string()),
        ("Update source", set),
        ("Updates from", following(&source)),
    ];
    for (label, value) in rows {
        ui.info(&format!("{} {value}", ui.bold(&format!("{label:<14}"))));
    }
    if source == UpdateSource::Manual {
        ui.info(&format!(
            "Set an update source with: appimg update-source {} <URL|github:owner/repo>",
            app.slug
        ));
    }
}

/// What an update follows, in full.
fn following(source: &UpdateSource) -> String {
    match source {
        UpdateSource::Zsync { update_info } => format!("the embedded {update_info}"),
        UpdateSource::GitHubZsync { owner, repo, .. } => {
            format!("the embedded update information, zsync files of github:{owner}/{repo}")
        }
        UpdateSource::GitHubRelease { owner, repo, tag: Some(tag), .. } => {
            format!("github:{owner}/{repo}, the release tagged {tag}")
        }
        UpdateSource::GitHubRelease { owner, repo, tag: None, .. } => {
            format!("github:{owner}/{repo}, the newest release that has this AppImage")
        }
        UpdateSource::DirectUrl { url } => url.clone(),
        UpdateSource::Manual => "nothing, it is updated manually".to_string(),
    }
}

fn is_embedded(source: &UpdateSource) -> bool {
    matches!(source, UpdateSource::Zsync { .. } | UpdateSource::GitHubZsync { .. })
}
