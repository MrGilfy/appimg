//! Adopting AppImages that are already on disk, and scanning for them. All
//! of it inside a temporary directory, and every test that changes anything
//! compares the whole of it before and after.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use appimg_core::adopt::{self, Placement, Transfer};
use appimg_core::desktop_entry::{DesktopEntry, KEY_ORIGIN};
use appimg_core::elf::Unfit;
use appimg_core::install::{self, InstallRequest};
use appimg_core::{list, metadata, AppImageInfo, Error};

use common::{changes, snapshot, with_unsquashfs_stand_in, FakeAppImage, Sandbox};

/// An ELF fake AppImage at `dir/file_name`, named "Fake App", the request
/// an install would make of it, and what was read out of it, which holds
/// the extracted icons for as long as it lives.
fn fake(sandbox: &Sandbox, dir: &str, file_name: &str) -> (PathBuf, InstallRequest, AppImageInfo) {
    let dir = sandbox.root.join(dir);
    fs::create_dir_all(&dir).unwrap();
    let built = FakeAppImage::new("Fake App").elf().build(&dir, file_name);
    let source = adopt::check(&sandbox.paths, &built).unwrap();
    let info = with_unsquashfs_stand_in(sandbox, || metadata::inspect(&source, None)).unwrap();
    let request = InstallRequest::from_info(&source, &source.to_string_lossy(), &info);
    (built, request, info)
}

#[test]
fn adopting_moves_the_file_and_gives_it_what_an_install_does() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "Applications", "Fake_App-1.0.0.AppImage");
    let bytes = fs::read(&file).unwrap();
    let source = file.canonicalize().unwrap();

    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    assert!(!plan.in_place && !plan.link_back);
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, false).unwrap();

    assert_eq!(outcome.placement, Placement::Moved { across_filesystems: false });
    assert_eq!(outcome.appimage_path, sandbox.paths.appimage_path("fake-app"));
    assert_eq!(fs::read(&outcome.appimage_path).unwrap(), bytes);
    assert!(fs_mode(&outcome.appimage_path) & 0o111 != 0);
    assert!(!file.exists());
    assert_eq!(outcome.icons.len(), 2);

    // It is an installed application like any other, and it remembers
    // where it came from.
    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(app.origin.as_deref(), Some(source.to_str().unwrap()));
    let entry = DesktopEntry::read(&outcome.desktop_entry_path).unwrap();
    assert_eq!(entry.get(KEY_ORIGIN), Some(source.to_str().unwrap()));
    assert!(entry.get("Exec").unwrap().contains(&*outcome.appimage_path.to_string_lossy()));
}

#[test]
fn adopting_a_copy_leaves_the_original_exactly_as_it_was() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "Applications", "Fake_App-1.0.0.AppImage");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&file).unwrap();
    let bytes = fs::read(&file).unwrap();

    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Copy, true).unwrap();
    // A copy leaves nothing behind to link to.
    assert!(!plan.link_back);
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, false).unwrap();

    assert_eq!(outcome.placement, Placement::Copied);
    assert_eq!(fs::read(&outcome.appimage_path).unwrap(), bytes);
    let after = fs::metadata(&file).unwrap();
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(after.permissions().mode(), before.permissions().mode());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    assert_eq!(outcome.link, None);
}

/// The answer to `~/.local/bin`: the file moves, and a link takes its place,
/// so the command keeps working and runs the adopted file.
#[test]
fn a_link_takes_the_place_of_the_file_when_asked() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "home/.local/bin", "fakeapp.AppImage");
    let bytes = fs::read(&file).unwrap();
    let source = file.canonicalize().unwrap();

    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, true).unwrap();
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, false).unwrap();

    assert_eq!(outcome.link.as_deref(), Some(source.as_path()));
    assert!(fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(&file).unwrap(), outcome.appimage_path);
    assert_eq!(fs::read(&file).unwrap(), bytes);
}

#[test]
fn a_file_in_the_appimages_directory_is_renamed_or_left_where_it_is() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "data/appimages", "Fake_App-1.0.0.AppImage");
    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, false).unwrap();
    assert_eq!(outcome.placement, Placement::Moved { across_filesystems: false });
    assert!(!file.exists());

    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "data/appimages", "fake-app.AppImage");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, true).unwrap();
    assert!(plan.in_place && !plan.link_back);
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, false).unwrap();
    assert_eq!(outcome.placement, Placement::InPlace);
    assert_eq!(outcome.appimage_path, sandbox.paths.appimage_path("fake-app"));
    assert!(fs_mode(&file) & 0o111 != 0);
}

