use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use appimg_core::digest::Verified;
use appimg_core::install::{IconChoice, InstallRequest};
use appimg_core::metadata::{AppImageInfo, Reading};
use appimg_core::remote::Remote;
use appimg_core::update::{self, UpdateSource};
use appimg_core::{archive, download, install, list, metadata, Paths};
use tempfile::TempDir;

use crate::cli::{EntryArgs, InstallArgs};
use crate::ui::{human_size, Ui};
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &InstallArgs) -> Result<Outcome> {
    // Something that is no update source is refused before anything is
    // downloaded.
    check_entry_args(&args.entry)?;
    // The temporary directory has to outlive the installation, a downloaded
    // AppImage lives in it until it has been copied into place.
    let (source, origin, remote, _scratch) = resolve_source(ui, &args.source)?;

    if !metadata::looks_like_appimage(&source) {
        bail!("{} does not look like an AppImage", source.display());
    }

    let info = read_metadata(&source, args.dry_run)?;
    if info.extract_root().is_none() {
        if args.dry_run {
            plan_without_metadata(ui, &info, "install");
        } else if !confirm_without_metadata(ui, &info, "Install")? {
            ui.info("Nothing was installed.");
            return Ok(Outcome::NothingToDo);
        }
    }

    let mut request = InstallRequest::from_info(&source, &origin, &info);
    request.remote = remote;
    apply_overrides(&mut request, &args.entry)?;
    if request.name.trim().is_empty() {
        bail!("no name could be determined, pass --name");
    }
    if args.entry.update_source.is_none() {
        offer_suggested_source(ui, &mut request, &info, args.dry_run)?;
    }
    apply_asset(&mut request, &args.entry)?;

    let plan = install::plan(paths, &request)?;
    if args.dry_run {
        print_plan(ui, &plan);
        return Ok(Outcome::Done);
    }

    if plan.already_installed {
        let question =
            format!("{:?} is already installed as {:?}. Replace it?", request.name, plan.slug);
        if !ui.confirm(&question, false)? {
            ui.info("Nothing was changed.");
            return Ok(Outcome::NothingToDo);
        }
        request.overwrite = true;
    }

    let outcome = install::install(paths, &request)?;

    ui.info(&format!(
        "{} {} as {}",
        if outcome.replaced { "Replaced" } else { "Installed" },
        ui.bold(&request.name),
        ui.accent(&outcome.slug)
    ));
    ui.info(&format!("  binary  {}", outcome.appimage_path.display()));
    ui.info(&format!("  entry   {}", outcome.desktop_entry_path.display()));
    match outcome.icons.len() {
        0 => ui.info(&format!("  icon    {} (no icon found)", install::FALLBACK_ICON)),
        count => ui.info(&format!("  icons   {count} installed into the hicolor theme")),
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
    for warning in &outcome.validation_warnings {
        ui.warn(warning);
    }

    Ok(Outcome::Done)
}

/// Reads the metadata of the AppImage to install or adopt. A dry run never
/// runs it, see [`Reading`]: without `unsquashfs` it reads nothing. Without
/// `--dry-run`, the command is the install the user asked for, which may
/// run the AppImage when `unsquashfs` cannot read it.
pub(crate) fn read_metadata(source: &Path, dry_run: bool) -> Result<AppImageInfo> {
    let reading = if dry_run { Reading::WithoutRunning } else { Reading::MayRun };
    Ok(metadata::inspect(source, appimg_core::current_locale().as_deref(), reading)?)
}

/// Refuses an update source, or an asset pattern, that is none, before
/// anything is downloaded or run.
pub(crate) fn check_entry_args(args: &EntryArgs) -> Result<()> {
    if let Some(source) = &args.update_source {
        update::parse_update_source(source)?;
    }
    if let Some(pattern) = &args.asset {
        update::check_asset_pattern(pattern)?;
        if let Some(source) = &args.update_source {
            update::with_asset_pattern(source, pattern)?;
        }
    }
    Ok(())
}

/// Keeps the pattern `--asset` gave with the update source the request
/// ended up with, which has to follow GitHub releases.
pub(crate) fn apply_asset(request: &mut InstallRequest, args: &EntryArgs) -> Result<()> {
    if let Some(pattern) = &args.asset {
        let source = request.update_source.as_deref().unwrap_or(update::MANUAL);
        request.update_source = Some(update::with_asset_pattern(source, pattern)?);
    }
    Ok(())
}

/// Asks about the update source the AppStream metadata inside the AppImage
/// suggests, yes by default, the way the TUI prefills it. `--yes` takes it
/// and says so. With nobody to ask, on a pipe, it is left out and the flag
/// that sets it is named, since nobody confirmed it.
pub(crate) fn offer_suggested_source(
    ui: &Ui,
    request: &mut InstallRequest,
    info: &AppImageInfo,
    dry_run: bool,
) -> Result<()> {
    let Some(suggested) = install::suggested_update_source(request, info) else {
        return Ok(());
    };
    let found = format!("The AppStream metadata links {suggested}");

    let take = if ui.assumes_yes() || dry_run {
        ui.info(&format!(
            "{found}, so updates come from its releases. --update-source sets another source."
        ));
        true
    } else if ui.is_interactive() {
        ui.confirm(&format!("{found}. Update from its releases?"), true)?
    } else {
        ui.info(&format!("{found}. Pass --update-source {suggested} to update from its releases."));
        false
    };
    if take {
        request.update_source = Some(suggested);
    }
    Ok(())
}

/// Turns the argument into a local AppImage. URLs are downloaded, and an
/// archive is unpacked, into a temporary directory that the caller keeps
/// alive. The origin is what the user gave, archive or not. A download
/// comes with what the server said about it.
fn resolve_source(
    ui: &Ui,
    source: &str,
) -> Result<(PathBuf, String, Option<Remote>, Option<TempDir>)> {
    if !download::is_url(source) {
        let path = PathBuf::from(source);
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        let absolute = path.canonicalize().unwrap_or(path);
        let origin = absolute.to_string_lossy().into_owned();
        if let Some((dest, scratch)) = unpack_local(ui, &absolute)? {
            return Ok((dest, origin, None, Some(scratch)));
        }
        // The same checks a download gets, before the metadata is read,
        // which can run the file.
        install::check_file(&absolute)?;
        return Ok((absolute, origin, None, None));
    }

    // Against the digest its release publishes, before anything reads the
    // metadata, which can run the file.
    let (dest, scratch, remote) =
        download_appimage(ui, source, &|file| install::verify_download(file, source))?;
    Ok((dest, source.to_string(), Some(remote), Some(scratch)))
}

/// Takes the AppImage out of a file on disk that is an archive, by its
/// bytes whatever its name says, into a temporary directory that the caller
/// keeps alive, and gives it the checks a download gets, see
/// [`install::unpack_archive`]. `None` for a file that is no archive.
pub(crate) fn unpack_local(ui: &Ui, file: &Path) -> Result<Option<(PathBuf, TempDir)>> {
    if archive::Kind::of(file).is_none() {
        return Ok(None);
    }
    let scratch = scratch_dir("appimg-unpack-")?;
    let (dest, extracted) = install::unpack_archive(file, scratch.path())?;
    ui.info(&format!("Took {} out of {}", extracted.entry, file.display()));
    Ok(Some((dest, scratch)))
}

fn scratch_dir(prefix: &str) -> Result<TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .context("cannot create a temporary directory for the download")
}

