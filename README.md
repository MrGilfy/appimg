# appimg

Installs, updates and removes AppImages as proper desktop applications, entirely
inside `$HOME`.


https://github.com/user-attachments/assets/c6752237-69ac-4ea1-ba8a-b70a516be8b7


## Install

Arch, from the AUR:

    yay -S appimg        # builds from source
    yay -S appimg-bin    # prebuilt binary

Anywhere else:

    cargo install appimg

Requires Rust 1.88.

## Usage

Run `appimg` without arguments for the TUI, or go straight to a command:

    appimg install ./someapp.AppImage
    appimg install https://example.com/someapp.AppImage
    appimg adopt ~/Applications/someapp.AppImage
    appimg adopt --scan
    appimg export apps.json
    appimg import apps.json
    appimg list --json
    appimg update --all --check
    appimg remove someapp
    appimg doctor
    appimg clean --dry-run

The entry is registered right away, though some launchers only read their
application list at startup.

The output of `list --json` and `update --check --json` is not a stable
interface: its field set may change between versions without notice.

## Updates

    appimg update --all --check    check without changing anything
    appimg update --all            download and replace
    appimg notify enable           check daily, notify when updates are available

No extra tool is needed. An application whose update information names a zsync
file, either `zsync|<url>` or `gh-releases-zsync|...`, fetches only the parts
of the new version it does not already have and verifies the assembled file
before installing it. If the delta fails, it downloads the whole file instead
and verifies that the same way. Every other source downloads the whole thing.
Each update says which it was and what it cost:

      reused 19054 of 46308 blocks, fetched 107.0 MB in 22 requests

The update information embedded in an AppImage comes first. An AppImage
without any updates from its update source, which an AppImage downloaded
from a URL starts out with. One installed from a local file is updated
manually until it gets one:

    appimg install ./App.AppImage --update-source github:owner/repo
    appimg update-source app github:owner/repo@continuous
    appimg update-source app https://example.com/App.AppImage
    appimg update-source app --clear
    appimg update-source app       show where it updates from

`github:owner/repo` follows the newest release that has the installed
AppImage in it, passing over drafts, pre-releases and releases that ship
other platforms only. `@tag` follows that tag instead, for projects that
publish under a moving tag such as `continuous`. Update information of the
form `gh-releases-zsync|owner|repo|latest|...` is followed the same way, to
the newest release with a zsync file its pattern fits.
The file to download is the one whose name matches the installed file,
versions aside. When the AppStream metadata inside an AppImage links a
GitHub repository, the install suggests it as the update source.

A URL can be a vendor's download link that redirects to the current
version, such as `https://lmstudio.ai/download/latest/linux/x64`, or a fixed
name whose file changes. A check follows it with HEAD requests, one per
redirect, and downloads nothing. A version in the name of the file the link
lands on decides. Without one, the path it lands on, the `ETag`, the
`Last-Modified` date and the `Content-Length` are compared with what the
server said when the installed file was downloaded, which its entry keeps.
The host and query of the URL never count: CDNs rotate the one and sign the
other. A file the server changed under a version that is installed already
is no update, the check says so in a note. A server that says nothing about
its files leaves it to the update, which downloads the file and keeps the
installed one when it is the same.

An application can be held at the version it has:

    appimg hold app        update --all, the terminal interface and notifications pass it over
    appimg unhold app      it updates like any other again

Holding never hides an update. `update --check` still checks a held
application and shows it as held next to what it found, and `list` and the
terminal interface show what the last check of it found and when.
`update --all` names the update it passes over. `appimg update app` asks
before it updates a held application, and with `--yes` updates it and
keeps the hold. An export carries the hold, and an import holds the
application again.

GitHub's API answers 60 requests an hour without a token, which checking a
few applications more than once can use up. With `GH_TOKEN` or `GITHUB_TOKEN`
set, appimg sends it as a bearer token to `api.github.com` over https, and to
no other host: never with a download, and never along a redirect, wherever it
leads. `GH_TOKEN` comes first, as with `gh`.

Releases on GitLab and on Forgejo, which Codeberg runs, are followed the
same way, `@tag` and `--asset` patterns included:

    appimg update-source app gitlab:group/subgroup/project
    appimg update-source app codeberg:owner/repo
    appimg update-source app gitlab:https://invent.kde.org/group/project
    appimg update-source app forgejo:https://git.example.org/owner/repo

GitLab marks no release as a pre-release, so one whose tag names one
(`rc`, `beta`, `alpha`, `pre`, `preview`) or a moving build such as
`nightly` is passed over unless `@tag` names it. Neither forge publishes a
digest in the release, so an update checks the download against the
SHA-256 the package registry of a GitLab project knows for it, or else a
checksum file in the release, `<file>.sha256`, `<file>.sha256sum` or
`SHA256SUMS`; a check never asks for either. A token goes to the API of
the host it is for, over https, in the `Authorization` header and nowhere
else: `GITLAB_TOKEN` for gitlab.com, `CODEBERG_TOKEN` for codeberg.org, and
`APPIMG_TOKEN_<HOST>` for any host, such as `APPIMG_TOKEN_INVENT_KDE_ORG`.

## Where things go

    $XDG_DATA_HOME/appimages/<name>.AppImage      the binary
    $XDG_DATA_HOME/applications/<name>.desktop    the launcher entry
    $XDG_DATA_HOME/icons/hicolor/*/apps/          every icon size it found
