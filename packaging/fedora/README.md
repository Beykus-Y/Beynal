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
- A desktop entry and AppStream metadata so Beyrix appears in application menus.
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
./packaging/fedora/make-sources.sh v0.1.0-alpha.1
```

The script downloads the tagged source archive, runs `cargo vendor --locked`,
creates a reproducible vendor archive and builds the SRPM in `dist/fedora/`.
It uses the tag's commit timestamp as `SOURCE_DATE_EPOCH`.

Build the binary RPM from the SRPM:

```sh
sudo dnf builddep dist/fedora/beyrix-0.1.0~alpha.1-1.fc44.src.rpm
rpmbuild --rebuild dist/fedora/beyrix-0.1.0~alpha.1-1.fc44.src.rpm
```

## Publish a new release

1. Bump `version` in `Cargo.toml`, run `cargo build --locked`, and commit the
   resulting `Cargo.lock`.
2. Tag the commit (`git tag vX.Y.Z`) and push the tag.
3. Update `%global upver`, `Version:` and the `%changelog` entry in
   `beyrix.spec`.
4. Generate and submit the SRPM:

   ```sh
   ./packaging/fedora/make-sources.sh vX.Y.Z --submit
   ```

   `copr-cli build` waits for the build by default. Use
   `copr-cli list-builds beykus-y/beyrix` or the web interface to check older
   builds.

The GitHub Actions workflow `.github/workflows/copr.yml` can do step 4
automatically when a GitHub release is published. It requires a repository
secret named `COPR_CONFIG` containing a complete `copr-cli` configuration
file; the workflow skips publication when the secret is not configured. Never
commit that file or the API token to the repository.

## Comparison with other installation methods

- GitHub release archive: prebuilt binary for glibc 2.39+.
- COPR package: built against the target Fedora release's libraries, managed by
  `dnf`, with dependencies and license data declared.