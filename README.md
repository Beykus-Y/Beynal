# Beyrix

**Beyrix** is an experimental open-source terminal emulator written in Rust, currently targeting **Linux/Wayland**.

> Early development (0.1.0). Please expect bugs and breaking changes. Vulkan rendering and native Windows support are goals, **not implemented features**.

## Current capabilities

- Native Wayland window with OpenGL ES and CPU/`wl_shm` renderers; the backend is chosen automatically and falls back to software.
- PTY-backed interactive Bash sessions.
- Scrollback, selection, clipboard integration and basic terminal escape sequence handling.
- Three cursor layouts: `as-is`, `bottom` and `bottom-input`.
- Built-in text editor for `beyrix edit FILE`.
- Built-in `minimal`, `developer`, and `signature` prompt themes.
- Customizable fonts, colors, cursor and scrolling.
- GPU and graphics API diagnostics via `beyrix gpu`.
- Ctrl+Arrow and Ctrl+Backspace/Delete word editing in the bundled Bash prompt integration.

Beyrix does **not** currently claim full terminal compatibility, a Vulkan renderer, or native Windows support. Please report incompatibilities with specific programs.

## Build and run (Linux)

Prerequisites: a Linux system with a working Wayland session, recent stable Rust/Cargo (edition 2024), Bash and the native development libraries required by the dependencies (including Wayland). Fedora users may need the corresponding `-devel` packages.

```sh
git clone https://github.com/Beykus-Y/Beyrix.git
cd Beyrix
cargo build --release --locked
./target/release/beyrix --help
./target/release/beyrix
```

Prebuilt packages and signed releases are not yet available.

Run `beyrix theme list` to inspect themes, or `beyrix theme set signature` to select one. Configuration documentation: [CONFIGURATION.md](CONFIGURATION.md).

### Upgrading from Beynal

The application, Cargo package and executable are now **Beyrix** (`beyrix`). Existing `~/.config/beynal/config.toml` is copied once to `~/.config/beyrix/config.toml` if the new file does not exist. No existing file is deleted or overwritten, and existing Beyrix settings always take priority. If the copy fails, Beyrix keeps reading the old file without modifying it. See [CONFIGURATION.md](CONFIGURATION.md) for details.

If you already installed a `beynal` binary system-wide, update any shell aliases or launchers to `beyrix` when installing the new version.

## Contributing

Contributions, feedback, bug reports, tests on other Linux distributions and documentation improvements are welcome.

- Read [CONTRIBUTING.md](CONTRIBUTING.md) and the [architecture overview](docs/ARCHITECTURE.md) before submitting a change.
- File bugs and feature suggestions through [Issues](https://github.com/Beykus-Y/Beyrix/issues).
- Smaller changes with a reproducer and tests are especially helpful.
- Review the [roadmap](ROADMAP.md) for priorities.
- Please read [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for community expectations.

To help without writing Rust: test terminal programs, report escape-sequence incompatibilities, suggest UX improvements, proofread English documentation, or help create distro packages.

## License

Beyrix is licensed under the **Mozilla Public License 2.0 (MPL-2.0)**. See [LICENSE](LICENSE). Modified MPL-covered files must remain available in source form when distributed under the license terms; using the project privately does not oblige you to publish changes. No license requires contributors to send pull requests upstream.

This project is not affiliated with Ghostty, Alacritty, or other terminal emulators.
