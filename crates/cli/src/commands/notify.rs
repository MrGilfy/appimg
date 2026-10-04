use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use appimg_core::notify::{self, Announced, Notification, Notifier, Units};
use appimg_core::update::UpdateStatus;
use appimg_core::{fs_util, list, Error, Paths};

use crate::cli::{NotifyAction, NotifyArgs};
use crate::commands::update;
use crate::ui::Ui;
use crate::Outcome;

/// Where a user reads what a check logged.
const JOURNAL: &str = "journalctl --user -u appimg-notify.service";

pub fn run(paths: &Paths, ui: &Ui, args: &NotifyArgs) -> Result<Outcome> {
    match args.action {
        NotifyAction::Enable => enable(paths, ui),
        NotifyAction::Disable => disable(paths, ui),
        NotifyAction::Status => status(paths, ui),
        NotifyAction::Test => test(ui),
        NotifyAction::Check => check(paths, ui),
    }
}

fn enable(paths: &Paths, ui: &Ui) -> Result<Outcome> {
    // A timer whose notification cannot be shown, or that nothing can
    // start, is no use: both are there before anything is written.
    let notifier = Notifier::find()?;
    if !notify::has_systemctl() {
        return Err(Error::NoSystemctl.into());
    }
    let binary = std::env::current_exe().context("cannot tell where this appimg is")?;

    let units = notify::write_units(paths, &binary)?;
    if let Err(error) = notify::systemctl(&["daemon-reload"])
        .and_then(|_| notify::systemctl(&["enable", "--now", notify::TIMER]))
    {
        bail!(
            "{error}\nThe units are written to {}, `appimg notify disable` removes them.",
            units.service.parent().unwrap_or(&units.service).display()
        );
    }

    ui.info(&format!(
        "Update notifications are on: appimg checks for updates once a day and shows them \
         through {}.",
        notifier.name()
    ));
    let mut rows = vec![
        ("Runs", format!("{} {}", binary.display(), notify::CHECK_ARGS.join(" "))),
        ("Apps in", paths.appimage_dir.display().to_string()),
    ];
    if let Some(next) = timer_state().ok().and_then(|timer| value(&timer, "NextElapseUSecRealtime"))
    {
        rows.push(("Next check", next));
    }
    rows.push(("Units", describe_units(&units)));
    print_rows(ui, &rows);

    if let Some(place) = notify::looks_temporary(&binary) {
        ui.warn(&format!(
            "the timer runs {}, which is in {place} and may not stay there. If it goes missing, \
             `appimg notify status` says so; `appimg notify enable` run from the appimg you keep \
             points the timer at that one.",
            binary.display()
        ));
    }
    Ok(Outcome::Done)
}

fn disable(paths: &Paths, ui: &Ui) -> Result<Outcome> {
    let units = Units::new(paths);
    if !units.exist() {
        ui.info("Update notifications are off already.");
        return Ok(Outcome::NothingToDo);
    }

    // The timer kept its record of announced updates wherever the unit
    // told it to, which need not be where this shell would.
    let state_home = notify::installed(paths)
        .and_then(|installed| installed.var("XDG_STATE_HOME").map(PathBuf::from))
        .unwrap_or_else(|| paths.state_home.clone());

    // Without systemctl there is no user manager that could have them
    // loaded, so the files are all there is.
    let systemd = notify::has_systemctl();
    if systemd {
        notify::systemctl(&["disable", "--now", notify::TIMER])?;
        // A check that is running right now ends here too.
        notify::systemctl(&["stop", notify::SERVICE])?;
    }
    notify::remove_units(paths)?;
    if systemd {
        notify::systemctl(&["daemon-reload"])?;
    }
    let mut removed = vec![units.service.clone(), units.timer.clone()];
    removed.extend(Announced::delete(&state_home)?);
    ui.info(&format!("Update notifications are off, removed {}.", and_list(&removed)));
    Ok(Outcome::Done)
}

fn status(paths: &Paths, ui: &Ui) -> Result<Outcome> {
    let units = Units::new(paths);
    if !units.exist() {
        ui.info("Update notifications are off. `appimg notify enable` turns them on.");
        return Ok(Outcome::Done);
    }

    let mut rows = Vec::new();
    match timer_state().and_then(|timer| Ok((timer, service_state()?))) {
        Ok((timer, service)) => {
            rows.push(("Timer", describe_timer(&timer)));
            let next = value(&timer, "NextElapseUSecRealtime");
            rows.push(("Next check", next.unwrap_or_else(|| "none".to_string())));
            rows.push(("Last check", describe_last_check(&timer, &service)));
        }
        Err(error) => rows.push(("Timer", format!("unknown, {error}"))),
    }

    let installed = notify::installed(paths);
    let missing = installed.as_ref().map(|i| &i.binary).filter(|b| !fs_util::is_executable(b));
    let runs = match &installed {
        None => format!(
            "unknown, {} is missing or was changed: `appimg notify enable` writes it again",
            units.service.display()
        ),
        Some(installed) if missing.is_some() => {
            format!("{}, which is missing", installed.binary.display())
        }
        Some(installed) => installed.binary.display().to_string(),
    };
    rows.push(("Runs", runs));
    if let Some(dir) = installed.as_ref().and_then(|installed| installed.var("APPIMG_DIR")) {
        rows.push(("Apps in", dir.to_string()));
    }
    rows.push((
        "Notifies with",
        match Notifier::find() {
            Ok(notifier) => notifier.name().to_string(),
            Err(error) => format!("nothing, {error}"),
        },
    ));
    rows.push(("Units", describe_units(&units)));
    print_rows(ui, &rows);

    if let Some(binary) = missing {
        bail!(
            "the timer runs {}, which no longer exists, so no check runs: `appimg notify enable` \
             run from the appimg you keep points the timer at that one",
            binary.display()
        );
    }
    Ok(Outcome::Done)
}

