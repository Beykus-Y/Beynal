# Beyrix configuration (English)

Beyrix reads `$XDG_CONFIG_HOME/beyrix/config.toml` at startup. If `XDG_CONFIG_HOME` is not set, it uses `~/.config/beyrix/config.toml`. Every setting is optional.

The original Russian reference is kept in [CONFIGURATION.md](CONFIGURATION.md). Both documents describe the same implementation; if they disagree, the code and the Russian document are the current source of truth.

## Migrating from Beynal

- The application, command, Cargo package, help text, window title and Wayland `app_id` (`beyrix`) use the new name.
- On first launch, if no Beyrix configuration exists, the old `beynal/config.toml` is copied to `beyrix/config.toml`. An existing Beyrix file is never overwritten, the old file is never deleted, and repeated launches do nothing. If copying fails, Beyrix reports the error and keeps reading the old file without modifying it.
- To migrate manually, copy `beynal/config.toml` to `beyrix/config.toml` while the application is closed. If a Beyrix file already exists, merge the desired lines yourself; automatic migration does not touch it.
- New processes export only `BEYRIX_CONTROL_SOCKET`, `BEYRIX_PROMPT_THEME` and `BEYRIX_COMMAND_DIR`. The CLI may read the matching `BEYNAL_*` variables once during an upgrade when the `BEYRIX_*` variable is absent. This is a temporary compatibility measure, not a permanent interface.
- The command `beynal` is not installed. Inside a new Beyrix window it is temporarily available as a transition symlink for old sessions.

## Example

```toml
theme = "signature"
signature_compact = false
font_family = "monospace"
font_size = 16.0
padding = 12
cursor_shape = "beam"
cursor_blink = false
scrollback_limit = 10000
scroll_speed = 3
layout = "as-is"
renderer = "auto"

# Optional color overrides
# background = "#140e24"
# foreground = "#e7e6fa"
# cursor = "#5debf7"
# selection = "#584099"
# selection_foreground = "#faf6ff"
# ansi = ["#000000", "..."] # exactly 16 colors: normal 0-7 and bright 8-15
```

`beyrix theme set NAME`, `beyrix layout set NAME` and `beyrix renderer set NAME` update the file while preserving comments and unknown settings.

## Themes and prompts

- `minimal` — default. A compact prompt with the current directory and a short Git branch indicator.
- `developer` — a single-line `user@host`, path, Git branch and path context. An asterisk marks a dirty working tree, including untracked files.
- `signature` — a violet `◆` marker, cyan path, a separate Git block and an exit-status indicator on errors. By default input starts on the next line; `signature_compact = true` puts `❯` on the first line.

`beyrix theme` prints the active theme and `beyrix theme list` lists the built-in themes. `beyrix theme set NAME` saves the choice, preserves comments and other TOML settings, and asks the running window to update. Bash applies the new prompt on its next prompt. A graphical launch reads the normal `~/.bashrc` and adds its prompt integration through a temporary startup file. `beyrix --pty` uses the normal Bash prompt.

Inside Beyrix the `beyrix` command prints help. From another terminal it opens a window. Help is always available through `beyrix --help`; redirected stdout and `NO_COLOR` produce plain text.

## Fonts, colors and cursor

- `font_size` is measured in pixels and accepts 6–96. `padding` accepts 0–256 pixels. Fontconfig provides fallback fonts. In a very small window Beyrix reduces padding so at least one text cell fits.
- Colors use `#RRGGBB`. Overrides take precedence over the theme. Changing the theme updates default colors and the first 16 ANSI colors, including history; application-provided truecolor values are preserved. Indexes 16–255 use the standard extended palette.
- `cursor_shape` accepts `block`, `beam` and `underline`. Applications may temporarily change the shape through DECSCUSR or hide the cursor. `cursor_blink = false` disables blinking, including application requests. An unfocused visible cursor is drawn as an outline.
- `selection` and `selection_foreground` set selection background and text independently.

## Cursor layout

`layout` controls where the terminal grid starts and is saved in `config.toml`:

- `as-is` — default. Bash starts in the upper-left corner like a conventional terminal.
- `bottom` — the initial prompt is drawn on the last row. After that the terminal scrolls normally: new lines appear below and old lines move up. The real terminal cursor is placed on the last row, so cursor-position queries (`CSI 6n`) and application addressing remain correct. `clear`/Ctrl+L return the cursor to the top like a normal terminal.
- `bottom-input` — the cursor row (prompt and input) is pinned to the last row. Other rows are displayed above it in chronological order; older rows move up. This is a presentation offset only: PTY coordinates, the grid, history and the alternate screen are not reordered. Full-screen programs (nano, vim, etc.) disable the offset automatically, and scrolling history shows a normal full screen. The offset returns after the user goes back to live output or presses a key.
- `beyrix layout` prints the active mode, `beyrix layout list` lists modes, and `beyrix layout set NAME` saves a choice for new windows. Changing the configuration or CLI value does not modify an already running window.

`bottom-input` limitation: an application that writes into the current cursor row while the user is typing (for example asynchronous output while Readline waits) still follows standard terminal semantics and may overwrite the input line. Beyrix deliberately does not fake coordinates or split the PTY to hide this.

## Graphical backend

`renderer` selects the drawing path:

- `auto` — default. Beyrix creates a real EGL/OpenGL ES context on the current Wayland display and uses OpenGL only for a hardware renderer (AMD/Intel/NVIDIA and similar). `llvmpipe`, `softpipe`, `swrast` and comparable software drivers count as software, so the CPU renderer is kept.
- `opengl` — force OpenGL. If context creation fails, Beyrix writes an error to stderr and falls back to software; it never leaves a black window.
- `software` — the CPU renderer through `wl_shm`.
- `vulkan` — reserved. Vulkan devices are discovered and shown by diagnostics, but Vulkan rendering is not implemented in this version, so the backend falls back to software.

