//! Taking over an AppImage that is already on disk, without downloading it
//! again: it is moved into the appimages directory, or copied with the
//! original left where it is, and gets a desktop entry and icons the way an
//! install gives them.
//!
//! Desktop entries something else wrote for the same file, the way
//! AppImageLauncher integrates an AppImage as `appimagekit_<hash>-<name>.desktop`,
//! would make the launcher list it twice. They are found here and removed
//! only when the caller says so, and only those whose `Exec` runs exactly
//! that file.
//!
//! Nothing in here runs the file. A [`scan`] reads the front of each file
//! and the desktop entries, nothing else, and changes nothing.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::desktop_entry::DesktopEntry;
use crate::digest;
use crate::elf::{self, Unfit};
use crate::error::{Error, Result};
use crate::fs_util::{self, MODE_EXEC};
use crate::install::{self, InstallPlan, InstallRequest};
use crate::list;
use crate::metadata;
use crate::paths::Paths;
use crate::{caches, update};

/// How the file gets into the appimages directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    /// Taken over: the file moves, and is gone from where it was.
    Move,
    /// Copied: the original stays where it is.
    Copy,
}

/// A desktop entry appimg did not write whose `Exec` runs the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignEntry {
    pub path: PathBuf,
    /// The icon files that go with it, which no other desktop entry uses:
    /// the ones AppImageLauncher made for this file, and for the entry the
    /// adopted one replaces, every size of its icon named after the slug.
    pub icons: Vec<PathBuf>,
    /// The icon the entry names, when its files stay where they are, and
    /// why.
    pub icon_stays: Option<String>,
}

impl ForeignEntry {
    /// Whether AppImageLauncher wrote it, going by the name it gives its
    /// entries.
    pub fn by_appimagelauncher(&self) -> bool {
        file_name(&self.path).starts_with("appimagekit_")
    }

    /// Every file removing this entry deletes: its icons, then the entry.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files = self.icons.clone();
        files.push(self.path.clone());
        files
    }
}

/// Everything adopting a file would do, worked out before anything changes.
#[derive(Debug, Clone)]
pub struct AdoptPlan {
    /// The file to adopt, its canonical path.
    pub source: PathBuf,
    /// Where it goes and the desktop entry it gets, as an install plans it.
    pub install: InstallPlan,
    pub transfer: Transfer,
    /// The file is already where it belongs, under the name its slug gives
    /// it, so there is nothing to move or copy.
    pub in_place: bool,
    /// A symbolic link to the adopted file is left at the old path, so a
    /// command run from there keeps working and runs the managed file.
    pub link_back: bool,
    /// The desktop entries from elsewhere that run the file.
    pub foreign: Vec<ForeignEntry>,
    /// The one of them already where the adopted file's entry goes. When
    /// they go, that entry is written over it; while they stay, the slug is
    /// taken.
    pub replaces: Option<PathBuf>,
}

impl AdoptPlan {
    /// Refuses when the foreign entry the adopted one would be written over
    /// stays: then the slug is taken, as by anything else there.
    pub fn check_slug(&self, remove_foreign: bool) -> Result<()> {
        match &self.replaces {
            Some(entry) if !remove_foreign => {
                Err(Error::SlugTaken { slug: self.install.slug.clone(), taken_by: entry.clone() })
            }
            _ => Ok(()),
        }
    }
}

/// How the file ended up where it belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Moved there. Across filesystems that is a copy, checked against the
    /// original, and the original deleted only once everything else was in
    /// place.
    Moved {
        across_filesystems: bool,
    },
    Copied,
    /// It was there already.
    InPlace,
}

#[derive(Debug, Clone)]
pub struct AdoptOutcome {
    pub slug: String,
    pub appimage_path: PathBuf,
    pub desktop_entry_path: PathBuf,
    pub icons: Vec<PathBuf>,
    pub placement: Placement,
    /// Why the original is still there after a move across filesystems: it
    /// could not be deleted. The adopted copy is complete either way.
    pub original_left: Option<String>,
    /// The symbolic link left at the old path.
    pub link: Option<PathBuf>,
    /// Why no link could be left, when one was planned.
    pub link_failed: Option<String>,
    /// The foreign entry the adopted one was written over.
    pub replaced: Option<PathBuf>,
    /// Every file of the foreign entries that was deleted.
    pub removed: Vec<PathBuf>,
    /// Files of the foreign entries that could not be deleted, and why.
    pub not_removed: Vec<(PathBuf, String)>,
    pub validation_warnings: Vec<String>,
}