fn test(ui: &Ui) -> Result<Outcome> {
    let notifier = Notifier::find()?;
    notifier.send(&Notification::sample())?;
    ui.info(&format!("Sent a test notification through {}.", notifier.name()));
    Ok(Outcome::Done)
}

/// What the timer runs: the check `update --all --check` makes, and one
/// notification when it found a version no notification named before.
/// Everything it prints, warnings about checks that failed included, goes
/// to the journal. A failed check never shows a notification of its own,
/// and fails the run, so `status` can tell.
fn check(paths: &Paths, ui: &Ui) -> Result<Outcome> {
    // A held application is not checked, nor announced: what was announced
    // about it is forgotten, so its update is news again once it is
    // released.
    let (held, apps): (Vec<_>, Vec<_>) =
        list::list(paths)?.into_iter().partition(|app| app.hold.is_some());
    if !held.is_empty() {
        let names: Vec<&str> = held.iter().map(|app| app.name.as_str()).collect();
        ui.info(&format!("Held, not checked: {}.", names.join(", ")));
    }
    let (statuses, failed) = update::statuses(ui, &apps);
    let available: Vec<&UpdateStatus> = statuses.iter().filter(|s| s.available).collect();
    let mut announced = Announced::load(paths)?;
    let news = announced.unannounced(&available);

    if !news.is_empty() {
        Notifier::find()?.send(&Notification::for_updates(&news))?;
        ui.info(&format!("Notified about updates for {}.", names(&news)));
    } else if available.is_empty() {
        ui.info("No updates available, nothing to notify about.");
    } else {
        ui.info(&format!("Nothing new to notify about: {} announced already.", names(&available)));
    }
    // Only once the notification is out: one that failed is tried again.
    if announced.record(&statuses, &apps) {
        announced.save(paths)?;
    }

    if failed > 0 {
        bail!("{failed} of {} checks failed, see the warnings above", apps.len());
    }
    // Nothing to do is a successful run here, not exit code 3, which the
    // service would count as a failure.
    Ok(Outcome::Done)
}

fn names(statuses: &[&UpdateStatus]) -> String {
    statuses.iter().map(|status| status.name.as_str()).collect::<Vec<_>>().join(", ")
}

fn timer_state() -> Result<HashMap<String, String>> {
    let properties =
        ["LoadState", "ActiveState", "UnitFileState", "NextElapseUSecRealtime", "LastTriggerUSec"];
    Ok(notify::show(notify::TIMER, &properties)?)
}

fn service_state() -> Result<HashMap<String, String>> {
    let properties = [
        "ActiveState",
        "Result",
        "ExecMainStatus",
        "ExecMainStartTimestamp",
        "ExecMainExitTimestamp",
    ];
    Ok(notify::show(notify::SERVICE, &properties)?)
}

/// A property that has a value.
fn value(properties: &HashMap<String, String>, key: &str) -> Option<String> {
    properties.get(key).filter(|value| !value.is_empty()).cloned()
}

fn describe_timer(timer: &HashMap<String, String>) -> String {
    let get = |key| value(timer, key).unwrap_or_default();
    match (get("UnitFileState").as_str(), get("ActiveState").as_str()) {
        ("enabled", "active") => "on".to_string(),
        ("enabled", state) => {
            format!("enabled but {state}, `appimg notify enable` starts it")
        }
        _ if get("LoadState") == "not-found" => {
            "not loaded by systemd, `appimg notify enable` loads and starts it".to_string()
        }
        (file_state, _) => format!("off ({file_state}), `appimg notify enable` turns it on"),
    }
}

/// When the last check ran and how it ended. The result lives only as long
/// as the user manager; the time the timer last fired survives it.
fn describe_last_check(
    timer: &HashMap<String, String>,
    service: &HashMap<String, String>,
) -> String {
    if value(service, "ActiveState").as_deref() == Some("activating") {
        return "running now".to_string();
    }
    let Some(started) = value(service, "ExecMainStartTimestamp") else {
        return match value(timer, "LastTriggerUSec") {
            Some(at) => format!("{at}, how it went is in `{JOURNAL}`"),
            None => "never".to_string(),
        };
    };
    match value(service, "Result").as_deref() {
        Some("success") => format!("{started}, succeeded"),
        Some("exit-code") => format!(
            "{started}, failed with exit code {}, see `{JOURNAL}`",
            value(service, "ExecMainStatus").unwrap_or_else(|| "unknown".to_string())
        ),
        Some(result) => format!("{started}, failed ({result}), see `{JOURNAL}`"),
        None => format!("{started}, see `{JOURNAL}`"),
    }
}

fn describe_units(units: &Units) -> String {
    and_list(&[units.service.clone(), units.timer.clone()])
}

/// `a, b and c`.
fn and_list(paths: &[PathBuf]) -> String {
    let names: Vec<String> = paths.iter().map(|path| path.display().to_string()).collect();
    match names.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

fn print_rows(ui: &Ui, rows: &[(&str, String)]) {
    for (label, value) in rows {
        ui.info(&format!("{} {value}", ui.bold(&format!("{label:<14}"))));
    }
}
