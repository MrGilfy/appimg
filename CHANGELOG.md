# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.1] - 2026-09-02

### Fixed

- A delta update is faster than downloading the whole file again, which it
  was not before: the connections it fetched over were not being reused, so
  every range paid for a new one. Updating ImHex 1.38.0 to 1.38.1 takes 16
  seconds where it took 46, against 44 seconds for the full download it
  replaces.

## [0.2.0] - 2026-09-02

### Added

- appimg applies zsync delta updates itself. An update of an application
  whose update information points at a zsync file no longer downloads the
  whole AppImage: appimg works out which blocks of the new version the
  installed file already holds, fetches only the ranges it is missing, and
  checks the assembled file against the checksum the zsync file carries
  before installing it. A file that does not match that checksum is thrown
  away and the installed version stays where it is.
- Every update says which path it took and what it cost:

      Updating ImHex...
        1.38.0 -> 1.38.1
        reused 19054 of 46308 blocks, fetched 107.0 MB in 22 requests

  A source with no delta to apply says `no delta for this source,
  downloaded 42.0 MB`, and an update that fell back to `appimageupdatetool`
  says so too, along with what stopped appimg from doing it.

### Changed

- `appimageupdatetool` no longer has to be installed. It is used only when
  appimg's own delta path fails, and `doctor` now says as much rather than
  reporting that delta updates cannot be applied without it.

### Fixed

- An application whose update information is `gh-releases-zsync|...` gets a
  delta update instead of a full download. The asset such an update
  information names is a zsync file, and appimg treated it as an AppImage to
  download, so every update of ImHex, and of everything else published this
  way, fetched the whole file. `appimg list` and `update --check` called
  those applications GitHub sources; they are zsync sources and are now
  shown as such. Updating ImHex 1.38.0 to 1.38.1 fetched 190 MB before and
  fetches 107 MB now.
- The asset pattern of a `gh-releases-zsync` source is matched properly,
  including a placeholder like `{{ARCHITECTURE_FILE_NAME}}` that a build
  system left behind, and including projects that call a 64 bit ARM build
  `arm64` rather than `aarch64`.

## [0.1.3] - 2026-09-02

### Changed

- A release that keeps moving is shown by the day it was published,
  `2025-10-18`, instead of the build id its AppImages declare. The two
  builds of one AppImageUpdate continuous release called themselves
  `255-a211784` and `254-a211784`: the same commit, different build
  numbers, and neither a version anyone can act on. A release counts as a
  moving one when it names no version at all — anything with a dotted
  number is out, however much else it carries — and carries a marker of a
  build that keeps moving, either a word like `continuous` or `nightly` or
  an abbreviated commit hash. So `2.0.0-alpha-1-20251018` and the
  date-stamped `20251018` go on being shown as the versions they are, while
  `continuous` and `255-a211784` do not. Without a date, which is all an
  installed file on its own can offer, the commit is shown instead, so both
  of those builds read `a211784`.

### Fixed

- `update --check` compares a moving release like with like. Two dates
  order, so the check says whether the installed file is older and not
  merely different. The commit settles identity: on a channel that only
  ever moves forward the same commit is the same build, so a check names
  the day that build was published, and a different commit is an update. A
  build id is never ordered against a version; the check says it has
  nothing to compare rather than deciding by how the two happen to be
  spelled.
- An update follows the tag it was installed from when that tag is a moving
  one. `gh-releases-zsync|AppImage|AppImageUpdate|continuous|...` names
  `continuous`, and appimg asked for the latest release regardless, which
  on that repository is an entirely different release. A tag that names a
  version is still ignored in favour of the latest release, since following
  it would pin the application to the version it was installed at.
- A zsync source whose file name carries no version reports the day the
  offered file was built, out of the `MTime` of the header. Reading a
  version out of `appimageupdatetool-x86_64.AppImage` yielded the `64` of
  its architecture.

## [0.1.2] - 2026-09-01

### Fixed

- A zsync update no longer leaves a full copy of the previous version on
  disk. `appimageupdatetool` hard-links the file it replaces to
  `<slug>.AppImage.zs-old` and never deletes it, which costs as much as the
  AppImage itself. appimg now claims that copy as its `.bak`, so a delta
  update can be rolled back like every other one, and confirming the update
  drops it along with everything else the run left behind.
- `doctor` knows every name an update can leave next to an AppImage: the
  `.bak` and `.new` of appimg's own updates, and the `.zs-old` and `.part`
  of `appimageupdatetool`. It names what each file is and how much disk it
  takes, and `remove` deletes all four with the application.

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