/// Downloads an AppImage, or an archive with one inside, into a temporary
/// directory that the caller keeps alive, see
/// [`download::appimage_or_archive`]: checked against what its release
/// publishes with `verify`, the archive before it is unpacked, then
/// complete, flushed, and an ELF file at least as long as its squashfs says.
///
/// The file is named after what the server named it, which for a link that
/// redirects to the current version is where its version is: the name and
/// version a file name gives come from there when the metadata says none.
pub(crate) fn download_appimage(
    ui: &Ui,
    url: &str,
    verify: &dyn Fn(&Path) -> appimg_core::Result<Option<Verified>>,
) -> Result<(PathBuf, TempDir, Remote)> {
    let scratch = scratch_dir("appimg-download-")?;
    let dest = scratch.path().join(download::file_name_from_url(url));

    ui.info(&format!("Downloading {}", ui.accent(url)));
    let mut progress = ui.progress();
    let fetched = download::appimage_or_archive(
        url,
        &dest,
        verify,
        Some(&mut |done, total| progress.update(done, total)),
    )?;
    progress.finish();
    ui.info(&format!("  {} downloaded", human_size(fetched.bytes)));
    if let Some(verified) = &fetched.verified {
        ui.info(&format!("  {}", ui.dim(&verified.describe())));
    }
    if let Some(entry) = &fetched.unpacked {
        ui.info(&format!("  took {entry} out of the archive"));
    }
    let dest = match fetched.remote.name.as_deref().filter(|_| fetched.unpacked.is_none()) {
        Some(name) => {
            let named = scratch.path().join(download::file_name_from_url(name));
            std::fs::rename(&dest, &named)
                .with_context(|| format!("cannot rename the download to {}", named.display()))?;
            named
        }
        None => dest,
    };
    Ok((dest, scratch, fetched.remote))
}