/// What can be said about a file before it is adopted, without running it:
/// it exists, is a file and no symbolic link, passes the checks an install
/// runs, and is no installed AppImage already. Returns its canonical path.
/// This comes before reading the metadata, which runs the file.
pub fn check(paths: &Paths, path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Error::NotFound(path.to_path_buf()),
        _ => Error::io(path, e),
    })?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|e| Error::io(path, e))?;
        let target = match path.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target,
        };
        return Err(Error::SymbolicLink { path: path.to_path_buf(), target });
    }
    if !metadata.is_file() {
        return Err(Error::NotAFile(path.to_path_buf()));
    }
    let source = fs::canonicalize(path).map_err(|e| Error::io(path, e))?;
    for app in list::list(paths)? {
        if fs::canonicalize(&app.appimage_path).is_ok_and(|installed| installed == source) {
            return Err(Error::AlreadyManaged { path: source, slug: app.slug });
        }
    }
    if !metadata::looks_like_appimage(&source) {
        return Err(Error::NotAnAppImage(source));
    }
    install::check_file(&source)?;
    Ok(source)
}

/// Works out what adopting the file `request.source` names would do. A slug
/// that is taken is refused, whoever took it, with one exception: a foreign
/// entry there that launches exactly this file, which adopting offers to
/// remove anyway. That one is replaced when the foreign entries go, see
/// [`AdoptPlan::check_slug`].
pub fn plan(
    paths: &Paths,
    request: &InstallRequest,
    transfer: Transfer,
    link_back: bool,
) -> Result<AdoptPlan> {
    let source = check(paths, &request.source)?;
    let install = install::plan(paths, request)?;
    let foreign = foreign_entries(paths, &source, &install.slug);

    let in_place = fs::canonicalize(&install.appimage_path).is_ok_and(|path| path == source);
    let replaces = foreign
        .iter()
        .map(|entry| &entry.path)
        .find(|path| replaceable(path, &install.desktop_entry_path))
        .cloned();
    if install.desktop_entry_path.exists() && replaces.is_none() {
        return Err(Error::SlugTaken {
            slug: install.slug.clone(),
            taken_by: install.desktop_entry_path.clone(),
        });
    }
    if install.appimage_path.exists() && !in_place {
        return Err(Error::SlugTaken {
            slug: install.slug.clone(),
            taken_by: install.appimage_path.clone(),
        });
    }

    Ok(AdoptPlan {
        foreign,
        replaces,
        link_back: link_back && transfer == Transfer::Move && !in_place,
        source,
        install,
        transfer,
        in_place,
    })
}

/// Adopts the file as planned. `remove_foreign` says whether the foreign
/// entries the plan found go, with their icons; the one the plan
/// [replaces](AdoptPlan::replaces) is written over instead, and while they
/// stay, nothing is adopted. Nothing else outside the adopted file and what
/// it is given is ever touched.
///
/// The icons come first, then the file moves, then the entry is written. A
/// failure on the way puts the file back where it was, takes the icons away
/// again, and gives whatever they and the entry were written over its bytes
/// back. A move across filesystems deletes the original last, once the
/// entry is in place.
pub fn adopt(
    paths: &Paths,
    plan: &AdoptPlan,
    request: &InstallRequest,
    remove_foreign: bool,
) -> Result<AdoptOutcome> {
    adopt_with(
        paths,
        plan,
        request,
        remove_foreign,
        &|from, to| fs::rename(from, to),
        &install::write_entry,
    )
}

