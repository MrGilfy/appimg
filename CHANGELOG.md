# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.1] - 2026-08-31

### Added

- `update --check` reads the zsync file itself: the header carries the
  length and the checksums of the complete file, so a single ranged request
  decides whether the installed AppImage is still the one being offered.
  `appimageupdatetool` is only needed to apply a delta, and its absence no
  longer keeps a check from reporting a version or a size difference.

### Fixed

- Release notes are the changelog section of the tag that is being built.
  A tag without a section fails the release job instead of publishing a
  pointer to `CHANGELOG.md`, and the extraction is covered by a test that
  runs in CI.
- An update that needs `appimageupdatetool` says that the tool is missing
  instead of reporting that no update source was recorded.

## [0.1.0] - 2026-08-31

### Added

- `appimg-core`: installing, updating and removing AppImages entirely inside
  the user's home, with the desktop entry as the only source of truth.
- Terminal interface: table of installed applications, details, search, an
  install form with a file browser and a preview of the generated desktop
  entry, update, edit and remove, with a panic hook that restores the
  terminal.
- Packaging: PKGBUILDs for `appimg` and `appimg-bin`, CI that checks
  formatting, clippy and the tests, and a release workflow that publishes a
  static musl binary. The man page and the shell completions are generated
  from the clap definition during the build.
- Command line interface: `install`, `list`, `update`, `remove`, `edit`,
  `doctor` and `completions`, all scriptable, with `--json` output for `list`
  and `update --check`.