/// Extraction failed, so name, icon and categories would be guesses. Say
/// exactly what went wrong and let the user decide, unless --yes already
/// decided. `verb` is what is about to happen, `Install` or `Adopt`.
pub(crate) fn confirm_without_metadata(
    ui: &Ui,
    info: &appimg_core::AppImageInfo,
    verb: &str,
) -> Result<bool> {
    ui.warn("the AppImage did not extract, so its name, icon and categories are unknown:");
    for problem in &info.extract_problems {
        ui.warn(&format!("  {problem}"));
    }
    ui.info(&format!(
        "Going ahead uses the name {:?} and the generic icon. Passing --name and --icon \
         instead gives the entry the values you want.",
        info.name.clone().unwrap_or_default()
    ));

    ui.confirm(&format!("{verb} without the embedded metadata?"), false)
}

/// A dry run read no metadata. Says why, what the plan goes by instead, and
/// how the real thing gets it. `command` is `install` or `adopt`.
pub(crate) fn plan_without_metadata(ui: &Ui, info: &AppImageInfo, command: &str) {
    ui.warn(
        "the metadata inside the AppImage was not read, so its name, icon and categories are \
         unknown:",
    );
    for problem in &info.extract_problems {
        ui.warn(&format!("  {problem}"));
    }
    ui.info(&format!(
        "The plan uses the name {:?}, from the file name, and the generic icon, unless --name and \
         --icon say otherwise. Without --dry-run, {command} reads the metadata by running the \
         AppImage when unsquashfs cannot.",
        info.name.clone().unwrap_or_default()
    ));
}

pub(crate) fn apply_overrides(request: &mut InstallRequest, args: &EntryArgs) -> Result<()> {
    if let Some(name) = &args.name {
        request.name = name.clone();
    }
    if let Some(comment) = &args.comment {
        request.comment = Some(comment.clone());
    }
    if !args.categories.is_empty() {
        request.categories = args
            .categories
            .iter()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
    }
    if let Some(extra) = &args.args {
        request.extra_args = split_args(extra);
    }
    if args.terminal {
        request.terminal = true;
    }
    if let Some(icon) = &args.icon {
        if !icon.is_file() {
            bail!("{} is not a file", icon.display());
        }
        request.icon = IconChoice::File(icon.clone());
    }
    if let Some(source) = &args.update_source {
        request.update_source = Some(update::parse_update_source(source)?);
    }
    Ok(())
}

/// Splits a launch argument string on whitespace, honouring quotes.
pub fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;

    for character in input.chars() {
        match (quote, character) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'') | (None, '"') => {
                quote = Some(character);
                has_token = true;
            }
            (None, c) if c.is_whitespace() => {
                if has_token || !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if has_token || !current.is_empty() {
        args.push(current);
    }
    args
}

fn print_plan(ui: &Ui, plan: &install::InstallPlan) {
    ui.info(&format!("Would install as {}", ui.accent(&plan.slug)));
    ui.info(&format!("  binary  {}", plan.appimage_path.display()));
    ui.info(&format!("  entry   {}", plan.desktop_entry_path.display()));
    if plan.already_installed {
        ui.warn("a version of this application is already installed and would be replaced");
    }
    ui.info("");
    ui.info(&ui.dim(&plan.desktop_entry.to_string()));
}

#[cfg(test)]
mod tests {
    use super::split_args;

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(split_args("--foo --bar"), vec!["--foo", "--bar"]);
        assert_eq!(split_args("  --foo   --bar  "), vec!["--foo", "--bar"]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn keeps_quoted_arguments_together() {
        assert_eq!(
            split_args("--flag \"two words\" --other='a b'"),
            vec!["--flag", "two words", "--other=a b"]
        );
        assert_eq!(split_args("\"\""), vec![""]);
    }
}