/// [`adopt`] with the rename it moves the file with and the write of the
/// entry, so a test can stand in for a move across filesystems and for a
/// write that fails once it replaced the entry.
fn adopt_with(
    paths: &Paths,
    plan: &AdoptPlan,
    request: &InstallRequest,
    remove_foreign: bool,
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
    write_entry: &dyn Fn(&InstallPlan, &[PathBuf]) -> Result<()>,
) -> Result<AdoptOutcome> {
    plan.check_slug(remove_foreign)?;
    let target = &plan.install.appimage_path;
    paths.ensure_dirs()?;

    // What the entry and the icons are about to be written over, or that
    // goes before they are: the foreign entry this one replaces, and icons
    // that already go by the slug, such as that entry's own.
    let mut before = vec![Before::read(&plan.install.desktop_entry_path)?];
    for icon in fs_util::find_files_with_stem(&paths.icons_root, &plan.install.slug)? {
        before.push(Before::read(&icon)?);
    }

    // The icons of the entry this one replaces that are named after the
    // slug go before the adopted icons take that name, every size of them,
    // so no old size is left among the new ones.
    let slug_icons: Vec<PathBuf> = plan
        .foreign
        .iter()
        .filter(|entry| plan.replaces.as_ref() == Some(&entry.path))
        .flat_map(|entry| &entry.icons)
        .filter(|icon| icon.file_stem().and_then(|stem| stem.to_str()) == Some(&plan.install.slug))
        .cloned()
        .collect();
    let (mut removed, mut not_removed) = (Vec::new(), Vec::new());
    for icon in &slug_icons {
        remove_noting(icon, &mut removed, &mut not_removed);
    }

    let icons = install::install_icons(paths, request, &plan.install.slug);
    let placed = match place(plan, rename) {
        Ok(placed) => placed,
        Err(error) => {
            put_back(&icons, &before);
            return Err(error);
        }
    };
    if let Err(error) = write_entry(&plan.install, &icons) {
        placed.undo(&plan.source, target);
        put_back(&icons, &before);
        return Err(error);
    }

    let mut original_left = None;
    let placement = match placed {
        Placed::Renamed { .. } => Placement::Moved { across_filesystems: false },
        Placed::CopiedAcross => {
            if let Err(e) = fs::remove_file(&plan.source) {
                original_left = Some(e.to_string());
            }
            Placement::Moved { across_filesystems: true }
        }
        Placed::Copied => Placement::Copied,
        Placed::InPlace { .. } => Placement::InPlace,
    };

    let (mut link, mut link_failed) = (None, None);
    if plan.link_back && original_left.is_none() {
        match std::os::unix::fs::symlink(target, &plan.source) {
            Ok(()) => link = Some(plan.source.clone()),
            Err(e) => link_failed = Some(e.to_string()),
        }
    }

    if remove_foreign {
        // The entry that was replaced is the adopted one's now, and so are
        // the icons under the slug, which went before.
        let files = plan.foreign.iter().flat_map(ForeignEntry::files);
        for file in files.filter(|file| plan.replaces.as_ref() != Some(file)) {
            if !slug_icons.contains(&file) {
                remove_noting(&file, &mut removed, &mut not_removed);
            }
        }
    }

    let validation_warnings = caches::validate_desktop_entry(&plan.install.desktop_entry_path);
    caches::refresh(paths);

    Ok(AdoptOutcome {
        slug: plan.install.slug.clone(),
        appimage_path: target.clone(),
        desktop_entry_path: plan.install.desktop_entry_path.clone(),
        icons,
        placement,
        original_left,
        link,
        link_failed,
        replaced: plan.replaces.clone(),
        removed,
        not_removed,
        validation_warnings,
    })
}

/// A file as it was before the adoption wrote over it, or that it was not
/// there, so that a failure can put it back exactly.
struct Before {
    path: PathBuf,
    was: Was,
}

enum Was {
    Missing,
    File { bytes: Vec<u8>, mode: u32 },
    Link(PathBuf),
}