/// A desktop entry from elsewhere that launches the file, AppImageLauncher
/// style, with its icons in the user's theme.
fn launcher_entry(sandbox: &Sandbox, name: &str, exec: &str, icon: &str) -> PathBuf {
    let path = sandbox.paths.applications_dir.join(name);
    fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=Fake App\nExec={exec}\nIcon={icon}\n\
             TryExec=/nowhere\nX-AppImage-Integrate=false\n"
        ),
    )
    .unwrap();
    path
}

fn theme_icon(sandbox: &Sandbox, size: u32, name: &str) -> PathBuf {
    let dir = sandbox.paths.icons_root.join(format!("{size}x{size}/apps"));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.png"));
    fs::write(&path, common::png_bytes(size, size)).unwrap();
    path
}

/// The rule that keeps foreign files safe: only an entry whose `Exec` runs
/// exactly the adopted file goes, and of its icons only the ones
/// AppImageLauncher made for that file, which nothing else uses. Everything
/// around it that only looks alike stays, byte for byte.
#[test]
fn only_entries_that_launch_exactly_that_file_go_and_nothing_else_is_touched() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "Applications", "Fake App.AppImage");
    let source = file.canonicalize().unwrap();
    let quoted = format!("\"{}\"", source.display());

    // What launches the file.
    let launcher = launcher_entry(
        &sandbox,
        "appimagekit_1a2b-Fake_App.desktop",
        &format!("{quoted} %U"),
        "appimagekit_1a2b_fakeapp",
    );
    // Sorted by path, the way they are found: 256x256 before 48x48.
    let launcher_icons = [
        theme_icon(&sandbox, 256, "appimagekit_1a2b_fakeapp"),
        theme_icon(&sandbox, 48, "appimagekit_1a2b_fakeapp"),
    ];
    let handmade =
        launcher_entry(&sandbox, "fake-by-hand.desktop", &format!("{quoted} --flag"), "krita");
    theme_icon(&sandbox, 48, "krita");
    let link = sandbox.root.join("Applications/link-to-fake");
    std::os::unix::fs::symlink(&source, &link).unwrap();
    let through_link =
        launcher_entry(&sandbox, "through-link.desktop", &link.to_string_lossy(), "");
    let sharing = launcher_entry(
        &sandbox,
        "appimagekit_9f9f-Fake_App.desktop",
        &quoted,
        "appimagekit_9f9f_shared",
    );
    theme_icon(&sandbox, 64, "appimagekit_9f9f_shared");

    // What only looks like it.
    let elsewhere = sandbox.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::copy(&source, elsewhere.join("Fake App.AppImage")).unwrap();
    fs::copy(&source, sandbox.root.join("Applications/Fake App.AppImage.old")).unwrap();
    launcher_entry(
        &sandbox,
        "same-name.desktop",
        &format!("\"{}\"", elsewhere.join("Fake App.AppImage").display()),
        "appimagekit_5555_same",
    );
    theme_icon(&sandbox, 48, "appimagekit_5555_same");
    launcher_entry(&sandbox, "env.desktop", &format!("env FOO=1 {quoted}"), "appimagekit_2222_env");
    launcher_entry(&sandbox, "older.desktop", &format!("\"{}.old\"", source.display()), "older");
    launcher_entry(&sandbox, "shares-the-icon.desktop", "/usr/bin/true", "appimagekit_9f9f_shared");
    let other = FakeAppImage::new("Other App").build(&sandbox.root, "Other.AppImage");
    let info = metadata::inspect(&other, None).unwrap();
    install::install(
        &sandbox.paths,
        &InstallRequest::from_info(&other, &other.to_string_lossy(), &info),
    )
    .unwrap();

    // The plan shows exactly what launches the file, and what goes with it.
    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    let shown: Vec<PathBuf> = plan.foreign.iter().map(|entry| entry.path.clone()).collect();
    assert_eq!(
        shown,
        vec![launcher.clone(), sharing.clone(), handmade.clone(), through_link.clone()]
    );
    assert_eq!(plan.foreign[0].icons, launcher_icons.to_vec());
    assert!(plan.foreign[0].by_appimagelauncher());
    assert!(plan.foreign[1].icons.is_empty());
    assert!(plan.foreign[1]
        .icon_stays
        .as_deref()
        .unwrap()
        .contains("another desktop entry uses it too"));
    assert!(plan.foreign[2].icon_stays.as_deref().unwrap().contains("krita stays"));
    let shown_files: Vec<PathBuf> = plan.foreign.iter().flat_map(|entry| entry.files()).collect();

    let before = snapshot(&sandbox.root);
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, true).unwrap();
    let after = snapshot(&sandbox.root);

    // Gone: the file from where it was, and what the plan showed. Nothing
    // else.
    let relative = |path: &Path| path.strip_prefix(&sandbox.root).unwrap().to_path_buf();
    let (gone, new, changed) = changes(&before, &after);
    let mut expected_gone: Vec<PathBuf> = shown_files.iter().map(|p| relative(p)).collect();
    expected_gone.push(relative(&source));
    expected_gone.sort();
    assert_eq!(gone, expected_gone);
    assert_eq!(
        outcome.removed.iter().map(|p| relative(p)).collect::<Vec<_>>().len(),
        shown_files.len()
    );

    // New: the adopted file, its entry and its icons. Nothing else.
    let mut expected_new =
        vec![relative(&outcome.appimage_path), relative(&outcome.desktop_entry_path)];
    expected_new.extend(outcome.icons.iter().map(|p| relative(p)));
    expected_new.sort();
    assert_eq!(new, expected_new);
    assert_eq!(changed, Vec::<PathBuf>::new());
}

