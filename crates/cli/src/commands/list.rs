use anyhow::Result;
use appimg_core::json::escape;
use appimg_core::list::{Health, InstalledApp};
use appimg_core::{list, update, Paths};

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
    match app.health {
        Health::Ok => ui.dim(&update::source_for(app).describe()),
        Health::MissingBinary => ui.bold("broken: binary missing"),
        Health::Incomplete => ui.bold("broken: entry incomplete"),
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
            "\"size_bytes\":{size},\"health\":\"{health}\",\"update_source\":\"{update_source}\"}}"
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
        update_source = update::source_for(app).describe(),
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
