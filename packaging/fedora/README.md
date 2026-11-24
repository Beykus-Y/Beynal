# Fedora / COPR packaging

Beyrix is published to the [beykus-y/beyrix](https://copr.fedorainfracloud.org/coprs/beykus-y/beyrix/)
COPR project for Fedora 44 and 45 on x86_64.

## Install

```sh
sudo dnf copr enable beykus-y/beyrix
sudo dnf install beyrix
```

Then start it from a Wayland session:

```sh
beyrix
```

## What the package contains

- `/usr/bin/beyrix` — the terminal emulator built with the Fedora toolchain.
- `/usr/bin/bedit` — the project editor launcher.
- Desktop entries for Beyrix and BEdit and AppStream metadata for Beyrix.
- `cargo-license.txt` and `cargo-vendor.txt` in `/usr/share/licenses/beyrix/`
  describing every vendored crate that is linked into the binary.

Dependencies that Beyrix loads with `dlopen()` at runtime are declared
explicitly, because RPM's automatic dependency generator cannot see them:
`libwayland-client` is required, while `libwayland-egl`, `libglvnd-egl`,
`libglvnd-gles` and `vulkan-loader` are recommended for hardware rendering.
The software renderer works without them.

## Build locally

Fedora 44 or newer:

```sh
sudo dnf install rpm-build cargo-rpm-macros rust cargo gcc pkgconf-pkg-config \
    libxkbcommon-devel fontconfig dejavu-sans-mono-fonts
./packaging/fedora/make-sources.sh v0.1.0-alpha.2
```

The script archives the local tag or downloads the tagged source, runs `cargo vendor --locked`,
creates a reproducible vendor archive and builds the SRPM in `dist/fedora/`.
It uses the tag's commit timestamp as `SOURCE_DATE_EPOCH`.

Build the binary RPM from the SRPM:

```sh
sudo dnf builddep dist/fedora/beyrix-0.1.0~alpha.2-1.fc44.src.rpm
rpmbuild --rebuild dist/fedora/beyrix-0.1.0~alpha.2-1.fc44.src.rpm
```

## Publish a new release

1. Update the Cargo version and lockfile, the RPM spec, AppStream metadata and release notes.
2. Push the release commit to `main` and wait for Rust CI.
3. Push the matching version tag. The Release workflow verifies and publishes the archive and checksums, then calls the COPR workflow with the same tag.
4. Check the COPR build linked from the workflow summary for Fedora 44 and 45 success and wait for the repository metadata to expose the new RPM.

To resume or retry the COPR stage, run the [COPR workflow](../../.github/workflows/copr.yml) manually with the published tag. The workflow watches an existing build for that RPM version instead of submitting it again. It requires the repository secret `COPR_CONFIG` and fails if the secret is missing. Keep COPR credentials in the repository secret store.

## Comparison with other installation methods

- GitHub release archive: prebuilt binary for glibc 2.39+.
- COPR package: built against the target Fedora release's libraries, managed by
  `dnf`, with dependencies and license data declared.
