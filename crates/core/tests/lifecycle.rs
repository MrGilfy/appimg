//! The full path an installation takes: inspect, install, list, update,
//! remove, doctor. Everything runs against a temporary XDG home.

mod common;

use std::fs;

use appimg_core::desktop_entry::{self, DesktopEntry};
use appimg_core::install::{IconChoice, InstallRequest};
use appimg_core::list::Health;
use appimg_core::{doctor, install, list, metadata, remove, update, Error};

use common::{is_executable, read, walk, FakeAppImage, Sandbox};

fn install_fake(sandbox: &Sandbox, file_name: &str) -> install::InstallOutcome {
    let source = FakeAppImage::new("Fake App").build(&sandbox.downloads, file_name);
    let info = metadata::inspect(&source, None).unwrap();
    let request = InstallRequest::from_info(&source, &source.to_string_lossy(), &info);
    install::install(&sandbox.paths, &request).unwrap()
}

#[test]
fn install_writes_binary_icons_and_entry() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let outcome = install_fake(&sandbox, "Fake_App-1.2.0-x86_64.AppImage");

    assert_eq!(outcome.slug, "fake-app");
    assert!(outcome.appimage_path.is_file());
    assert!(is_executable(&outcome.appimage_path));
    assert_eq!(outcome.appimage_path, sandbox.paths.appimage_dir.join("fake-app.AppImage"));
    assert!(!outcome.replaced);

    let icons = walk(&sandbox.paths.icons_root);
    assert!(icons.contains(&"48x48/apps/fake-app.png".into()), "{icons:?}");
    assert!(icons.contains(&"256x256/apps/fake-app.png".into()), "{icons:?}");

    let entry = DesktopEntry::read(&outcome.desktop_entry_path).unwrap();
    assert_eq!(entry.get("Type"), Some("Application"));
    assert_eq!(entry.get("Name"), Some("Fake App"));
    assert_eq!(entry.get("Comment"), Some("A fake application"));
    assert_eq!(entry.get("Icon"), Some("fake-app"));
    assert_eq!(entry.get(desktop_entry::KEY_MANAGED), Some("true"));
    assert_eq!(entry.get(desktop_entry::KEY_SLUG), Some("fake-app"));
    assert_eq!(entry.get(desktop_entry::KEY_VERSION), Some("1.2.0"));
    assert!(entry.get(desktop_entry::KEY_INSTALLED_AT).is_some());
    assert_eq!(entry.categories(), vec!["Utility"]);

    let exec = entry.get("Exec").unwrap();
    assert!(exec.contains(&outcome.appimage_path.to_string_lossy().to_string()), "{exec}");
    assert!(exec.ends_with(" %U"), "the AppImage declared %U, so it is carried over: {exec}");

    // Whatever `desktop-file-validate` is installed must accept the entry.
    assert!(outcome.validation_warnings.is_empty(), "{:?}", outcome.validation_warnings);
}

#[test]
fn the_preferred_locale_wins_for_the_name() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let source = FakeAppImage::new("Fake App").build(&sandbox.downloads, "Fake App.AppImage");
    let info = metadata::inspect(&source, Some("de_DE.UTF-8")).unwrap();
    assert_eq!(info.name.as_deref(), Some("Fake App (de)"));
}

#[test]
fn list_reports_what_was_installed() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    install_fake(&sandbox, "Fake_App-1.2.0.AppImage");

    let apps = list::list(&sandbox.paths).unwrap();
    assert_eq!(apps.len(), 1);
    let app = &apps[0];
    assert_eq!(app.slug, "fake-app");
    assert_eq!(app.name, "Fake App");
    assert_eq!(app.version.as_deref(), Some("1.2.0"));
    assert_eq!(app.health, Health::Ok);
    assert!(app.size_bytes.unwrap() > 0);

    assert_eq!(list::find(&sandbox.paths, "fake-app").unwrap().slug, "fake-app");
    assert_eq!(list::find(&sandbox.paths, "Fake App").unwrap().slug, "fake-app");
    assert_eq!(list::find(&sandbox.paths, "fak").unwrap().slug, "fake-app");
    assert!(matches!(list::find(&sandbox.paths, "nope"), Err(Error::NotInstalled(_))));
}

#[test]
fn foreign_desktop_entries_are_left_alone() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    fs::write(
        sandbox.paths.applications_dir.join("someone-else.desktop"),
        "[Desktop Entry]\nType=Application\nName=Other\nExec=/usr/bin/other\n",
    )
    .unwrap();

    assert!(list::list(&sandbox.paths).unwrap().is_empty());
}