impl Before {
    fn read(path: &Path) -> Result<Before> {
        let was = match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Was::Missing,
            Err(e) => return Err(Error::io(path, e)),
            Ok(meta) if meta.file_type().is_symlink() => {
                Was::Link(fs::read_link(path).map_err(|e| Error::io(path, e))?)
            }
            Ok(meta) if meta.is_file() => Was::File {
                bytes: fs::read(path).map_err(|e| Error::io(path, e))?,
                mode: mode_of(path)?,
            },
            Ok(_) => return Err(Error::NotAFile(path.to_path_buf())),
        };
        Ok(Before { path: path.to_path_buf(), was })
    }

    fn put_back(&self) {
        match &self.was {
            Was::Missing => {
                let _ = fs::remove_file(&self.path);
            }
            Was::File { bytes, mode } => {
                let untouched = fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.is_file())
                    && mode_of(&self.path).is_ok_and(|now| now == *mode)
                    && fs::read(&self.path).is_ok_and(|now| now == *bytes);
                if !untouched {
                    let _ = fs_util::write_atomic(&self.path, bytes, *mode);
                }
            }
            Was::Link(target) => {
                let _ = fs::remove_file(&self.path);
                let _ = std::os::unix::fs::symlink(target, &self.path);
            }
        }
    }
}

/// Deletes a file of a foreign entry, and notes whether it went. One that
/// is gone already is no news.
fn remove_noting(
    file: &Path,
    removed: &mut Vec<PathBuf>,
    not_removed: &mut Vec<(PathBuf, String)>,
) {
    match fs::remove_file(file) {
        Ok(()) => removed.push(file.to_path_buf()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => not_removed.push((file.to_path_buf(), e.to_string())),
    }
}

/// Takes the icons away again, then puts back what they and the entry were
/// written over.
fn put_back(icons: &[PathBuf], before: &[Before]) {
    remove_all(icons);
    for file in before {
        file.put_back();
    }
}

/// What [`place`] did, so it can be undone.
enum Placed {
    /// Renamed into place. `mode` is what the file had before it was made
    /// executable.
    Renamed {
        mode: u32,
    },
    /// Copied across filesystems and checked; the original is still there.
    CopiedAcross,
    Copied,
    InPlace {
        mode: u32,
    },
}

impl Placed {
    /// Puts things back the way they were before [`place`].
    fn undo(&self, source: &Path, target: &Path) {
        match self {
            Placed::Renamed { mode } => {
                let _ = fs_util::set_mode(target, *mode);
                let _ = fs::rename(target, source);
            }
            Placed::CopiedAcross | Placed::Copied => {
                let _ = fs::remove_file(target);
            }
            Placed::InPlace { mode } => {
                let _ = fs_util::set_mode(target, *mode);
            }
        }
    }
}

/// Gets the file to where it belongs, executable.
fn place(plan: &AdoptPlan, rename: &dyn Fn(&Path, &Path) -> io::Result<()>) -> Result<Placed> {
    let (source, target) = (&plan.source, &plan.install.appimage_path);
    let mode = mode_of(source)?;

    if plan.in_place {
        fs_util::set_mode(target, MODE_EXEC)?;
        return Ok(Placed::InPlace { mode });
    }
    match plan.transfer {
        Transfer::Copy => {
            fs_util::copy_atomic(source, target, MODE_EXEC)?;
            Ok(Placed::Copied)
        }
        Transfer::Move => match rename(source, target) {
            Ok(()) => {
                fs_util::set_mode(target, MODE_EXEC)?;
                // The rename itself lasts once the directories are flushed.
                for dir in [source.parent(), target.parent()].into_iter().flatten() {
                    let _ = fs_util::sync(dir);
                }
                Ok(Placed::Renamed { mode })
            }
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                copy_across(source, target)?;
                Ok(Placed::CopiedAcross)
            }
            Err(e) => Err(Error::io(source, e)),
        },
    }
}

/// The copy a move across filesystems makes: into place, flushed, and
/// compared with the original before anything relies on it. A copy that
/// does not match is removed again.
fn copy_across(source: &Path, target: &Path) -> Result<()> {
    fs_util::copy_atomic(source, target, MODE_EXEC)?;
    let (original, copy) = (digest::sha256_file(source)?, digest::sha256_file(target)?);
    if original != copy {
        let _ = fs::remove_file(target);
        return Err(Error::io(
            target,
            io::Error::other(format!(
                "the copy is checksummed sha256:{copy}, the original sha256:{original}"
            )),
        ));
    }
    Ok(())
}