#[test]
fn declining_the_removal_leaves_every_foreign_entry_where_it_is() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "Applications", "Fake_App.AppImage");
    let source = file.canonicalize().unwrap();
    launcher_entry(
        &sandbox,
        "appimagekit_1a2b-Fake_App.desktop",
        &source.to_string_lossy(),
        "appimagekit_1a2b_fakeapp",
    );
    theme_icon(&sandbox, 48, "appimagekit_1a2b_fakeapp");

    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    assert_eq!(plan.foreign.len(), 1);
    let before = snapshot(&sandbox.root);
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, false).unwrap();
    let (gone, _, changed) = changes(&before, &snapshot(&sandbox.root));

    assert_eq!(gone, vec![source.strip_prefix(&sandbox.root).unwrap().to_path_buf()]);
    assert_eq!(changed, Vec::<PathBuf>::new());
    assert!(outcome.removed.is_empty());
}

/// An entry from elsewhere can sit where the adopted one goes, under the
/// slug's own name, the way another installer writes one. When it launches
/// exactly this file it is listed like any other foreign entry, and when
/// they go the adopted entry takes its place, while the icons that went with
/// it go as they would anyway.
#[test]
fn a_foreign_entry_under_the_slug_is_replaced_when_the_entries_go() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "data/appimages", "fake-app.AppImage");
    let source = file.canonicalize().unwrap();
    let entry = launcher_entry(
        &sandbox,
        "fake-app.desktop",
        &format!("\"{}\" %U", source.display()),
        "appimagekit_1a2b_fakeapp",
    );
    let icon = theme_icon(&sandbox, 64, "appimagekit_1a2b_fakeapp");

    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    assert!(plan.in_place);
    assert_eq!(plan.replaces.as_ref(), Some(&entry));
    let shown: Vec<&PathBuf> = plan.foreign.iter().map(|entry| &entry.path).collect();
    assert_eq!(shown, vec![&entry]);
    assert_eq!(plan.foreign[0].icons, vec![icon.clone()]);
    assert!(plan.check_slug(true).is_ok());

    let before = snapshot(&sandbox.root);
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, true).unwrap();
    let (gone, new, changed) = changes(&before, &snapshot(&sandbox.root));

    assert_eq!(outcome.desktop_entry_path, entry);
    assert_eq!(outcome.replaced.as_ref(), Some(&entry));
    assert_eq!(outcome.removed, vec![icon.clone()]);
    assert!(DesktopEntry::read(&entry).unwrap().is_managed());
    assert_eq!(list::find(&sandbox.paths, "fake-app").unwrap().appimage_path, source);

    let relative = |path: &Path| path.strip_prefix(&sandbox.root).unwrap().to_path_buf();
    assert_eq!(gone, vec![relative(&icon)]);
    let mut expected_new: Vec<PathBuf> = outcome.icons.iter().map(|p| relative(p)).collect();
    expected_new.sort();
    assert_eq!(new, expected_new);
    assert_eq!(changed, vec![relative(&entry)]);
}

