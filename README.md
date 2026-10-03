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

The entry is registered right away, though some launchers only read their
application list at startup.

The output of `list --json` and `update --check --json` is not a stable
interface: its field set may change between versions without notice.

## Updates

    appimg update --all --check    check without changing anything
    appimg update --all            download and replace

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

GitHub's API answers 60 requests an hour without a token, which checking a
few applications more than once can use up. With `GH_TOKEN` or `GITHUB_TOKEN`
set, appimg sends it as a bearer token to `api.github.com` over https, and to
no other host: never with a download, and never along a redirect, wherever it
leads. `GH_TOKEN` comes first, as with `gh`.

## Where things go

    $XDG_DATA_HOME/appimages/<name>.AppImage      the binary
    $XDG_DATA_HOME/applications/<name>.desktop    the launcher entry
    $XDG_DATA_HOME/icons/hicolor/*/apps/          every icon size it found
