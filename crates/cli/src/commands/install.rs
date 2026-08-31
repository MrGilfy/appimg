use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use appimg_core::install::{IconChoice, InstallRequest};
use appimg_core::{download, install, metadata, Paths};
use tempfile::TempDir;

use crate::cli::InstallArgs;
use crate::ui::{human_size, Ui};
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &InstallArgs) -> Result<Outcome> {
    // The temporary directory has to outlive the installation, a downloaded
    // AppImage lives in it until it has been copied into place.
    let (source, origin, _scratch) = resolve_source(ui, &args.source)?;

    if !metadata::looks_like_appimage(&source) {
        bail!("{} does not look like an AppImage", source.display());
    }

    let info = metadata::inspect(&source, appimg_core::current_locale().as_deref())?;
    if info.extract_root().is_none() && !confirm_without_metadata(ui, &info)? {
        ui.info("Nothing was installed.");
        return Ok(Outcome::NothingToDo);
    }

    let mut request = InstallRequest::from_info(&source, &origin, &info);
    apply_overrides(&mut request, args)?;
    if request.name.trim().is_empty() {
        bail!("no name could be determined, pass --name");
    }

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
    for warning in &outcome.validation_warnings {
        ui.warn(warning);
    }

    Ok(Outcome::Done)
}

/// Turns the argument into a local file. URLs are downloaded into a
/// temporary directory that the caller keeps alive.
fn resolve_source(ui: &Ui, source: &str) -> Result<(PathBuf, String, Option<TempDir>)> {
    if !download::is_url(source) {
        let path = PathBuf::from(source);
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        let absolute = path.canonicalize().unwrap_or(path);
        let origin = absolute.to_string_lossy().into_owned();
        return Ok((absolute, origin, None));
    }

    let scratch = tempfile::Builder::new()
        .prefix("appimg-download-")
        .tempdir()
        .context("cannot create a temporary directory for the download")?;
    let dest = scratch.path().join(download::file_name_from_url(source));

    ui.info(&format!("Downloading {}", ui.accent(source)));
    let mut progress = ui.progress();
    let bytes =
        download::to_file(source, &dest, Some(&mut |done, total| progress.update(done, total)))?;
    progress.finish();
    ui.info(&format!("  {} downloaded", human_size(bytes)));

    Ok((dest, source.to_string(), Some(scratch)))
}

/// Extraction failed, so name, icon and categories would be guesses. Say
/// exactly what went wrong and let the user decide, unless --yes already
/// decided.
fn confirm_without_metadata(ui: &Ui, info: &appimg_core::AppImageInfo) -> Result<bool> {
    ui.warn("the AppImage did not extract, so its name, icon and categories are unknown:");
    for problem in &info.extract_problems {
        ui.warn(&format!("  {problem}"));
    }
    ui.info(&format!(
        "Installing anyway uses the name {:?} and the generic icon. Passing --name and --icon \
         instead gives the entry the values you want.",
        info.name.clone().unwrap_or_default()
    ));

    ui.confirm("Install without the embedded metadata?", false)
}

fn apply_overrides(request: &mut InstallRequest, args: &InstallArgs) -> Result<()> {
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
