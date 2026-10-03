use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use appimg_core::adopt::{self, AdoptPlan, Placement, Transfer};
use appimg_core::install::InstallRequest;
use appimg_core::update::{self, UpdateSource};
use appimg_core::{install, list, metadata, Paths};

use crate::cli::AdoptArgs;
use crate::commands::install::{
    apply_asset, apply_overrides, check_entry_args, confirm_without_metadata,
    offer_suggested_source,
};
use crate::ui::{human_size, Ui};
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &AdoptArgs) -> Result<Outcome> {
    if args.scan {
        return scan(paths, ui);
    }
    let Some(path) = &args.path else {
        bail!("name the AppImage to adopt, or pass --scan");
    };
    // Something that is no update source is refused before anything else.
    check_entry_args(&args.entry)?;

    // Everything that can be said without running the file comes first:
    // reading the metadata runs it.
    let source = adopt::check(paths, path)?;
    let info = metadata::inspect(&source, appimg_core::current_locale().as_deref())?;
    if info.extract_root().is_none() && !confirm_without_metadata(ui, &info, "Adopt")? {
        ui.info("Nothing was adopted.");
        return Ok(Outcome::NothingToDo);
    }

    let origin = source.to_string_lossy().into_owned();
    let mut request = InstallRequest::from_info(&source, &origin, &info);
    apply_overrides(&mut request, &args.entry)?;
    if request.name.trim().is_empty() {
        bail!("no name could be determined, pass --name");
    }
    if args.entry.update_source.is_none() {
        offer_suggested_source(ui, &mut request, &info, args.dry_run)?;
    }
    apply_asset(&mut request, &args.entry)?;

    let transfer = if args.copy { Transfer::Copy } else { Transfer::Move };
    let plan = adopt::plan(paths, &request, transfer, in_local_bin(&source))?;
    if args.dry_run {
        if args.keep_entries {
            plan.check_slug(false)?;
        }
        print_plan(ui, &plan);
        return Ok(Outcome::Done);
    }

    let remove = remove_foreign(ui, &plan, args.keep_entries)?;
    let outcome = adopt::adopt(paths, &plan, &request, remove)?;

    ui.info(&format!("Adopted {} as {}", ui.bold(&request.name), ui.accent(&outcome.slug)));
    ui.info(&format!("  binary  {}", outcome.appimage_path.display()));
    let from = plan.source.display();
    match outcome.placement {
        Placement::Moved { across_filesystems: false } => {
            ui.info(&format!("          moved from {from}"))
        }
        Placement::Moved { across_filesystems: true } => ui.info(&format!(
            "          moved from {from}, across filesystems: copied, checked, then deleted"
        )),
        Placement::Copied => {
            ui.info(&format!("          copied from {from}, which stays where it is"))
        }
        Placement::InPlace => ui.info("          already in place"),
    }
    ui.info(&format!("  entry   {}", outcome.desktop_entry_path.display()));
    if outcome.replaced.is_some() {
        ui.info("          written over the one from elsewhere that was there");
    }
    match outcome.icons.len() {
        0 => ui.info(&format!("  icon    {} (no icon found)", install::FALLBACK_ICON)),
        count => ui.info(&format!("  icons   {count} installed into the hicolor theme")),
    }
    if let Some(link) = &outcome.link {
        ui.info(&format!(
            "  link    {} -> {}, so the command keeps working and runs the adopted file",
            link.display(),
            outcome.appimage_path.display()
        ));
    }
    for file in &outcome.removed {
        // An old icon under the slug whose path an adopted icon took is not
        // gone.
        let what = if outcome.icons.contains(file) { "replaced" } else { "removed" };
        ui.info(&format!("  {what} {}", file.display()));
    }
    let installed = list::find(paths, &outcome.slug)?;
    match update::source_for(&installed) {
        UpdateSource::Manual => ui.info(&format!(
            "  updates manually, set a source with: appimg update-source {} \
             <URL|github:owner/repo>",
            outcome.slug
        )),
        source => ui.info(&format!("  updates from {}", source.describe())),
    }

    if let Some(why) = &outcome.original_left {
        ui.warn(&format!(
            "could not delete {from}: {why}. The adopted copy is complete, the original is still \
             there"
        ));
    }
    if let Some(why) = &outcome.link_failed {
        ui.warn(&format!("could not leave a link at {from}: {why}"));
    }
    for (file, why) in &outcome.not_removed {
        ui.warn(&format!("could not remove {}: {why}", file.display()));
    }
    let by_launcher = plan.foreign.iter().any(|entry| entry.by_appimagelauncher());
    if remove && by_launcher && outcome.placement == Placement::Copied {
        ui.warn(&format!(
            "the original stays in {}, where AppImageLauncher may list it again",
            plan.source.parent().unwrap_or(Path::new("/")).display()
        ));
    }
    for warning in &outcome.validation_warnings {
        ui.warn(warning);
    }
    Ok(Outcome::Done)
}

/// Whether the file sits in `~/.local/bin`, where a command runs it by
/// name. Moving it away would take that command along, so a link stays.
fn in_local_bin(source: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return false;
    };
    let bin = PathBuf::from(home).join(".local/bin");
    match (bin.canonicalize(), source.parent()) {
        (Ok(bin), Some(dir)) => dir == bin,
        _ => false,
    }
}