/// While the foreign entries stay, the one under the slug keeps it: the
/// slug is taken, and nothing changes. Under the slug, an entry that does
/// not launch this very file, or a link to one that does, takes it however
/// that is decided.
#[test]
fn a_foreign_entry_under_the_slug_that_stays_keeps_the_slug_taken() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "Applications", "Fake App.AppImage");
    let source = file.canonicalize().unwrap();
    let launching = format!("\"{}\" %U", source.display());
    let entry = launcher_entry(&sandbox, "fake-app.desktop", &launching, "fake-app");
    theme_icon(&sandbox, 48, "fake-app");

    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    assert_eq!(plan.replaces.as_ref(), Some(&entry));
    let before = snapshot(&sandbox.root);
    let error = adopt::adopt(&sandbox.paths, &plan, &request, false).unwrap_err();
    assert!(
        matches!(&error, Error::SlugTaken { slug, taken_by } if slug == "fake-app" && *taken_by == entry),
        "{error}"
    );
    assert!(error.to_string().contains("--name"), "{error}");
    assert_eq!(snapshot(&sandbox.root), before);

    let copy = sandbox.root.join("elsewhere/Fake App.AppImage");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(&source, &copy).unwrap();
    launcher_entry(&sandbox, "fake-app.desktop", &format!("\"{}\"", copy.display()), "fake-app");
    let error = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap_err();
    assert!(matches!(error, Error::SlugTaken { .. }), "{error}");

    let real_entry = launcher_entry(&sandbox, "real.desktop", &launching, "fake-app");
    fs::remove_file(&entry).unwrap();
    std::os::unix::fs::symlink(&real_entry, &entry).unwrap();
    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false);
    assert!(matches!(plan, Err(Error::SlugTaken { .. })), "{:?}", plan.map(|plan| plan.foreign));
}

/// When the entry under the slug is replaced, its icons named after the
/// slug go too, every size of them, before the adopted icons take that
/// name: no old size is left among the new ones. Should another entry use
/// that icon, they all stay.
#[test]
fn the_icons_under_the_slug_go_with_the_entry_they_belong_to() {
    let _serial = common::serial();
    let setup = || {
        let sandbox = Sandbox::new();
        let (file, request, _info) = fake(&sandbox, "data/appimages", "fake-app.AppImage");
        let launching = format!("\"{}\" %U", file.canonicalize().unwrap().display());
        let entry = launcher_entry(&sandbox, "fake-app.desktop", &launching, "fake-app");
        // The fake ships 48 and 256: one size it writes over, two it does
        // not have.
        let old_icons = [16, 48, 512].map(|size| theme_icon(&sandbox, size, "fake-app"));
        fs::write(&old_icons[1], [common::png_bytes(48, 48), b"old".to_vec()].concat()).unwrap();
        (sandbox, request, entry, old_icons, _info)
    };

    let (sandbox, request, entry, old_icons, _info) = setup();
    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    assert_eq!(plan.foreign[0].icons, old_icons.to_vec());
    assert_eq!(plan.foreign[0].icon_stays, None);

    let before = snapshot(&sandbox.root);
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, true).unwrap();
    let (gone, new, changed) = changes(&before, &snapshot(&sandbox.root));

    assert_eq!(outcome.removed, old_icons.to_vec());
    let mut adopted = outcome.icons.clone();
    adopted.sort();
    let under_the_slug =
        appimg_core::fs_util::find_files_with_stem(&sandbox.paths.icons_root, "fake-app").unwrap();
    assert_eq!(under_the_slug, adopted);
    let relative = |path: &Path| path.strip_prefix(&sandbox.root).unwrap().to_path_buf();
    assert_eq!(gone, vec![relative(&old_icons[0]), relative(&old_icons[2])]);
    let icon_256 = adopted.iter().find(|icon| !old_icons.contains(icon)).unwrap();
    assert_eq!(new, vec![relative(icon_256)]);
    assert_eq!(changed, vec![relative(&entry), relative(&old_icons[1])]);

    let (sandbox, request, _entry, old_icons, _info) = setup();
    launcher_entry(&sandbox, "other.desktop", "/usr/bin/true", "fake-app");
    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    assert!(plan.foreign[0].icons.is_empty());
    let why = plan.foreign[0].icon_stays.as_deref().unwrap();
    assert!(why.contains("another desktop entry uses it too"), "{why}");
    let outcome = adopt::adopt(&sandbox.paths, &plan, &request, true).unwrap();
    assert!(outcome.removed.is_empty());
    assert!(old_icons[0].is_file() && old_icons[2].is_file());
}