#[test]
fn installing_the_same_slug_twice_needs_overwrite() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    install_fake(&sandbox, "Fake_App-1.2.0.AppImage");

    let source = FakeAppImage::new("Fake App")
        .marker("second")
        .build(&sandbox.downloads, "Fake_App-2.0.0.AppImage");
    let info = metadata::inspect(&source, None).unwrap();
    let mut request = InstallRequest::from_info(&source, &source.to_string_lossy(), &info);

    match install::install(&sandbox.paths, &request) {
        Err(Error::AlreadyInstalled { slug, .. }) => assert_eq!(slug, "fake-app"),
        other => panic!("expected AlreadyInstalled, got {other:?}"),
    }

    request.overwrite = true;
    let outcome = install::install(&sandbox.paths, &request).unwrap();
    assert!(outcome.replaced);
    assert!(read(&outcome.appimage_path).contains("second"));
    assert_eq!(list::list(&sandbox.paths).unwrap().len(), 1);
}

#[test]
fn stale_icons_do_not_survive_a_replacement() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let first = FakeAppImage::new("Fake App")
        .icon_sizes(&[48, 256])
        .build(&sandbox.downloads, "Fake_App-1.0.0.AppImage");
    let info = metadata::inspect(&first, None).unwrap();
    install::install(&sandbox.paths, &InstallRequest::from_info(&first, "local", &info)).unwrap();

    let second = FakeAppImage::new("Fake App")
        .icon_sizes(&[64])
        .build(&sandbox.downloads, "Fake_App-2.0.0.AppImage");
    let info = metadata::inspect(&second, None).unwrap();
    let mut request = InstallRequest::from_info(&second, "local", &info);
    request.overwrite = true;
    install::install(&sandbox.paths, &request).unwrap();

    let icons = walk(&sandbox.paths.icons_root);
    assert!(icons.contains(&"64x64/apps/fake-app.png".into()), "{icons:?}");
    assert!(!icons.contains(&"256x256/apps/fake-app.png".into()), "{icons:?}");
}

#[test]
fn an_unusable_icon_falls_back_to_the_generic_one() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let source = FakeAppImage::new("Fake App").build(&sandbox.downloads, "Fake_App.AppImage");
    let info = metadata::inspect(&source, None).unwrap();
    let mut request = InstallRequest::from_info(&source, "local", &info);
    request.icon = IconChoice::Fallback;

    let outcome = install::install(&sandbox.paths, &request).unwrap();
    assert!(outcome.icons.is_empty());
    let entry = DesktopEntry::read(&outcome.desktop_entry_path).unwrap();
    assert_eq!(entry.get("Icon"), Some(install::FALLBACK_ICON));
}

#[test]
fn a_deleted_binary_shows_up_as_broken() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let outcome = install_fake(&sandbox, "Fake_App-1.0.0.AppImage");
    fs::remove_file(&outcome.appimage_path).unwrap();

    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(app.health, Health::MissingBinary);
    assert!(app.is_broken());

    let report = doctor::run(&sandbox.paths).unwrap();
    assert_eq!(report.broken_entries.len(), 1);
    assert_eq!(report.broken_entries[0].0, "fake-app");
    assert!(!report.is_clean());
}

#[test]
fn doctor_leaves_files_of_other_applications_alone() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    install_fake(&sandbox, "Fake_App-1.0.0.AppImage");

    // A real home shares the icon theme and the AppImage directory with
    // everything else installed on the machine.
    let foreign_icons =
        ["512x512/apps/osu.png", "48x48/apps/curseforge.png", "64x64/apps/steam.png"];
    for icon in foreign_icons {
        let path = sandbox.paths.icons_root.join(icon);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, common::png_bytes(48, 48)).unwrap();
    }
    for appimage in ["osu.AppImage", "curseforge.AppImage"] {
        fs::write(sandbox.paths.appimage_dir.join(appimage), "someone else's AppImage").unwrap();
    }
    fs::write(
        sandbox.paths.applications_dir.join("wine-Steam.desktop"),
        "[Desktop Entry]\nType=Application\nName=Steam\nExec=wine steam.exe\nIcon=steam\n",
    )
    .unwrap();

    let report = doctor::run(&sandbox.paths).unwrap();
    assert!(report.orphaned_icons.is_empty(), "{:?}", report.orphaned_icons);
    assert!(report.leftover_files.is_empty(), "{:?}", report.leftover_files);
    assert!(report.broken_entries.is_empty(), "{:?}", report.broken_entries);

    // Nothing of the foreign files is gone or even mentioned.
    for icon in foreign_icons {
        assert!(sandbox.paths.icons_root.join(icon).is_file());
    }
    assert!(sandbox.paths.appimage_dir.join("osu.AppImage").is_file());
}