/// Shows the desktop entries from elsewhere that launch the file, with the
/// icons that would go with them, and asks whether they go, yes by default:
/// left alone, the launcher lists the application twice. `--yes` takes
/// them away and says so, `--keep-entries` keeps them without asking. With
/// nobody to ask, on a pipe, they stay and the flags are named.
fn remove_foreign(ui: &Ui, plan: &AdoptPlan, keep: bool) -> Result<bool> {
    if plan.foreign.is_empty() {
        return Ok(false);
    }
    let count = plan.foreign.len();
    ui.info(&format!(
        "{} from elsewhere already {} {}:",
        if count == 1 { "A desktop entry".to_string() } else { format!("{count} desktop entries") },
        if count == 1 { "launches" } else { "launch" },
        plan.source.display()
    ));
    describe_foreign(ui, plan);

    if keep {
        ui.info("They stay, as --keep-entries says.");
        return Ok(false);
    }
    if ui.assumes_yes() {
        ui.info("They go, so the launcher lists the application once.");
        return Ok(true);
    }
    if ui.is_interactive() {
        return ui.confirm("Remove them, so the launcher does not list it twice?", true);
    }
    ui.info("They stay. Pass --yes to remove them, or --keep-entries to keep them without asking.");
    Ok(false)
}

fn describe_foreign(ui: &Ui, plan: &AdoptPlan) {
    for entry in &plan.foreign {
        ui.info(&format!("  {}", entry.path.display()));
        if plan.replaces.as_ref() == Some(&entry.path) {
            let icons_too = if entry.icons.is_empty() { "" } else { ", its icons below too" };
            ui.info(&format!(
                "    is where the adopted entry goes: replaced if they go{icons_too}; while it \
                 stays {:?} is taken",
                plan.install.slug
            ));
        }
        for icon in &entry.icons {
            ui.info(&format!("    with its icon {}", icon.display()));
        }
        if let Some(why) = &entry.icon_stays {
            ui.info(&format!("    {why}"));
        }
    }
}

fn print_plan(ui: &Ui, plan: &AdoptPlan) {
    ui.info(&format!("Would adopt as {}", ui.accent(&plan.install.slug)));
    ui.info(&format!("  binary  {}", plan.install.appimage_path.display()));
    let from = plan.source.display();
    match (plan.in_place, plan.transfer) {
        (true, _) => ui.info("          already in place"),
        (false, Transfer::Move) => ui.info(&format!("          moved from {from}")),
        (false, Transfer::Copy) => {
            ui.info(&format!("          copied from {from}, which stays where it is"))
        }
    }
    if plan.link_back {
        ui.info(&format!("  link    {from} -> {}", plan.install.appimage_path.display()));
    }
    ui.info(&format!("  entry   {}", plan.install.desktop_entry_path.display()));
    if plan.replaces.is_some() {
        ui.info("          written over the one from elsewhere that is there, if that one goes");
    }
    if !plan.foreign.is_empty() {
        ui.info("Desktop entries from elsewhere that launch it, which it would offer to remove:");
        describe_foreign(ui, plan);
    }
    ui.info("");
    ui.info(&ui.dim(&plan.install.desktop_entry.to_string()));
}

/// Lists what could be adopted, and the command that does it, for each.
/// Reads, never writes, and never runs any of the files.
fn scan(paths: &Paths, ui: &Ui) -> Result<Outcome> {
    let mut dirs = vec![paths.appimage_dir.clone()];
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        let home = PathBuf::from(home);
        dirs.push(home.join("Applications"));
        dirs.push(home.join(".local/bin"));
    }
    let candidates = adopt::scan(paths, &dirs)?;
    let looked = dirs.iter().map(|dir| dir.display().to_string()).collect::<Vec<_>>().join(", ");

    if candidates.is_empty() {
        ui.info(&format!("Nothing to adopt in {looked}."));
        return Ok(Outcome::NothingToDo);
    }
    let adoptable = candidates.iter().filter(|candidate| candidate.unfit.is_none()).count();
    ui.info(&format!(
        "{adoptable} of {} AppImages in {looked} could be adopted:",
        candidates.len()
    ));
    for candidate in &candidates {
        ui.info("");
        ui.info(&format!(
            "{}  {}",
            ui.bold(&candidate.path.display().to_string()),
            human_size(candidate.size)
        ));
        for entry in &candidate.entries {
            ui.info(&format!("  launched by {}", entry.display()));
        }
        match &candidate.unfit {
            Some(unfit) => ui.info(&format!("  cannot be adopted: {unfit}")),
            None => ui.info(&format!(
                "  {}",
                ui.accent(&format!("appimg adopt {}", shell_quote(&candidate.path)))
            )),
        }
    }
    Ok(if adoptable > 0 { Outcome::Done } else { Outcome::NothingToDo })
}

/// A path as a shell takes it, quoted only when it has to be.
fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    let plain = !text.is_empty()
        && text.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c));
    if plain {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::shell_quote;
    use std::path::Path;

    #[test]
    fn a_path_is_quoted_only_when_a_shell_needs_it() {
        assert_eq!(
            shell_quote(Path::new("/home/u/Applications/App-1.0.AppImage")),
            "/home/u/Applications/App-1.0.AppImage"
        );
        assert_eq!(shell_quote(Path::new("/home/u/My App.AppImage")), "'/home/u/My App.AppImage'");
        assert_eq!(shell_quote(Path::new("/home/u/it's.AppImage")), "'/home/u/it'\\''s.AppImage'");
        assert_eq!(shell_quote(Path::new("/home/u/$(x).AppImage")), "'/home/u/$(x).AppImage'");
    }
}
