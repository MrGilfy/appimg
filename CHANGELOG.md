# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- A URL update source can be a vendor's download link that redirects to
  the current version, such as LM Studio's, or a fixed name whose file
  changes, such as CurseForge's. A check follows the link with HEAD
  requests, one per redirect, and downloads nothing; a server that refuses
  HEAD is asked for its first byte. A newer version in the name of the file
  the link lands on is an update, taken from `Content-Disposition` when the
  server sends one. Without a version, the path it lands on, the `ETag`, a
  later `Last-Modified` or another `Content-Length` decide, in that order,
  against what the server said when the installed file was downloaded:
  installs and updates from a URL keep that in `X-AppImg-Remote`, each
  field percent-encoded and the whole tied to the checksum of the installed
  file, so a file replaced by any other means leaves a record that no longer
  counts. The host and query of the URL never count, CDNs rotate the one and
  sign the other. A file the server changed under a version already
  installed is no update, the check says so in a note. A link that ends at
  an error page is an error that says where it ended, and one that lands on
  a GitHub release names the `github:` source that would also check the
  release digests. A server that says nothing about its files still gets a
  full download, which now keeps the installed file when it turns out to be
  the same. An install from such a link takes the name and version of the
  file it lands on. (#28)
- The terminal interface and `appimg adopt` take a zip or tar archive with
  an AppImage inside, the way `install` and `update` do: the AppImage is
  found by its bytes, never by its name, the archive has to hold exactly
  one, nothing it names is ever a path anything is written to, and it may
  unpack to no more than its own size allows. What comes out gets the
  checks any AppImage gets, and the entry records the archive as where it
  came from. The file browser of the terminal interface lists archives
  beside AppImages. `adopt` leaves the archive where it is, with whatever
  else it holds, `--copy` or not, and leaves no link in its place. (#26)
- End-to-end tests for `import` and `adopt --asset`: an import that picks
  a release archive by its asset pattern where the file name cannot, keeps
  the pattern, and checks the archive against its digest before unpacking
  it; an import from an archive URL; and `adopt --asset`, kept with the
  update source on the command line or the one the AppStream metadata
  suggests, refused without a GitHub source, and followed by the next
  update. (#25)

### Changed

- The AppImage taken out of an archive on disk is named after the archive
  until it is installed, so the name and version a file name gives come
  from the archive when the metadata inside cannot be read, as in a dry
  run without `unsquashfs`. When it is no complete AppImage, the error
  names the archive and the entry it came from.
- A `github:` update source shows the way the desktop entry stores it,
  `github:owner/repo@tag#pattern`, wherever it is shown: after an install
  or adoption, in `list` and `update --check`, in the terminal interface
  and in `update-source`, which showed `github:owner/repo` alone. The
  `update_source` field of `list --json` and the `source` field of
  `update --check --json` carry the same, tag and pattern included,
  escaped like every other string there.

### Fixed

- A version read from a file name keeps a numeric build number behind it:
  `LM-Studio-0.4.25-1-x64.AppImage` is `0.4.25-1`, no longer `0.4.25`.
  `-1` and `+1` are the same build number and compare as numbers, so
  `0.4.25-1` equals the `0.4.25+1` LM Studio's metadata declares, and a
  rebuild as `0.4.25-2` is newer. A version without a build number stands
  for any build of it. Pre-release markers such as `-beta`, `-rc` and
  `-alpha` keep their meaning. `update --check` had reported the installed
  LM Studio as newer than the one its download link offered. (#28)

## [0.4.1] - 2026-10-04

### Changed

- An install or adoption the user asked for, an import and an update read
  the metadata through `unsquashfs` first as well, and only run the
  AppImage's own runtime when `unsquashfs` is not installed or cannot read
  the file, as with a type 1 AppImage. The check that an updated AppImage
  runs, before its backup goes, still runs it. `doctor` says what a
  missing `unsquashfs` costs now.

### Fixed

- `install --dry-run` and `adopt --dry-run` ran the AppImage they were
  meant to preview: reading the metadata ran its runtime with
  `--appimage-extract`, after setting the executable bit on a file that
  had none. Nothing before a confirmed install runs it anymore: not a dry
  run, and not the TUI while it fills in the install form. They read the
  metadata through `unsquashfs` from the squashfs payload, which runs
  nothing of the file. Without `unsquashfs` they read no metadata: a dry
  run says so and why, and shows the plan with the name from the file name
  and the generic icon, and the TUI says the same in its status line and
  installs what the form shows once it is confirmed. (#27)

## [0.4.0] - 2026-10-03

### Added

- Every AppImage that comes out of a GitHub release is checked against
  the SHA-256 digest GitHub publishes for it before it replaces anything:
  updates from a `github:` or `gh-releases-zsync` source, whether
  downloaded whole or assembled from a zsync delta, and installs from a
  release download URL, which costs one request for that release. An
  install checks the file before reading its metadata, which runs it. A
  file that does not match is deleted and refused with an error that names
  both digests, the way a failed zsync checksum is, and the installed
  version stays as it was. An asset without a digest, as on releases from
  before GitHub published them, is installed as before. Either way, the
  install and update output says what the check found in one line.
- `appimg adopt <path>` takes over an AppImage that is already on disk,
  without downloading it again. It moves into the appimages directory, or
  with `--copy` is copied and the original stays. A move across
  filesystems copies, checks the copy against the original and deletes the
  original only once the entry is written. One in `~/.local/bin` leaves a
  symbolic link to the adopted file in its place, so the command keeps
  working. It gets the checks, desktop entry, icons and update source an
  install gives it, with the original path as its origin, and a slug that
  is taken is refused with `--name` named. Desktop entries from elsewhere
  whose `Exec` runs exactly that file, such as AppImageLauncher's
  `appimagekit_*.desktop`, are listed and removed when confirmed, together
  with the icons AppImageLauncher made for that file when no other entry
  uses them. `--keep-entries` keeps them. One of them already at
  `<slug>.desktop` does not take the slug: when they go, the adopted entry
  is written over it, and its icons named after the slug go, every size of
  them, before the adopted icons take that name, unless another entry uses
  them too. Only while they stay is the slug taken. A
  failure on the way puts everything back, and gives an entry or icons it
  wrote over their exact bytes back.
- `appimg adopt --scan` lists the AppImages appimg does not manage in its
  appimages directory, `~/Applications` and `~/.local/bin`, with the
  foreign entries that launch each one and the exact `appimg adopt`
  command for it. It runs none of them and changes nothing.
- `appimg export [FILE]` writes every managed AppImage to a versioned JSON
  file, or to standard output: slug, name, comment, categories, launch
  arguments, terminal flag, update source, origin, and the installed
  version for information. `appimg import FILE` installs them on another
  machine, downloading each again, from the first of these it has: the
  newest matching AppImage of a `github:` update source, picked by the
  file name it was installed from; an update source that is a URL; the
  URL it was installed from, brought up to date right away when it has
  something to update from. The entry gets what the export holds, under
  the slug it had, without asking about AppStream. Downloads get the
  digest, ELF and squashfs checks an install gives them. An app that is
  installed already is skipped, one with none of these is listed at the
  end with what it needs, and one that fails does not stop the others. One
  whose update right after the install fails stays installed at the
  version it was installed from, and the output says only the update
  failed. The exit code is 1 if anything failed. A format version it does
  not know is refused, and `--dry-run` shows the plan and changes nothing.
- `appimg notify enable` turns on update notifications. It writes a
  systemd user service and timer to `$XDG_CONFIG_HOME/systemd/user` and
  starts the timer through `systemctl --user`: nothing system-wide, no
  root. Once a day, a missed day caught up after the next boot, and up to
  an hour later at random so that not every machine asks GitHub in the
  same minute, the timer runs the appimg that enabled it, by its absolute
  path. That checks every application the way `update --all --check` does
  and shows one notification naming the updates no notification named
  before, through `notify-send` or, without it, `gdbus` calling
  `org.freedesktop.Notifications`. Each version is announced once: the
  ones announced are remembered in `$XDG_STATE_HOME/appimg/announced`, and
  an update that is still pending stays quiet until a newer version
  appears. When everything is current, or nothing is new, it shows
  nothing. A check that fails is logged to the journal and fails the run,
  shows no notification, and forgets nothing that was announced for that
  application. The service keeps `XDG_DATA_HOME`, `APPIMG_DIR` and
  `XDG_STATE_HOME` as they were when `enable` ran, so the timer checks the
  same applications and keeps one record of what it announced. An appimg
  in a cargo target directory or a temporary one gets a warning.
  `appimg notify status` shows whether the timer is on, the next check,
  the last one and how it went, and whether the appimg it runs still
  exists, and exits with 1 when it does not. `appimg notify disable` stops
  and removes both units and that record, and `appimg notify test` shows a
  sample notification right away. Without `notify-send` and `gdbus`,
  `enable` and `test` say so and change nothing.
- AppImages shipped inside an archive install and update like any other:
  `appimg install` takes a local zip, tar or tar.gz file or a URL that
  serves one, and an update from a `github:` source or a URL takes the
  archive a release ships, the way HarbourMasters/Shipwright ships
  `soh.appimage` inside `SoH-<codename>-Linux.zip`. The AppImage is found
  inside by its magic bytes, the ELF header with `AI` and its type, never
  by its name, and it has to be exactly one: an archive with none or
  several is refused with what it holds. Encrypted zip entries, zip64 and
  compression methods other than stored and deflate are refused by name.
  No name, path or link out of the archive is ever written: the one
  AppImage is unpacked straight to appimg's own staging file, and to no
  more than four times the archive's size plus 64 MiB, whatever the
  archive claims, so a broken or hostile one cannot fill the disk. A
  download from a GitHub release is checked against its published digest
  as the archive, before anything is unpacked, and the AppImage then gets
  the checks every download gets: ELF header, squashfs length, flushed to
  disk before it takes the installed name. The update output says which
  file came out of the archive. zip and tar are read by appimg itself on
  top of `flate2`, which the HTTP client already built in, so no crate was
  added; xz-compressed tar files are not read, since that would take one.
- `--asset '<pattern>'` on `install`, `adopt` and `update-source` picks
  the file out of each GitHub release instead of the name of the
  installed one: a file name in which `*` stands for whatever changes
  between releases, matched without regard to case, such as
  `'SoH-*-Linux.zip'`. It is kept with the update source, as
  `github:owner/repo#SoH-*-Linux.zip`, and travels with it through export
  and import. `update-source <app> --asset <pattern>` adds it to the
  current source, and setting a source without it drops it. A source that
  follows no GitHub release refuses one.

### Changed

- A `github:` source whose release has no file matching the installed
  one by name no longer gives up. It sets aside the AppImages and
  archives naming another platform, `mac`, `macos`, `osx`, `darwin`,
  `win` or `windows` with or without digits behind them, and `android`,
  and those built for another machine, and takes what is left if that is
  exactly one: a project that renames its files every release still
  ships one build for Linux. More than one left is an error that lists
  them and names `--asset`. Archives only count when no AppImage matches,
  so a release that ships an AppImage beside zips for other platforms is
  read as before.
- A zsync check whose offered file is as large as the installed one no
  longer reads all of the installed file to compare checksums. Install,
  update and adopt store the SHA-1 of every AppImage they write in its
  desktop entry, as `X-AppImg-SHA1`, together with the file's size and
  modification time. A check takes the stored checksum while size and
  time still match, and hashes the file again only when either changed,
  storing the result. An entry written before there was a checksum in it
  is hashed once, the first time a check needs it. A zsync update stores
  the checksum it already verified the new file against, without reading
  the file again.
- A local install, from the command line or the TUI, gets the checks a
  download gets before anything runs the file: it has to start with an ELF
  header and be at least as long as its squashfs says.

- A zsync update whose delta fails, for any reason, downloads the complete
  file the zsync file names instead of handing over to
  `appimageupdatetool`. That file goes the way every other update goes: it
  is held to the length and checksum in the zsync file, checked against the
  GitHub digest where there is one, flushed to disk and swapped in with the
  previous version kept for a rollback. The update says that the delta
  failed, why, and what the whole file cost:
  `the delta failed, downloaded the whole file instead, 107.0 MB: ...`. A
  zsync file that cannot be read at all names no file to fall back on, and
  the update fails without changing anything.

### Removed

- The fallback to `appimageupdatetool`, and with it the `doctor` line about
  the tool. No external tool downloads or applies an update anymore. The
  `.zs-old` and `.part` files it left next to an AppImage under an older
  appimg are still reported by `doctor` and still removed with the next
  confirmed update or with `appimg remove`.

### Fixed

- A `gh-releases-zsync` pattern without a `*` matches that one file name,
  not every name that starts with it, such as a `.zsync.sha256` beside the
  zsync file.
- A `github:` source on an aarch64 machine finds the arm64 AppImage of a
  release that names its x86_64 build without an architecture, the way
  electron-builder does: `Obsidian-1.13.8.AppImage` next to
  `Obsidian-1.13.8-arm64.AppImage`. Both used to count as this machine's,
  and the check stopped at "2 of its AppImages match". An AppImage that
  names this machine's architecture now beats one that names none, and an
  unlabeled AppImage only counts when no AppImage of the release names this
  machine's architecture. A release of unlabeled AppImages alone works as
  before, on either machine.

## [0.3.0] - 2026-10-02

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