#[test]
fn doctor_reports_only_leftovers_of_managed_slugs() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let outcome = install_fake(&sandbox, "Fake_App-1.0.0.AppImage");

    // An update leaves these behind, all of them named after a slug appimg
    // manages. The first two are appimg's own, the other two are what
    // `appimageupdatetool` leaves next to the file it updated.
    let backup = sandbox.paths.appimage_dir.join("fake-app.AppImage.bak");
    let staged = sandbox.paths.appimage_dir.join("fake-app.AppImage.new");
    let zs_old = sandbox.paths.appimage_dir.join("fake-app.AppImage.zs-old");
    let part = sandbox.paths.appimage_dir.join("fake-app.AppImage.part");
    fs::write(&backup, "the previous version").unwrap();
    fs::write(&staged, "half a download").unwrap();
    fs::write(&zs_old, "the previous version, all 371 MB of it").unwrap();
    fs::write(&part, "half a delta").unwrap();
    // Same shape, but for something appimg never installed.
    fs::write(sandbox.paths.appimage_dir.join("osu.AppImage.zs-old"), "not ours").unwrap();

    let report = doctor::run(&sandbox.paths).unwrap();
    assert_eq!(
        report.leftover_files,
        vec![backup.clone(), staged.clone(), part.clone(), zs_old.clone()]
    );
    assert!(report.orphaned_icons.is_empty());
    assert!(!report.is_clean());

    // Once the entry falls back to the generic icon, the icons installed
    // under the slug are ours and unused.
    let mut entry = DesktopEntry::read(&outcome.desktop_entry_path).unwrap();
    entry.set("Icon", install::FALLBACK_ICON);
    entry.write(&outcome.desktop_entry_path).unwrap();
    for leftover in [&backup, &staged, &zs_old, &part] {
        fs::remove_file(leftover).unwrap();
    }

    let report = doctor::run(&sandbox.paths).unwrap();
    assert!(report.leftover_files.is_empty());
    assert_eq!(report.orphaned_icons, outcome.icons);
}

#[test]
fn missing_optional_tools_are_not_a_problem() {
    let _serial = common::serial();
    let report = doctor::DoctorReport {
        libfuse2: true,
        xdg_data_home_in_search_path: true,
        applications_dir_writable: true,
        required_tools: vec![doctor::ToolStatus {
            name: "update-desktop-database".to_string(),
            found: true,
            consequence: String::new(),
        }],
        optional_tools: vec![
            doctor::ToolStatus {
                name: "appimageupdatetool".to_string(),
                found: false,
                consequence: String::new(),
            },
            doctor::ToolStatus {
                name: "unsquashfs".to_string(),
                found: false,
                consequence: String::new(),
            },
        ],
        orphaned_icons: Vec::new(),
        leftover_files: Vec::new(),
        broken_entries: Vec::new(),
    };

    assert!(report.is_clean(), "optional tooling never decides whether something is wrong");
}

#[test]
fn remove_leaves_nothing_behind() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    install_fake(&sandbox, "Fake_App-1.0.0.AppImage");
    // A failed update left these behind, removal has to take them along.
    let stale = sandbox.paths.appimage_dir.join("fake-app.AppImage.new");
    let zs_old = sandbox.paths.appimage_dir.join("fake-app.AppImage.zs-old");
    fs::write(&stale, "half a download").unwrap();
    fs::write(&zs_old, "the version appimageupdatetool replaced").unwrap();

    let plan = remove::plan(&sandbox.paths, "fake-app").unwrap();
    assert!(plan.files().contains(&stale));
    assert!(plan.files().contains(&zs_old));
    assert!(plan.files().len() >= 5);

    remove::remove(&sandbox.paths, "fake-app").unwrap();
    assert!(list::list(&sandbox.paths).unwrap().is_empty());
    assert!(walk(&sandbox.paths.appimage_dir).is_empty());
    // `update-desktop-database` leaves its own cache in there, that is not ours.
    assert!(walk(&sandbox.paths.applications_dir)
        .iter()
        .all(|path| path.file_name().unwrap() == "mimeinfo.cache"));
    assert!(walk(&sandbox.paths.icons_root)
        .iter()
        .all(|path| path.file_name().unwrap() == "index.theme"));

    let report = doctor::run(&sandbox.paths).unwrap();
    assert!(report.orphaned_icons.is_empty());
    assert!(report.leftover_files.is_empty());
    assert!(report.broken_entries.is_empty());

    assert!(matches!(remove::remove(&sandbox.paths, "fake-app"), Err(Error::NotInstalled(_))));
}

