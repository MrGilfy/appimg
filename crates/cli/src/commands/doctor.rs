use anyhow::Result;
use appimg_core::doctor::{self, DoctorReport};
use appimg_core::Paths;

use crate::ui::Ui;
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui) -> Result<Outcome> {
    let report = doctor::run(paths)?;

    ui.info(&ui.bold("Environment"));
    check(ui, report.libfuse2, "libfuse2", "most AppImages refuse to run without it");
    check(
        ui,
        report.xdg_data_home_in_search_path,
        "XDG_DATA_DIRS",
        "the desktop may not read the directories appimg writes to",
    );
    check(
        ui,
        report.applications_dir_writable,
        "applications directory writable",
        "installing will fail",
    );

    ui.info("");
    ui.info(&ui.bold("Tools"));
    for tool in &report.required_tools {
        check(ui, tool.found, &tool.name, &tool.consequence);
    }

    ui.info("");
    ui.info(&ui.bold("Optional tools"));
    for tool in &report.optional_tools {
        // Nothing here is a problem, so none of it sets the exit code.
        if tool.found {
            ui.info(&format!("  {} {}", mark(ui, true), tool.name));
        } else {
            ui.info(&format!(
                "  {} {} is not installed: {}",
                ui.dim("--  "),
                tool.name,
                tool.consequence
            ));
        }
    }

    ui.info("");
    ui.info(&ui.bold("Installed files"));
    report_leftovers(ui, &report);

    ui.info("");
    if report.is_clean() {
        ui.info("Everything looks fine.");
        return Ok(Outcome::Done);
    }

    ui.info("Some checks need attention, see above.");
    Ok(Outcome::NothingToDo)
}

fn report_leftovers(ui: &Ui, report: &DoctorReport) {
    if report.broken_entries.is_empty()
        && report.orphaned_icons.is_empty()
        && report.leftover_files.is_empty()
    {
        ui.info(&format!("  {} nothing left behind by appimg", mark(ui, true)));
        return;
    }

    for (slug, path) in &report.broken_entries {
        ui.info(&format!(
            "  {} {slug}: the entry {} has no working AppImage, remove it with `appimg remove {slug}`",
            mark(ui, false),
            path.display()
        ));
    }
    for icon in &report.orphaned_icons {
        ui.info(&format!(
            "  {} icon no longer used by its entry: {}",
            mark(ui, false),
            icon.display()
        ));
    }
    for file in &report.leftover_files {
        ui.info(&format!(
            "  {} leftover from an interrupted update: {}",
            mark(ui, false),
            file.display()
        ));
    }
}

fn check(ui: &Ui, ok: bool, what: &str, consequence: &str) {
    if ok {
        ui.info(&format!("  {} {what}", mark(ui, true)));
    } else {
        ui.info(&format!("  {} {what}: {consequence}", mark(ui, false)));
    }
}

fn mark(ui: &Ui, ok: bool) -> String {
    if ok {
        ui.accent("ok  ")
    } else {
        ui.bold("miss")
    }
}
