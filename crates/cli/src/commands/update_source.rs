use anyhow::Result;
use appimg_core::list::InstalledApp;
use appimg_core::update::{self, UpdateSource};
use appimg_core::{download, list, Paths};

use crate::cli::UpdateSourceArgs;
use crate::ui::Ui;
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &UpdateSourceArgs) -> Result<Outcome> {
    let app = list::find(paths, &args.name)?;
    if args.source.is_none() && !args.clear && args.asset.is_none() {
        show(ui, &app);
        return Ok(Outcome::Done);
    }

    let source = match (&args.source, &args.asset) {
        (source, None) => source.clone(),
        (Some(source), Some(pattern)) => Some(update::with_asset_pattern(source, pattern)?),
        // A pattern alone goes with the source the application has.
        (None, Some(pattern)) => {
            let current = app
                .update_source
                .as_deref()
                .or(app.origin.as_deref().filter(|origin| download::is_url(origin)))
                .unwrap_or(update::MANUAL);
            Some(update::with_asset_pattern(current, pattern)?)
        }
    };
    if !update::set_update_source(&app, source.as_deref())? {
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
        UpdateSource::ForgeRelease { tag: Some(tag), pattern, .. } => {
            let file = match pattern {
                Some(pattern) => format!(", the asset matching {pattern}"),
                None => String::new(),
            };
            format!("{}, the release tagged {tag}{file}", source.describe())
        }
        UpdateSource::ForgeRelease { tag: None, pattern: Some(pattern), .. } => {
            format!("{}, the newest release with an asset matching {pattern}", source.describe())
        }
        UpdateSource::ForgeRelease { tag: None, pattern: None, .. } => {
            format!("{}, the newest release that has this AppImage", source.describe())
        }
        UpdateSource::DirectUrl { url } => url.clone(),
        UpdateSource::Manual => "nothing, it is updated manually".to_string(),
    }
}

fn is_embedded(source: &UpdateSource) -> bool {
    matches!(source, UpdateSource::Zsync { .. } | UpdateSource::GitHubZsync { .. })
}