#[test]
fn updating_from_a_local_file_keeps_manual_edits() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let origin = sandbox.downloads.join("Fake_App.AppImage");
    FakeAppImage::new("Fake App").marker("v1").build(&sandbox.downloads, "Fake_App.AppImage");

    let info = metadata::inspect(&origin, None).unwrap();
    let mut request = InstallRequest::from_info(&origin, &origin.to_string_lossy(), &info);
    request.name = "My Renamed App".to_string();
    request.categories = vec!["Graphics".to_string()];
    request.extra_args = vec!["--enable-something".to_string()];
    request.version = Some("1.0.0".to_string());
    let outcome = install::install(&sandbox.paths, &request).unwrap();
    assert_eq!(outcome.slug, "my-renamed-app");

    // The origin file gets a new build, as a manual re-download would.
    FakeAppImage::new("Fake App")
        .marker("v2")
        .icon_sizes(&[128])
        .build(&sandbox.downloads, "Fake_App.AppImage");

    let app = list::find(&sandbox.paths, "my-renamed-app").unwrap();
    assert!(matches!(update::source_for(&app), update::UpdateSource::LocalFile { .. }));

    let result = update::update(&sandbox.paths, &app, None).unwrap();
    assert!(read(&result.appimage_path).contains("v2"));
    let backup = result.backup_path.clone().unwrap();
    assert!(backup.is_file());
    assert!(read(&backup).contains("v1"));

    let entry = DesktopEntry::read(&sandbox.paths.desktop_entry_path("my-renamed-app")).unwrap();
    assert_eq!(entry.get("Name"), Some("My Renamed App"));
    assert_eq!(entry.categories(), vec!["Graphics"]);
    assert!(entry.get("Exec").unwrap().contains("--enable-something"));

    // Icons come from the new version.
    let icons = walk(&sandbox.paths.icons_root);
    assert!(icons.contains(&"128x128/apps/my-renamed-app.png".into()), "{icons:?}");

    // Whatever else the update left next to the AppImage goes with the
    // backup: `appimageupdatetool` writes these, and never cleans them up.
    let zs_old = sandbox.paths.appimage_dir.join("my-renamed-app.AppImage.zs-old");
    fs::write(&zs_old, "the previous version, a second time").unwrap();

    update::confirm(&sandbox.paths, "my-renamed-app").unwrap();
    assert!(!backup.exists());
    assert!(!zs_old.exists());
    assert!(update::leftovers(&sandbox.paths, "my-renamed-app").is_empty());
}

#[test]
fn a_rollback_puts_the_previous_version_back() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let origin = sandbox.downloads.join("Fake_App.AppImage");
    FakeAppImage::new("Fake App").marker("v1").build(&sandbox.downloads, "Fake_App.AppImage");
    let info = metadata::inspect(&origin, None).unwrap();
    let request = InstallRequest::from_info(&origin, &origin.to_string_lossy(), &info);
    install::install(&sandbox.paths, &request).unwrap();

    FakeAppImage::new("Fake App").marker("v2").build(&sandbox.downloads, "Fake_App.AppImage");
    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    let result = update::update(&sandbox.paths, &app, None).unwrap();
    assert!(read(&result.appimage_path).contains("v2"));

    update::rollback(&sandbox.paths, "fake-app").unwrap();
    assert!(read(&result.appimage_path).contains("v1"));
    assert!(is_executable(&result.appimage_path));
    assert!(!update::backup_path(&sandbox.paths, "fake-app").exists());
}

#[test]
fn check_reports_without_touching_anything() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let origin = sandbox.downloads.join("Fake_App-1.0.0.AppImage");
    FakeAppImage::new("Fake App").build(&sandbox.downloads, "Fake_App-1.0.0.AppImage");
    let info = metadata::inspect(&origin, None).unwrap();
    install::install(
        &sandbox.paths,
        &InstallRequest::from_info(&origin, &origin.to_string_lossy(), &info),
    )
    .unwrap();

    let before = walk(&sandbox.paths.data_home);
    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    let status = update::check(&app).unwrap();
    assert_eq!(status.current_version.as_deref(), Some("1.0.0"));
    assert_eq!(status.latest_version.as_deref(), Some("1.0.0"));
    assert!(!status.available);
    assert_eq!(walk(&sandbox.paths.data_home), before);

    // A newer file at the recorded origin is an available update.
    let newer = sandbox.downloads.join("Fake_App-2.0.0.AppImage");
    FakeAppImage::new("Fake App").build(&sandbox.downloads, "Fake_App-2.0.0.AppImage");
    let mut app = list::find(&sandbox.paths, "fake-app").unwrap();
    app.origin = Some(newer.to_string_lossy().into_owned());
    let status = update::check(&app).unwrap();
    assert!(status.available);
    assert_eq!(status.latest_version.as_deref(), Some("2.0.0"));
    assert_eq!(walk(&sandbox.paths.data_home), before);
}

