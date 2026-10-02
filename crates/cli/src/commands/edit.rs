use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use appimg_core::desktop_entry::{self, DesktopEntry};
use appimg_core::{caches, fs_util, list, update, Paths};

use crate::cli::EditArgs;
use crate::ui::Ui;
use crate::Outcome;

/// Keys appimg owns. A user may edit anything else, but losing these would
/// turn the entry into something appimg no longer manages.
const MANAGED_KEYS: &[&str] = &[
    desktop_entry::KEY_MANAGED,
    desktop_entry::KEY_SLUG,
    desktop_entry::KEY_ORIGIN,
    desktop_entry::KEY_VERSION,
    desktop_entry::KEY_UPDATE_INFO,
    desktop_entry::KEY_UPDATE_SOURCE,
    desktop_entry::KEY_INSTALLED_AT,
];

/// The editors tried when neither `$VISUAL` nor `$EDITOR` names one that
/// exists, in this order.
const FALLBACK_EDITORS: &[&str] = &["nvim", "vim", "nano"];

/// What came out of an editing session.
pub struct Edited {
    pub changed: bool,
    /// Whatever `desktop-file-validate` said about the saved entry.
    pub warnings: Vec<String>,
}

pub fn run(paths: &Paths, ui: &Ui, args: &EditArgs) -> Result<Outcome> {
    if !ui.is_interactive() {
        bail!("editing needs a terminal");
    }

    let app = list::find(paths, &args.name)?;
    let edited = edit_entry(paths, &app.desktop_entry_path)?;
    if !edited.changed {
        ui.info("Nothing changed.");
        return Ok(Outcome::NothingToDo);
    }

    for warning in &edited.warnings {
        ui.warn(warning);
    }
    ui.info(&format!("Updated {}.", app.desktop_entry_path.display()));
    Ok(Outcome::Done)
}

/// Opens a desktop entry in the editor [`find_editor`] picks and writes it
/// back. The keys appimg owns survive even when the user deletes them. A
/// saved entry is validated again and the desktop database and the icon
/// cache are refreshed, because the name, the categories or the icon may
/// have changed.
pub fn edit_entry(paths: &Paths, entry_path: &Path) -> Result<Edited> {
    let original = DesktopEntry::read(entry_path)?;
    let mut edited = edit_in_editor(&original)?;
    if edited == original {
        return Ok(Edited { changed: false, warnings: Vec::new() });
    }

    for key in MANAGED_KEYS {
        if edited.get(key).is_none() {
            if let Some(value) = original.get(key) {
                edited.set(*key, value);
            }
        }
    }
    desktop_entry::validate_categories(&edited.categories())?;
    if let Some(value) = edited.get(desktop_entry::KEY_UPDATE_SOURCE) {
        if value != update::MANUAL {
            update::parse_update_source(value)?;
        }
    }

    edited.write(entry_path)?;
    let warnings = caches::validate_desktop_entry(entry_path);
    caches::refresh(paths);
    Ok(Edited { changed: true, warnings })
}

fn edit_in_editor(entry: &DesktopEntry) -> Result<DesktopEntry> {
    let path = env::var_os("PATH");
    let editor = find_editor(env::var_os("VISUAL"), env::var_os("EDITOR"), path.as_deref())
        .context(
            "no editor found, set EDITOR to the one to use: VISUAL and EDITOR are unset or name \
             no program that exists, and none of nvim, vim and nano is on PATH",
        )?;
    let program = &editor.program;

    let dir = tempfile::Builder::new().prefix("appimg-edit-").tempdir()?;
    let file = dir.path().join("entry.desktop");
    std::fs::write(&file, entry.to_string())?;

    let status = editor
        .command(&file)
        .status()
        .with_context(|| format!("cannot start the editor {program:?}"))?;
    if !status.success() {
        bail!("the editor {program:?} exited with {status}");
    }

    let text = std::fs::read_to_string(&file)?;
    Ok(DesktopEntry::parse(&text))
}

/// The editor to open an entry in, and the arguments it gets before the
/// file, such as the `--wait` of `code --wait`.
#[derive(Debug, PartialEq, Eq)]
struct Editor {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Editor {
    fn command(&self, file: &Path) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).arg(file);
        command
    }
}

/// Picks the editor to open an entry in: `$VISUAL`, then `$EDITOR`, then the
/// first of [`FALLBACK_EDITORS`] on `path`. A variable is split on
/// whitespace into the program and its arguments, and one that is unset,
/// empty or whose program is nothing executable is passed over. A program
/// with a slash in it is a path and is used as it is, any other is looked up
/// on `path`.
fn find_editor(
    visual: Option<OsString>,
    editor: Option<OsString>,
    path: Option<&OsStr>,
) -> Option<Editor> {
    let named = [visual, editor].into_iter().flatten().map(|value| words(&value));
    let fallbacks = FALLBACK_EDITORS.iter().map(|name| vec![OsString::from(name)]);

    named.chain(fallbacks).find_map(|words| {
        let (program, args) = words.split_first()?;
        let program = locate(program, path)?;
        Some(Editor { program, args: args.to_vec() })
    })
}

/// The executable a program name stands for, if there is one.
fn locate(program: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    if program.as_bytes().contains(&b'/') {
        let program = PathBuf::from(program);
        return fs_util::is_executable(&program).then_some(program);
    }
    fs_util::which_in(program, path?)
}