The setting applies to new windows. `beyrix renderer` prints the choice, `beyrix renderer list` lists options, and `beyrix renderer set NAME` saves it.

`beyrix gpu` prints detected DRM devices, the actually created EGL/OpenGL context and renderer string, physical Vulkan devices from the loader, ICD manifests, the selected backend and the reason for the decision. PCI presence alone is not treated as proof of acceleration: OpenGL is verified by creating a context, and software implementations are recognized by their renderer string.

The OpenGL backend draws backgrounds, selection, cursor and glyphs on the GPU. Glyphs are cached in an atlas and uploaded on first use; rectangles and glyphs are batched into vertex buffers. Changed rows are redrawn with scissor rectangles, and `EGL_KHR_swap_buffers_with_damage` reports damaged regions to the compositor. Resize, theme, scale and editor switches trigger a full redraw. On a driver error or context loss the backend is disabled once and the application continues with software without a retry loop.

Fractional scaling uses `wp_fractional_scale_v1` plus `wp_viewporter` when available; otherwise the integer `preferred_buffer_scale` is used. With the software backend, fractional scaling is stretched by the compositor (slightly less sharp) so layout and PTY coordinates stay correct.

OpenGL initialization costs driver memory (about 45–50 MiB PSS on the AMD Lucienne test machine) and slightly increases startup time for EGL setup and shader compilation. Use `renderer = "software"` when minimal memory use matters more. On large windows and during heavy redraws the GPU path removes rasterization from the CPU, but input/output speed may still be limited by parsing and grid scrolling.

## History and selection

- `scrollback_limit` sets the maximum number of stored lines, 0–100,000. `0` disables history. In addition, history payload is capped at 64 MiB. The oldest lines are removed when either limit is reached.
- `scroll_speed` sets lines per wheel step, 1–100. Fractional offsets accumulate. Shift+PageUp/PageDown scroll history; Shift+Home/End jump to the beginning or back to live output.
- History preserves the original line width. A narrow window shows the visible part, and hidden columns become available again after widening. The live grid is clipped when the window narrows. Resize may clear a selection.
- Double-click selects a word and triple-click selects a logical line, including soft-wrapped content. Holding the pointer at the top or bottom text edge scrolls while dragging. If history evicts the selected row, the selection is cleared.
- Ctrl+C copies a selection. Without a selection, Ctrl+C sends an interrupt to the application. Ctrl+V pastes from the Wayland clipboard using bracketed paste when the application enables it.

## Command-line editing

In interactive Bash inside a graphical Beyrix window, Ctrl+Left and Ctrl+Right move to the nearest word boundary. Ctrl+Backspace and Ctrl+Delete delete to the same boundary. A word is any run of characters other than space or tab; adjacent spaces/tabs form a separate block. For example, at the end of `hello   world`, three Ctrl+Backspace presses remove `world`, then the three spaces, then `hello`. In `/tmp/folder`, slashes are part of the word.

The bindings work in Bash Emacs and Vi modes in a graphical Beyrix window. In a UTF-8 locale boundaries are computed by character. Ordinary arrows, Backspace, Delete and Ctrl+W use Readline's built-in actions.

Applications receive Ctrl+Left/Right as `ESC [ 1 ; 5 D` / `ESC [ 1 ; 5 C`, Ctrl+Backspace as `ESC [ 127 ; 5 u`, and Ctrl+Delete as `ESC [ 3 ; 5 ~`. Support for modified keys depends on the program. Older decoders may not recognize the CSI-u form of Ctrl+Backspace. Wayland keyboard autorepeat is not implemented in the graphical window yet.

## Built-in editor

`beyrix edit FILE` opens the built-in editor. If run in a shell inside Beyrix (where `BEYRIX_CONTROL_SOCKET` is set), the request goes to the current window; otherwise a new window opens. The file may not exist yet; it is created on the first save. Only UTF-8 is supported; BOM and CRLF line endings are preserved.

- Typing, Enter, Tab, Backspace, Delete; arrows, Home/End, Ctrl+Left/Right by word, Ctrl+Home/End by document.
- Shift with arrows/Home/End extends the selection; the mouse selects by dragging, double-click selects a word (Unicode and combining marks aware).
- Ctrl+C/X/V use the Wayland clipboard.
- Ctrl+S saves, Ctrl+Z undoes, Ctrl+Y or Ctrl+Shift+Z redoes, Ctrl+A selects all.
- The wheel scrolls the file; long lines scroll horizontally.
- Esc returns to the terminal without disturbing the PTY, history or running programs.
- The status line shows the file name, modified marker `[+]`, line and column, encoding and shortcuts.

Saving is atomic: data is written to a temporary file in the destination directory, synced, and renamed over the original. On write failure the original file stays untouched. Existing file permissions are preserved. If the file changed on disk or is read-only, the first Ctrl+S shows a warning and a second Ctrl+S overwrites it.

On Esc or window close with unsaved changes, the editor asks for confirmation: `Y` saves and exits, `N` exits without saving, `C` or Esc cancels. If a `Y` save fails, the editor stays open and shows the error. With no changes Esc returns immediately.

Version 0.1 limits: one open file, no tabs, no syntax highlighting and no LSP. If a window already has an unsaved editor file, a `beyrix edit` request is rejected, but the CLI does not receive an error reply and still reports that the request was sent.

## Diagnostics

- `BEYRIX_GL_DUMP=1` forces a full redraw after a short delay, useful for capturing a frame.
- `BEYRIX_DEBUG_INPUT=1` prints input and clipboard diagnostics to stderr.
- `beyrix gpu` prints the graphics report described above.