#[test]
fn an_app_without_a_source_cannot_be_updated() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let outcome = install_fake(&sandbox, "Fake_App-1.0.0.AppImage");
    let mut entry = DesktopEntry::read(&outcome.desktop_entry_path).unwrap();
    entry.remove(desktop_entry::KEY_SOURCE);
    entry.write(&outcome.desktop_entry_path).unwrap();

    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(update::source_for(&app), update::UpdateSource::None);
    assert!(update::check(&app).unwrap().note.is_some());
    assert!(matches!(update::update(&sandbox.paths, &app, None), Err(Error::NoUpdateSource(_))));
}

#[test]
fn a_missing_source_file_is_an_error_not_a_panic() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let missing = sandbox.downloads.join("gone.AppImage");
    let request = InstallRequest::from_info(&missing, "local", &Default::default());
    // No name at all cannot even produce a slug.
    assert!(matches!(install::install(&sandbox.paths, &request), Err(Error::InvalidName(_))));

    let mut named = request.clone();
    named.name = "Ghost".to_string();
    assert!(matches!(install::install(&sandbox.paths, &named), Err(Error::NotFound(_))));

    named.name = "///".to_string();
    assert!(matches!(install::install(&sandbox.paths, &named), Err(Error::InvalidName(_))));
}

/// The only test that touches the environment: nothing else reads these
/// variables, every other test builds its `Paths` directly.
#[test]
fn the_environment_decides_where_everything_lives() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let elsewhere = sandbox.root.join("elsewhere");

    std::env::set_var("XDG_DATA_HOME", sandbox.root.join("xdg-data"));
    std::env::set_var("XDG_CONFIG_HOME", sandbox.root.join("xdg-config"));
    std::env::set_var("APPIMG_DIR", &elsewhere);
    let paths = appimg_core::Paths::from_env().unwrap();
    std::env::remove_var("XDG_DATA_HOME");
    std::env::remove_var("XDG_CONFIG_HOME");
    std::env::remove_var("APPIMG_DIR");

    assert_eq!(paths.data_home, sandbox.root.join("xdg-data"));
    assert_eq!(paths.config_home, sandbox.root.join("xdg-config"));
    assert_eq!(paths.appimage_dir, elsewhere);
    assert_eq!(paths.applications_dir, sandbox.root.join("xdg-data/applications"));
    assert_eq!(paths.icons_root, sandbox.root.join("xdg-data/icons/hicolor"));
    assert_eq!(paths.appimage_path("app"), elsewhere.join("app.AppImage"));
}

/// An AppImage downloaded through a browser has no executable bit, and the
/// runtime cannot extract itself without one.
#[test]
fn an_appimage_without_the_executable_bit_still_extracts() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let source = FakeAppImage::new("Fake App")
        .not_executable()
        .build(&sandbox.downloads, "Fake_App-1.0.0.AppImage");
    assert!(!is_executable(&source));

    let info = metadata::inspect(&source, None).unwrap();
    assert!(info.extract_problems.is_empty(), "{:?}", info.extract_problems);
    assert!(info.extract_root().is_some());
    assert_eq!(info.name.as_deref(), Some("Fake App"));
    assert!(is_executable(&source), "the file gets the bit it needs to run");
}

/// A runtime that refuses to extract has to say why, with its own words.
#[test]
fn a_failing_runtime_reports_its_exit_status_and_message() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let source = FakeAppImage::new("Fake App")
        .failing(3, "runtime: cannot open the payload")
        .build(&sandbox.downloads, "Fake_App-1.0.0.AppImage");

    let info = metadata::inspect(&source, None).unwrap();
    assert!(info.extract_root().is_none());

    let problems = info.extract_problems.join("\n");
    assert!(problems.contains("exited with 3"), "{problems}");
    assert!(problems.contains("cannot open the payload"), "{problems}");
    // Nothing here is about FUSE, the runtime extracts without it.
    assert!(!problems.to_lowercase().contains("fuse"), "{problems}");
    // A shell script carries no squashfs payload, and that is said plainly.
    assert!(problems.contains("no squashfs payload"), "{problems}");

    // The name still falls back to the file name, so installing stays possible.
    assert_eq!(info.name.as_deref(), Some("Fake_App"));
}