fn mode_of(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).map_err(|e| Error::io(path, e))
}

fn remove_all(files: &[PathBuf]) {
    for file in files {
        let _ = fs::remove_file(file);
    }
}

/// The desktop entries appimg did not write whose `Exec` runs exactly
/// `file`: its program is an absolute path that resolves to the same file.
/// Only the user's own applications directory is looked at, and only its
/// top level, which is where AppImageLauncher writes. `slug` is the one the
/// file is adopted under: an entry already where its entry goes is the one
/// that is replaced, see [`AdoptPlan::replaces`].
pub fn foreign_entries(paths: &Paths, file: &Path, slug: &str) -> Vec<ForeignEntry> {
    let Ok(file) = fs::canonicalize(file) else {
        return Vec::new();
    };
    let (launching, others): (Vec<_>, Vec<_>) = desktop_entries(&paths.applications_dir)
        .into_iter()
        .partition(|(_, entry)| launches(entry).as_ref() == Some(&file));

    let entry_path = paths.desktop_entry_path(slug);
    launching
        .into_iter()
        .map(|(path, entry)| {
            let replaced_by = replaceable(&path, &entry_path).then_some(slug);
            let (icons, icon_stays) = icons_of(paths, &entry, &others, replaced_by);
            ForeignEntry { path, icons, icon_stays }
        })
        .collect()
}

/// Whether the foreign entry at `path` is the one the adopted entry, going
/// to `entry_path`, is written over. Only a file of its own, not a link:
/// what is put back after a failure is the file's bytes.
fn replaceable(path: &Path, entry_path: &Path) -> bool {
    path == entry_path && fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// The icon files a foreign entry takes along, every size of them in the
/// user's hicolor theme: those named after an icon AppImageLauncher made
/// for this one file, `appimagekit_<hash>_...`, and for the entry the
/// adopted one replaces, those named after the slug in `replaced_by`, which
/// the adopted icons take. Only when no other desktop entry names the same
/// icon. Anything else stays, and the second value says why.
fn icons_of(
    paths: &Paths,
    entry: &DesktopEntry,
    others: &[(PathBuf, DesktopEntry)],
    replaced_by: Option<&str>,
) -> (Vec<PathBuf>, Option<String>) {
    let Some(name) = entry.get("Icon").map(str::trim).filter(|name| !name.is_empty()) else {
        return (Vec::new(), None);
    };
    let stays = |why: &str| (Vec::new(), Some(format!("its icon {name} stays, {why}")));
    if name.contains('/') {
        return stays("it is a file of its own, not an icon of the theme");
    }
    if !name.starts_with("appimagekit_") && replaced_by != Some(name) {
        return match replaced_by {
            Some(slug) => stays(&format!(
                "it is neither named {slug:?} like the adopted icons nor one AppImageLauncher \
                 made for this file"
            )),
            None => stays("it is not one AppImageLauncher made for this file"),
        };
    }
    if others.iter().any(|(_, other)| other.get("Icon").map(str::trim) == Some(name)) {
        return stays("another desktop entry uses it too");
    }
    (fs_util::find_files_with_stem(&paths.icons_root, name).unwrap_or_default(), None)
}

/// Every desktop entry at the top level of `dir`, with where it is. appimg's
/// own are among them: they count when the question is whether another
/// entry uses an icon, and [`launches`] passes them over, so they are never
/// touched.
fn desktop_entries(dir: &Path) -> Vec<(PathBuf, DesktopEntry)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, DesktopEntry)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("desktop"))
        .filter(|path| path.is_file())
        .filter_map(|path| DesktopEntry::read(&path).ok().map(|entry| (path, entry)))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The file a foreign desktop entry runs, canonical. `None` for appimg's
/// own entries, and for an `Exec` whose program is no absolute path or
/// does not resolve.
fn launches(entry: &DesktopEntry) -> Option<PathBuf> {
    if entry.is_managed() {
        return None;
    }
    let program = exec_program(entry.get("Exec")?)?;
    fs::canonicalize(program).ok()
}

