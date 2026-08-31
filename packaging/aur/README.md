# Publishing appimg on the AUR

Two packages live here: `appimg` builds from the release tarball, `appimg-bin`
installs the prebuilt musl binary from the same GitHub release. They set
`provides` and `conflicts` against each other, so only one can be installed.

Everything below happens on an Arch machine with `base-devel`, `git` and
`pacman-contrib` installed.

## 1. Tag the release first

The AUR packages point at a GitHub tag, so the tag has to exist before the
package can be built:

```
git tag -a v0.1.0 -m "appimg 0.1.0"
git push origin v0.1.0
```

Wait for the release workflow to finish. `appimg-bin` needs the
`appimg-<version>-x86_64-linux-musl.tar.gz` asset it produces.

## 2. Prepare a working copy of the AUR repository

Each AUR package is its own git repository, unrelated to the GitHub one. Your
SSH key has to be registered in your AUR account first.

```
git clone ssh://aur@aur.archlinux.org/appimg.git aur-appimg
cd aur-appimg
cp ../appimg/packaging/aur/PKGBUILD .
```

For the binary package use `ssh://aur@aur.archlinux.org/appimg-bin.git` and copy
`PKGBUILD-bin` to `PKGBUILD` there. A fresh package starts as an empty
repository, that is expected.

## 3. Fill in the checksums

The committed PKGBUILDs carry `SKIP` as a placeholder. Never upload that:

```
updpkgsums
```

For `appimg-bin` the checksum is also in `sha256sums.txt` of the GitHub release,
which is worth comparing against.

## 4. Build it in a clean chroot

A build that works on your machine may still miss a dependency. The chroot
build is the one that counts:

```
extra-x86_64-build
```

This creates `/var/lib/archbuild/extra-x86_64/`, builds there with nothing but
`base-devel` and the declared dependencies, and fails if something is missing.
For a quick local check, `makepkg -si` is enough, but do not upload before the
chroot build passed.

## 5. Check the package

```
namcap PKGBUILD
namcap appimg-0.1.0-1-x86_64.pkg.tar.zst
```

`namcap` complains about missing dependencies, wrong permissions and files in
unusual places. Warnings about the Rust binary being statically linked are
expected for `appimg-bin`.

## 6. Generate .SRCINFO and push

`.SRCINFO` is what the AUR reads, and it is generated, never edited by hand:

```
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m "appimg 0.1.0-1"
git push
```

## Updating to a new version

1. Tag and push the new release on GitHub, wait for the workflow.
2. In the AUR repository: raise `pkgver`, set `pkgrel=1`, run `updpkgsums`.
3. Build in the clean chroot, run `namcap`, regenerate `.SRCINFO`.
4. Commit and push.

`pkgrel` only goes up when the package changes without the upstream version
changing, for example a fixed install path.