/// The icons go in first, the file moves, then the entry is written. When
/// the entry cannot be written, the file goes back with the mode it had and
/// the icons go away: everything is as it was.
#[test]
fn a_failure_after_the_move_puts_everything_back() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let (file, request, _info) = fake(&sandbox, "Applications", "Fake_App.AppImage");
    // Not executable, so the move makes it executable and putting it back
    // has to undo that too.
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let plan = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap();
    let before = snapshot(&sandbox.root);

    let applications = &sandbox.paths.applications_dir;
    fs::set_permissions(applications, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(applications.join("probe"), b"").is_ok() {
        let _ = fs::remove_file(applications.join("probe"));
        fs::set_permissions(applications, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: a read-only directory does not stop this user");
        return;
    }
    let result = adopt::adopt(&sandbox.paths, &plan, &request, false);
    fs::set_permissions(applications, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(result.is_err());
    assert!(file.is_file());
    assert_eq!(snapshot(&sandbox.root), before);
}

#[test]
fn what_cannot_be_adopted_is_refused_before_anything_changes() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let dir = sandbox.root.join("Applications");
    fs::create_dir_all(&dir).unwrap();

    // A shell script that only claims the extension.
    let script = FakeAppImage::new("Script").build(&dir, "Script.AppImage");
    // An AppImage whose squashfs says it is longer than the file.
    let cut = dir.join("Cut.AppImage");
    fs::write(&cut, cut_short()).unwrap();
    // A link, and an installed application's own file.
    let (real, _, _real_info) = fake(&sandbox, "elsewhere", "Real.AppImage");
    let link = dir.join("Link.AppImage");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let installed = FakeAppImage::new("Installed").build(&sandbox.root, "Installed.AppImage");
    let info = metadata::inspect(&installed, None).unwrap();
    let outcome =
        install::install(&sandbox.paths, &InstallRequest::from_info(&installed, "x", &info))
            .unwrap();
    let before = snapshot(&sandbox.root);

    let check = |path: &Path| adopt::check(&sandbox.paths, path).unwrap_err();
    assert!(matches!(check(&script), Error::Unfit { unfit: Unfit::NoElfHeader, .. }));
    assert!(matches!(check(&cut), Error::Unfit { unfit: Unfit::CutShort { .. }, .. }));
    assert!(matches!(check(&link), Error::SymbolicLink { .. }), "{}", check(&link));
    assert!(check(&link).to_string().contains("Real.AppImage"));
    assert!(
        matches!(check(&outcome.appimage_path), Error::AlreadyManaged { slug, .. } if slug == "installed")
    );
    assert!(matches!(check(&dir.join("Missing.AppImage")), Error::NotFound(_)));

    // A slug that is taken is refused too, by the plan, whoever took it.
    let (_, request, _info) = fake(&sandbox, "Applications", "Installed.AppImage");
    let request = InstallRequest { name: "Installed".to_string(), ..request };
    let before_plan = snapshot(&sandbox.root);
    let error = adopt::plan(&sandbox.paths, &request, Transfer::Move, false).unwrap_err();
    assert!(matches!(error, Error::SlugTaken { ref slug, .. } if slug == "installed"), "{error}");
    assert!(error.to_string().contains("--name"), "{error}");
    assert_eq!(snapshot(&sandbox.root), before_plan);

    // None of the checks changed anything.
    let mut after = snapshot(&sandbox.root);
    after.retain(|path, _| before.contains_key(path));
    assert_eq!(after, before);
}

/// The front of an AppImage whose squashfs superblock says the filesystem
/// is 10000 bytes long, cut off after 1000.
fn cut_short() -> Vec<u8> {
    let mut out = b"\x7fELF\x02\x01\x01".to_vec();
    out.resize(16, 0);
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&62u16.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&64u64.to_le_bytes()); // e_shoff
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&64u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
    out.extend_from_slice(&1u16.to_le_bytes()); // e_shnum
    out.extend_from_slice(&0u16.to_le_bytes());
    out.resize(128, 0);
    out.extend_from_slice(b"hsqs");
    out.resize(128 + 28, 0);
    out.extend_from_slice(&4u16.to_le_bytes()); // s_major
    out.resize(128 + 40, 0);
    out.extend_from_slice(&10_000u64.to_le_bytes()); // bytes_used
    out.resize(1000, 0);
    out
}