/// The program an `Exec` value runs, unquoted the way the desktop entry
/// specification quotes it: the escapes of a string value first, then the
/// quoting of the `Exec` key. Only an absolute path counts, `env FOO=1 ...`
/// or `sh -c ...` runs `env` or `sh`, not the file.
fn exec_program(exec: &str) -> Option<PathBuf> {
    let value = unescape_string(exec);
    let value = value.trim_start();
    let program = match value.strip_prefix('"') {
        Some(rest) => {
            let mut out = String::new();
            let mut chars = rest.chars();
            loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => out.push(chars.next()?),
                    c => out.push(c),
                }
            }
            out
        }
        None => value.split_whitespace().next()?.to_string(),
    };
    let program = PathBuf::from(program);
    program.is_absolute().then_some(program)
}

/// The escapes of a desktop entry string value: `\s`, `\n`, `\t`, `\r` and
/// `\\`. Any other backslash is left for the `Exec` quoting to read.
fn unescape_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            _ => {
                out.push('\\');
                continue;
            }
        }
        chars.next();
    }
    out
}

/// An AppImage found by [`scan`] that appimg does not manage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub size: u64,
    /// Desktop entries appimg did not write whose `Exec` runs it.
    pub entries: Vec<PathBuf>,
    /// Why adopting it would be refused, when its front already says so.
    pub unfit: Option<Unfit>,
}

