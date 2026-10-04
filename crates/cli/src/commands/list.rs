use anyhow::Result;
use appimg_core::hold::Checked;
use appimg_core::json::escape;
use appimg_core::list::{Health, InstalledApp};
use appimg_core::{date, list, update, Paths};

use crate::cli::ListArgs;
use crate::ui::{human_size, table, Ui};
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &ListArgs) -> Result<Outcome> {
    let apps = list::list(paths)?;

    if args.json {
        ui.info(&to_json(&apps));
        return Ok(Outcome::Done);
    }

    if apps.is_empty() {
        ui.info("Nothing installed yet. Try: appimg install <PATH|URL>");
        return Ok(Outcome::NothingToDo);
    }

    let rows: Vec<Vec<String>> = apps
        .iter()
        .map(|app| {
            vec![
                app.name.clone(),
                app.version.clone().unwrap_or_else(|| "-".to_string()),
                app.size_bytes.map(human_size).unwrap_or_else(|| "-".to_string()),
                first_category(app),
                describe_health(ui, app),
            ]
        })
        .collect();

    ui.info(&table(ui, &["NAME", "VERSION", "SIZE", "CATEGORY", "STATUS"], &rows));
    Ok(Outcome::Done)
}

fn first_category(app: &InstalledApp) -> String {
    app.categories.first().cloned().unwrap_or_else(|| "-".to_string())
}

fn describe_health(ui: &Ui, app: &InstalledApp) -> String {
    match (app.health, &app.hold) {
        // Held, and whether the last check found an update it keeps back.
        (Health::Ok, Some(hold)) if hold.holds_back_an_update() => ui.accent(&hold.describe()),
        (Health::Ok, Some(hold)) => ui.dim(&hold.describe()),
        (Health::Ok, None) => ui.dim(&update::source_for(app).describe()),
        (Health::MissingBinary, _) => ui.bold("broken: binary missing"),
        (Health::Incomplete, _) => ui.bold("broken: entry incomplete"),
    }
}

fn to_json(apps: &[InstalledApp]) -> String {
    let items: Vec<String> = apps.iter().map(app_to_json).collect();
    format!("[{}]", items.join(","))
}

fn app_to_json(app: &InstalledApp) -> String {
    let categories: Vec<String> =
        app.categories.iter().map(|c| format!("\"{}\"", escape(c))).collect();

    format!(
        concat!(
            "{{\"slug\":\"{slug}\",\"name\":\"{name}\",\"comment\":{comment},",
            "\"version\":{version},\"categories\":[{categories}],\"source\":{source},",
            "\"update_info\":{update_info},\"installed_at\":{installed_at},",
            "\"appimage\":\"{appimage}\",\"desktop_entry\":\"{entry}\",",
            "\"size_bytes\":{size},\"health\":\"{health}\",\"update_source\":\"{update_source}\",",
            "\"held\":{held},\"hold_check\":{hold_check}}}"
        ),
        slug = escape(&app.slug),
        name = escape(&app.name),
        comment = optional(app.comment.as_deref()),
        version = optional(app.version.as_deref()),
        categories = categories.join(","),
        source = optional(app.origin.as_deref()),
        update_info = optional(app.update_info.as_deref()),
        installed_at = optional(app.installed_at.as_deref()),
        appimage = escape(&app.appimage_path.to_string_lossy()),
        entry = escape(&app.desktop_entry_path.to_string_lossy()),
        size = app.size_bytes.map(|s| s.to_string()).unwrap_or_else(|| "null".to_string()),
        health = health_name(app.health),
        update_source = escape(&update::source_for(app).describe()),
        held = app.hold.is_some(),
        hold_check = hold_check_to_json(app.hold.as_ref().and_then(|hold| hold.checked.as_ref())),
    )
}

/// What the last check of a held application found, `null` when it is not
/// held or was not checked since.
fn hold_check_to_json(checked: Option<&Checked>) -> String {
    let Some(checked) = checked else {
        return "null".to_string();
    };
    format!(
        "{{\"checked_at\":{at},\"found\":\"{found}\",\"latest_version\":{latest}}}",
        at = optional(date::from_seconds(checked.at).as_deref()),
        found = checked.found.word(),
        latest = optional(checked.latest.as_deref()),
    )
}

fn health_name(health: Health) -> &'static str {
    match health {
        Health::Ok => "ok",
        Health::MissingBinary => "missing-binary",
        Health::Incomplete => "incomplete",
    }
}

fn optional(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{}\"", escape(value)),
        None => "null".to_string(),
    }
}
