# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Every installed AppImage has an update source of its own, separate from
  where it was installed from, stored as `X-AppImg-UpdateSource`. It is a
  URL to download, `github:owner/repo` to follow the newest release that
  has the installed AppImage in it, or `github:owner/repo@tag` to follow
  that tag exactly as written. A release that is a draft, a pre-release or
  ships other platforms only, such as an Android build alone, is passed
  over, out of the first page of releases, in the one request a check
  makes. A link to
  a repository or to its releases on github.com is stored as
  `github:owner/repo`. The update information embedded in an AppImage still
  comes first whenever it has some.
- `appimg install --update-source <URL|github:owner/repo>` sets it at
  install time, and the install form in the TUI has an "Update from" field
  for it.
- `appimg update-source <name>` shows where an installed AppImage updates
  from, `appimg update-source <name> <URL|github:owner/repo>` sets it and
  `--clear` makes it manual, without a reinstall.
- When the AppStream metadata inside an AppImage links a GitHub repository,
  an install suggests it as the update source. The TUI prefills the field
  with it. `appimg install` asks, yes by default, takes it with `--yes` and
  says so, and on a pipe leaves it out and names the flag that sets it.
- With `GH_TOKEN` or `GITHUB_TOKEN` set, appimg sends the token to the
  GitHub API, which then allows 5000 requests an hour instead of 60. It goes
  to `api.github.com` over https and nowhere else: never with a download,
  never to another host, and never along a redirect. `GH_TOKEN` comes first,
  as with `gh`, and a token GitHub refuses is an error that names the
  variable it came from.
- A static aarch64 binary is released next to the x86_64 one, as
  `appimg-<version>-aarch64-linux-musl.tar.gz`, and `appimg-bin` on the AUR
  installs on aarch64 too.

### Changed

- Where an AppImage was installed from is history only. It stays in
  `X-AppImg-Source` and shows as "Origin" in the TUI. An AppImage installed
  from a local file no longer updates from that file, whether the file is
  still there or not: it is updated manually until it gets an update
  source. An entry written by 0.2.x is read as it is: one installed from a
  URL still updates from that URL.
- An AppImage with nothing to update from shows as `manual` instead of
  `none`, in `list`, in the TUI, and in the output of both `--json` options.
  `appimg update <name>` on one fails with a message that says why and
  names `appimg update-source`. `update --all` skips it and does not count
  it as a failure, and neither does updating everything in the TUI.
- Update information of the form `gh-releases-zsync` that follows `latest`
  takes the newest release with a zsync file its pattern fits, passing over
  drafts, pre-releases and releases that ship other platforms only. It used
  to take the latest release, which may hold nothing for Linux at all. A
  version tag in it has always meant the latest release and does the same;
  a tag that keeps moving, such as `continuous`, is still followed exactly.
  A check still makes one request.
- An update from a GitHub release picks its file by name. The parts of the
  installed file's name that are versions are ignored, and the rest of the
  name and the architecture have to match: `imhex-1.38.0-x86_64.AppImage`
  finds `imhex-1.38.1-x86_64.AppImage`, and `x64`, `amd64` and `x86_64` are
  the same architecture. When no file or more than one fits, the update
  fails and lists the AppImages of the release, where it used to take the
  first one that looked close.

### Fixed

- An AppImage that names its version differently from the version read
  out of its release tag no longer shows an update forever. osu! calls
  itself `2026.921.0-lazer` in the release of that tag, and the check
  offered `2026.921.0` as newer right after updating to it. An install or
  an update out of a GitHub release now records that release in the
  desktop entry as `X-AppImg-Release=github:owner/repo@tag`, and a check
  compares that tag with the tag of the release it would follow; a leading
  `v` makes no difference. Without a recorded tag, for an AppImage
  installed from a file or by 0.2.x, the versions are compared as before,
  except that two whose numbers are the same and that differ only in a
  trailing label without digits count as the same. A pre-release marker
  such as `-beta` is not such a label. The first update records the tag.
- A download that was cut off is no longer installed, and no longer
  replaces the installed version in an update. A server that sends neither
  a Content-Length nor a chunked body and hangs up early leaves a file that
  starts like an AppImage, so it got past the check for an ELF header. The
  front of the file says how long a complete one is at least: the ELF
  header says where the payload starts, and the squashfs superblock there
  says how long the payload is. A file that ends before its ELF section
  table, less than 48 bytes behind it, or before the end of its squashfs
  payload is now removed and refused with an error that names the URL, the
  length it needs at least and the length that arrived. The size of a type
  1 AppImage's payload is not checked, since it is not squashfs, and a
  32-bit AppImage is not checked at all.
- A power cut right after an update or an install no longer can leave an
  empty or partial AppImage under the installed name. The new file got that
  name by a rename, and the rename could reach the disk before the file did.
  The file is now flushed to disk before the rename, and the directory after
  it, in every update that installs a file appimg wrote: a full download, a
  zsync delta and a copy from a local file. The same goes for an install,
  from a file or a URL, including one that replaces an installed AppImage.
  A filesystem that refuses to flush the directory does not fail the update
  or the install, since the new file is complete on disk by then. An update that falls back to `appimageupdatetool` swaps the
  file itself, as before.
- `appimg edit`, and editing in the TUI, no longer fail when `EDITOR` is not
  set and there is no `vi`. The editor is now `VISUAL`, then `EDITOR`, then
  the first of `nvim`, `vim` and `nano` that is installed. `VISUAL` and
  `EDITOR` are split on whitespace into the program and its arguments, so
  `code --wait` and `subl -w` work; the arguments go before the file, and
  there is no quoting. A variable whose program does not exist is passed
  over, and only when nothing is found is that an error, one that says to
  set `EDITOR`. `VISUAL` now comes before `EDITOR`, where it used to come
  after it, and `vi` is no longer tried.

## [0.2.2] - 2026-10-01

### Changed

- The output of `list --json` and `update --check --json` is documented as
  unstable: the README and the `--help` of both say that its field set may
  change between versions without notice. The output itself is the same as
  in 0.2.1.

### Fixed

- An update that downloads the whole file no longer installs whatever the
  server sends. A file that arrived complete but is no AppImage at all, such
  as an error page sent with a 200, used to replace the installed version;
  anything that does not start with an ELF header is now refused before the
  swap, and the installed version stays where it is.
- `appimg install <url>` refuses the same kind of download. An error page
  sent with a 200 used to be installed as the application, with a desktop
  entry and the generic icon, whenever `--yes` answered the question about
  the missing metadata. Now nothing is installed and the error names the URL.

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