/// Lists the AppImages in `dirs` that appimg does not manage: every file at
/// the top level that carries the AppImage magic bytes or the extension,
/// apart from installed ones, the leftovers of an update, and hidden files.
/// Symbolic links are passed over, the file they point to is what would be
/// adopted. Reads the front of each file and the desktop entries, runs
/// nothing and changes nothing. A directory that does not exist is skipped.
pub fn scan(paths: &Paths, dirs: &[PathBuf]) -> Result<Vec<Candidate>> {
    let installed: HashSet<PathBuf> = list::list(paths)?
        .iter()
        .filter_map(|app| fs::canonicalize(&app.appimage_path).ok())
        .collect();
    let mut launched: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (path, entry) in desktop_entries(&paths.applications_dir) {
        if let Some(file) = launches(&entry) {
            launched.entry(file).or_default().push(path);
        }
    }

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for dir in dirs {
        let Ok(dir) = fs::canonicalize(dir) else {
            continue;
        };
        if !seen.insert(dir.clone()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        files.sort();

        for path in files {
            let name = file_name(&path);
            let lowercase = name.to_lowercase();
            let leftover = update::LEFTOVER_SUFFIXES
                .iter()
                .any(|suffix| lowercase.ends_with(&format!(".{}", suffix.to_lowercase())));
            if name.starts_with('.') || leftover {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || !metadata::looks_like_appimage(&path) {
                continue;
            }
            if installed.contains(&path) || !seen.insert(path.clone()) {
                continue;
            }
            candidates.push(Candidate {
                entries: launched.get(&path).cloned().unwrap_or_default(),
                unfit: elf::check_whole(&path).err(),
                size: metadata.len(),
                path,
            });
        }
    }
    Ok(candidates)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::IconChoice;

    #[test]
    fn the_program_of_an_exec_line_is_read_the_way_the_spec_quotes_it() {
        let program = |exec: &str| exec_program(exec).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(program("/a/App.AppImage %U").as_deref(), Some("/a/App.AppImage"));
        assert_eq!(program("\"/a/My App.AppImage\" %U").as_deref(), Some("/a/My App.AppImage"));
        assert_eq!(program("\"/a/q\\\\\"uote.AppImage\"").as_deref(), Some("/a/q\"uote.AppImage"));
        assert_eq!(program("\"/a/My\\sApp.AppImage\"").as_deref(), Some("/a/My App.AppImage"));
        // Unquoted, a space ends the program, however it was written.
        assert_eq!(program("/a/My\\sApp.AppImage").as_deref(), Some("/a/My"));
        assert_eq!(program("  /a/App.AppImage").as_deref(), Some("/a/App.AppImage"));
        // Whatever runs the file, it is not the file.
        assert_eq!(program("env FOO=1 /a/App.AppImage").as_deref(), None);
        assert_eq!(program("App.AppImage").as_deref(), None);
        assert_eq!(program("\"/a/unterminated").as_deref(), None);
        assert_eq!(program("").as_deref(), None);
    }

    fn sandbox() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            appimage_dir: root.join("appimages"),
            applications_dir: root.join("applications"),
            icons_root: root.join("icons"),
            config_home: root.join("config"),
            data_home: root.to_path_buf(),
        };
        paths.ensure_dirs().unwrap();
        (dir, paths)
    }

    /// The smallest file the checks pass: the ELF magic, and a front that
    /// names no length.
    fn appimage(path: &Path) {
        fs::write(path, b"\x7fELF\nnot really a runtime\n").unwrap();
    }

    fn request(source: &Path) -> InstallRequest {
        let info =
            metadata::AppImageInfo { name: Some("Moved App".to_string()), ..Default::default() };
        InstallRequest::from_info(source, &source.to_string_lossy(), &info)
    }

    /// A move across filesystems is a copy, checked, with the original
    /// deleted only once the entry is in place. The rename that would cross
    /// is stood in for, since a test cannot count on two filesystems.
    #[test]
    fn a_move_across_filesystems_copies_checks_and_deletes_last() {
        let (dir, paths) = sandbox();
        let source = dir.path().join("elsewhere/Moved.AppImage");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        appimage(&source);
        let bytes = fs::read(&source).unwrap();
        let request = request(&source);
        let plan = plan(&paths, &request, Transfer::Move, false).unwrap();

        let crosses = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::CrossesDevices));
        let outcome =
            adopt_with(&paths, &plan, &request, false, &crosses, &install::write_entry).unwrap();

        assert_eq!(outcome.placement, Placement::Moved { across_filesystems: true });
        assert_eq!(fs::read(&outcome.appimage_path).unwrap(), bytes);
        assert!(!source.exists());
        assert_eq!(outcome.original_left, None);
        assert!(outcome.desktop_entry_path.is_file());
    }

    /// When the original cannot be deleted, the adoption stands on the
    /// complete copy and says the original is still there. That the copy is
    /// complete by then is what deleting last buys.
    #[test]
    fn an_original_that_cannot_be_deleted_is_left_and_said_so() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, paths) = sandbox();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let source = elsewhere.join("Moved.AppImage");
        appimage(&source);
        let request = request(&source);
        let plan = plan(&paths, &request, Transfer::Move, true).unwrap();

        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o555)).unwrap();
        // Root deletes from a read-only directory all the same.
        let probe = elsewhere.join("probe");
        if fs::write(&probe, b"").is_ok() {
            let _ = fs::remove_file(&probe);
            fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: a read-only directory does not stop this user");
            return;
        }
        let crosses = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::CrossesDevices));
        let outcome = adopt_with(&paths, &plan, &request, false, &crosses, &install::write_entry);
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
        let outcome = outcome.unwrap();

        assert!(outcome.original_left.is_some());
        assert!(source.is_file());
        assert_eq!(fs::read(&outcome.appimage_path).unwrap(), fs::read(&source).unwrap());
        // No link where the original still is.
        assert_eq!(outcome.link, None);
    }

    /// Deleting the original last is what makes a failure on the way
    /// harmless: the entry cannot be written, so the copy goes and the
    /// original was never touched.
    #[test]
    fn a_move_across_filesystems_that_fails_keeps_the_original() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, paths) = sandbox();
        let source = dir.path().join("elsewhere/Moved.AppImage");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        appimage(&source);
        let bytes = fs::read(&source).unwrap();
        let request = request(&source);
        let plan = plan(&paths, &request, Transfer::Move, true).unwrap();

        let applications = &paths.applications_dir;
        fs::set_permissions(applications, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(applications.join("probe"), b"").is_ok() {
            let _ = fs::remove_file(applications.join("probe"));
            fs::set_permissions(applications, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: a read-only directory does not stop this user");
            return;
        }
        let crosses = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::CrossesDevices));
        let result = adopt_with(&paths, &plan, &request, false, &crosses, &install::write_entry);
        fs::set_permissions(applications, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err());
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert!(!plan.install.appimage_path.exists());
        assert!(fs::symlink_metadata(&source).unwrap().is_file());
    }

    /// A foreign entry under the slug is written over only once the file is
    /// in place. Should anything fail after that, it gets its very bytes and
    /// mode back, and so do its icons under the slug: the one the adopted
    /// icon went in over, and the one of a size the adopted icons do not
    /// have, which went before them.
    #[test]
    fn a_failure_after_the_foreign_entry_was_replaced_puts_its_bytes_back() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, paths) = sandbox();
        let source = dir.path().join("elsewhere/Moved.AppImage");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        appimage(&source);
        // Written in a way no rewrite of it keeps: a comment, keys out of
        // order, a blank line, no newline at the end.
        let entry = paths.desktop_entry_path("moved-app");
        let entry_bytes = format!(
            "# by another installer\n[Desktop Entry]\nIcon=moved-app\n\nExec=\"{}\" %U\n\
             Name=Moved App\nType=Application",
            source.display()
        );
        fs::write(&entry, &entry_bytes).unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o600)).unwrap();
        let icon = paths.icons_root.join("48x48/apps/moved-app.png");
        fs::create_dir_all(icon.parent().unwrap()).unwrap();
        let icon_bytes = [png(48), b"by another installer".to_vec()].concat();
        fs::write(&icon, &icon_bytes).unwrap();
        let small_icon = paths.icons_root.join("16x16/apps/moved-app.png");
        fs::create_dir_all(small_icon.parent().unwrap()).unwrap();
        fs::write(&small_icon, png(16)).unwrap();
        let adopted_icon = dir.path().join("elsewhere/icon.png");
        fs::write(&adopted_icon, png(48)).unwrap();
        let request = InstallRequest { icon: IconChoice::File(adopted_icon), ..request(&source) };

        let plan = plan(&paths, &request, Transfer::Move, false).unwrap();
        assert_eq!(plan.replaces.as_ref(), Some(&entry));
        let before = files(dir.path());

        let rename = |from: &Path, to: &Path| fs::rename(from, to);
        let fails_once_written = |install: &InstallPlan, icons: &[PathBuf]| {
            install::write_entry(install, icons)?;
            // All of them really were written over or gone by then.
            assert!(DesktopEntry::read(&entry).unwrap().is_managed());
            assert_ne!(fs::read(&icon).unwrap(), icon_bytes);
            assert!(!small_icon.exists());
            Err(Error::io(&entry, io::Error::other("failed once it was written")))
        };
        let result = adopt_with(&paths, &plan, &request, true, &rename, &fails_once_written);

        assert!(result.is_err());
        assert_eq!(fs::read(&entry).unwrap(), entry_bytes.as_bytes());
        assert_eq!(fs::read(&icon).unwrap(), icon_bytes);
        assert_eq!(fs::read(&small_icon).unwrap(), png(16));
        assert_eq!(files(dir.path()), before);
    }

    /// Every file and link under `root`: its mode and bytes, or where it
    /// points.
    fn files(root: &Path) -> std::collections::BTreeMap<PathBuf, (u32, Vec<u8>)> {
        use std::os::unix::fs::PermissionsExt;
        let mut found = std::collections::BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let (path, meta) = (entry.path(), entry.metadata().unwrap());
                if meta.file_type().is_symlink() {
                    let target = fs::read_link(&path).unwrap();
                    found.insert(path, (0, target.into_os_string().into_encoded_bytes()));
                } else if meta.is_dir() {
                    stack.push(path);
                } else {
                    let bytes = fs::read(&path).unwrap();
                    found.insert(path, (meta.permissions().mode() & 0o7777, bytes));
                }
            }
        }
        found
    }

    /// The front of a PNG, as far as its size.
    fn png(size: u32) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        out
    }

    #[test]
    fn a_copy_across_leaves_an_executable_copy_and_the_original() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&source, b"abc").unwrap();
        copy_across(&source, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"abc");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::read(&source).unwrap(), b"abc");
    }
}