/// A file that starts like an AppImage with the type 2 magic bytes and no
/// extension, the way an AppImage sits in `~/.local/bin` as a command.
fn bare_appimage(path: &Path) {
    let mut bytes = b"\x7fELF\x01\x01\x01\x00AI\x02".to_vec();
    bytes.extend_from_slice(b" a runtime that never runs\n");
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The scan finds what could be adopted, says which foreign entries launch
/// it and why something could not be adopted, and leaves out what appimg
/// already manages, the leftovers of an update, hidden files, links and
/// whatever is no AppImage. It changes nothing and runs nothing: one of the
/// files would leave a mark if it ever ran, and reading the metadata of the
/// one that is not executable would make it executable first.
#[test]
fn a_scan_lists_what_could_be_adopted_and_changes_and_runs_nothing() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let applications = sandbox.root.join("home/Applications");
    let bin = sandbox.root.join("home/.local/bin");
    fs::create_dir_all(&applications).unwrap();
    fs::create_dir_all(&bin).unwrap();

    // Managed, and its leftover: not listed.
    let managed = FakeAppImage::new("Managed").build(&sandbox.root, "Managed.AppImage");
    let info = metadata::inspect(&managed, None).unwrap();
    install::install(&sandbox.paths, &InstallRequest::from_info(&managed, "x", &info)).unwrap();
    fs::copy(&managed, sandbox.paths.appimage_dir.join("managed.AppImage.bak")).unwrap();
    // Unmanaged in the appimages directory, not executable the way a
    // browser saves it: listed.
    let stray = FakeAppImage::new("Stray")
        .elf()
        .not_executable()
        .build(&sandbox.paths.appimage_dir, "Stray.AppImage");

    // ~/Applications: one with an AppImageLauncher entry, one that would
    // leave a mark if anything ran it, a hidden one, a link, and a text file.
    let launched = FakeAppImage::new("Launched").elf().build(&applications, "Launched.AppImage");
    let entry = launcher_entry(
        &sandbox,
        "appimagekit_77-Launched.desktop",
        &launched.to_string_lossy(),
        "x",
    );
    // A copy of the same name elsewhere, with an entry of its own: that
    // entry launches the copy, not this file.
    let copy = sandbox.root.join("elsewhere/Launched.AppImage");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(&launched, &copy).unwrap();
    launcher_entry(&sandbox, "appimagekit_88-Launched.desktop", &copy.to_string_lossy(), "y");
    let mark = sandbox.root.join("it-ran");
    let runner = applications.join("Runner.AppImage");
    fs::write(&runner, format!("#!/bin/sh\ntouch '{}'\n", mark.display())).unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o755)).unwrap();
    FakeAppImage::new("Hidden").elf().build(&applications, ".Hidden.AppImage");
    std::os::unix::fs::symlink(&launched, applications.join("Link.AppImage")).unwrap();
    fs::write(applications.join("notes.txt"), "not an AppImage").unwrap();

    // ~/.local/bin: an AppImage without the extension, and a script.
    let command = bin.join("fakeapp");
    bare_appimage(&command);
    fs::write(bin.join("script"), "#!/bin/sh\necho hi\n").unwrap();

    let before = snapshot(&sandbox.root);
    let dirs = [
        sandbox.paths.appimage_dir.clone(),
        applications.clone(),
        bin.clone(),
        sandbox.root.join("home/missing"),
    ];
    let candidates = adopt::scan(&sandbox.paths, &dirs).unwrap();

    let canonical = |path: &Path| path.canonicalize().unwrap();
    let found: Vec<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
    assert_eq!(
        found,
        vec![canonical(&stray), canonical(&launched), canonical(&runner), canonical(&command)]
    );
    assert_eq!(candidates[1].entries, vec![entry]);
    assert!(candidates[0].entries.is_empty());
    assert_eq!(candidates[2].unfit, Some(Unfit::NoElfHeader));
    assert_eq!(candidates[3].unfit, None);
    assert_eq!(candidates[3].size, fs::metadata(&command).unwrap().len());

    assert_eq!(snapshot(&sandbox.root), before);
    assert!(!mark.exists());
}

fn fs_mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode()
}