/// The words of a value split on whitespace. There is no quoting, so a
/// program whose path holds a space cannot be named with arguments.
fn words(value: &OsStr) -> Vec<OsString> {
    value
        .as_bytes()
        .split(u8::is_ascii_whitespace)
        .filter(|word| !word.is_empty())
        .map(|word| OsStr::from_bytes(word).to_os_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use appimg_core::fs_util::{MODE_EXEC, MODE_FILE};

    /// A directory holding an executable for each of `names`, for a `PATH`
    /// the test controls.
    fn bin_dir(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            fs_util::write_atomic(&dir.path().join(name), b"#!/bin/sh\n", MODE_EXEC).unwrap();
        }
        dir
    }

    fn var(value: impl Into<OsString>) -> Option<OsString> {
        Some(value.into())
    }

    fn editor(program: PathBuf, args: &[&str]) -> Option<Editor> {
        Some(Editor { program, args: args.iter().map(OsString::from).collect() })
    }

    #[test]
    fn visual_comes_before_editor_and_the_fallbacks() {
        let bin = bin_dir(&["code", "micro", "nvim"]);
        let path = Some(bin.path().as_os_str());
        let code = editor(bin.path().join("code"), &[]);
        assert_eq!(find_editor(var("code"), var("micro"), path), code);
    }

    #[test]
    fn editor_comes_before_the_fallbacks() {
        let bin = bin_dir(&["micro", "nvim"]);
        let path = Some(bin.path().as_os_str());
        let micro = editor(bin.path().join("micro"), &[]);
        assert_eq!(find_editor(None, var("micro"), path), micro);
        assert_eq!(find_editor(var(""), var("micro"), path), micro);
        assert_eq!(find_editor(var(" \t "), var("micro"), path), micro);
    }

    #[test]
    fn a_variable_that_names_nothing_executable_is_passed_over() {
        let bin = bin_dir(&["micro", "nano"]);
        let path = Some(bin.path().as_os_str());
        let micro = editor(bin.path().join("micro"), &[]);
        assert_eq!(find_editor(var("gone"), var("micro"), path), micro);
        let nano = editor(bin.path().join("nano"), &[]);
        assert_eq!(find_editor(var("gone"), var("also-gone"), path), nano);
    }

    #[test]
    fn the_fallbacks_go_nvim_then_vim_then_nano_wherever_they_are_on_path() {
        let first = bin_dir(&["nano"]);
        let second = bin_dir(&["vim"]);
        let third = bin_dir(&["nvim"]);

        let path = env::join_paths([first.path(), second.path(), third.path()]).unwrap();
        assert_eq!(find_editor(None, None, Some(&path)), editor(third.path().join("nvim"), &[]));
        let path = env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(find_editor(None, None, Some(&path)), editor(second.path().join("vim"), &[]));
        let path = env::join_paths([first.path()]).unwrap();
        assert_eq!(find_editor(None, None, Some(&path)), editor(first.path().join("nano"), &[]));
    }

    #[test]
    fn a_path_in_a_variable_is_used_as_it_is() {
        let elsewhere = bin_dir(&["my-editor"]);
        let program = elsewhere.path().join("my-editor");
        let bin = bin_dir(&["nvim"]);
        let path = Some(bin.path().as_os_str());
        assert_eq!(find_editor(None, var(&program), path), editor(program, &[]));
    }

    #[test]
    fn a_variable_with_arguments_names_the_program_and_its_arguments() {
        let bin = bin_dir(&["code", "subl", "nvim"]);
        let path = Some(bin.path().as_os_str());
        let code = editor(bin.path().join("code"), &["--wait"]);
        assert_eq!(find_editor(var("code --wait"), var("subl -w"), path), code);
        let subl = editor(bin.path().join("subl"), &["-w", "-n"]);
        assert_eq!(find_editor(None, var("  subl\t-w   -n "), path), subl);
    }

    #[test]
    fn a_path_with_arguments_is_used_as_it_is() {
        let elsewhere = bin_dir(&["my-editor"]);
        let program = elsewhere.path().join("my-editor");
        let bin = bin_dir(&["nvim"]);
        let path = Some(bin.path().as_os_str());
        let value = format!("{} --flag", program.display());
        assert_eq!(find_editor(var(value), None, path), editor(program, &["--flag"]));
    }

    #[test]
    fn a_variable_whose_program_is_missing_is_passed_over_with_its_arguments() {
        let bin = bin_dir(&["subl", "nvim"]);
        let path = Some(bin.path().as_os_str());
        let subl = editor(bin.path().join("subl"), &["-w"]);
        assert_eq!(find_editor(var("code --wait"), var("subl -w"), path), subl);
        let nvim = editor(bin.path().join("nvim"), &[]);
        assert_eq!(find_editor(var("code --wait"), var("gone -w"), path), nvim);
    }

    #[test]
    fn the_arguments_come_before_the_file() {
        let found = editor(PathBuf::from("/usr/bin/code"), &["--wait", "--new-window"]).unwrap();
        let command = found.command(Path::new("/tmp/entry.desktop"));
        assert_eq!(command.get_program(), "/usr/bin/code");
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(args, ["--wait", "--new-window", "/tmp/entry.desktop"]);
    }

    #[test]
    fn nothing_executable_is_none() {
        let bin = bin_dir(&["vi"]);
        fs_util::write_atomic(&bin.path().join("nvim"), b"", MODE_FILE).unwrap();
        let path = Some(bin.path().as_os_str());
        assert_eq!(find_editor(var("gone"), var("also-gone"), path), None);
        assert_eq!(find_editor(None, None, None), None);
    }
}
