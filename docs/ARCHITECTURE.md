# Beyrix architecture (current Linux/Wayland implementation)

Beyrix targets Linux/Wayland and includes a PTY-backed terminal and the BEdit editor in the same application binary. The Cargo and Fedora launcher forwards to this binary. Release archives use a relative symlink.

| Module | Current responsibility |
| --- | --- |
| `src/main.rs` | CLI entry point, `--help`, `theme` commands, PTY mode and graphical entry point |
| `src/wayland.rs` | Wayland connection, surface, events, shared-memory buffers, input and interaction with the terminal state |
| `src/terminal.rs` | Terminal grid, escape-sequence interpretation via `vte`, scrollback and screen state |
| `src/render.rs` | CPU text rendering and cell geometry, using `fontdue` |
| `src/gl.rs` | OpenGL ES 2.0 backend built on EGL and a Wayland surface |
| `src/gpu.rs` | DRM, EGL/OpenGL and Vulkan discovery plus backend selection |
| `src/editor.rs` | Document text, undo/redo, file metadata, selections and visible text layout |
| `src/project.rs` | Background directory index, incremental insertion of saved paths and path lookup |
| `src/file_metadata.rs` | Linux file ownership, mode, ACL and extended attribute preservation for atomic saves |
| `src/bedit.rs` | Workspace, tabs, panel focus, close transactions and editor view composition |
| `src/syntax.rs` | Language selection and bounded lexical coloring of visible lines |
| `src/keyboard.rs` | Keyboard mapping and key sequence translation using `xkbcommon` |
| `src/mouse.rs` | Mouse reporting modes and SGR/X10 pointer event encoding |
| `src/pty.rs` | PTY process creation, Bash launch and resize |
| `src/selection.rs` | Selection position and word/line selection behavior |
| `src/theme.rs` | Themes, TOML configuration and persisted theme changes |
| `src/control.rs` | Local Unix datagram control socket for theme updates and batched editor requests |
| `src/prompt.rs` and `src/prompt_*.sh` | Bash prompt integration and its tests |

## BEdit data flow

The command launcher sends absolute native paths through `BEYRIX_CONTROL_SOCKET` when available. A terminal window hosts BEdit on the same surface and continues servicing the existing PTY. Standalone windows skip shell creation and advertise the Wayland application ID `io.github.beykus_y.BEdit`. Explorer caches the expanded tree and lays out only the visible range on each frame. The project index and file loader run independently of the frame loop. File loading returns a batch of documents, which the workspace resolves against stable tab IDs.

The workspace owns close confirmation for each affected document. Discard decisions take effect when the close transaction completes. If the user cancels or a save fails, the workspace keeps all buffers. Document saves check the target path, file identity and change stamps, then stage content and metadata in the destination directory before atomic replacement. Existing files retain ownership, mode, access time and extended attributes exposed by Linux, including POSIX ACLs. Modification time reflects the save. Inode identity, hard-link relationships and filesystem-specific inode flags are outside the metadata copied by this operation. New files use a no-clobber save.

Both renderers consume the same view and logical grid, including tab and Explorer hit areas, line numbers, syntax ranges and selection. The current lexer runs on the visible line prefixes with a byte limit. It has no multiline state. Tree-sitter provides incremental parse trees and highlight queries, with a separate grammar and query set per language. Syntect provides Sublime grammars and cacheable parser state, with a choice of regex engines. The current lexer favors a small dependency footprint and bounded viewport work. [Tree-sitter](https://tree-sitter.github.io/tree-sitter/3-syntax-highlighting.html) · [Syntect](https://github.com/trishume/syntect#caching)

## LSP and terminal pane design

A future LSP client can use the workspace root and each document’s canonical path, language and stable tab identity. Before adding a client, introduce document revisions and explicit edit deltas. A background server process per root and language should consume open/change/save/close notifications and return completion, diagnostics, definition and rename results tagged by revision. Position conversion belongs at the protocol boundary, from character indices to the server’s negotiated encoding. Workspace edits should pass through the document history and conflict checks. Diagnostics and completion can extend the shared view without putting protocol state into either renderer.

For an integrated terminal pane, reuse the terminal grid and PTY with the project as the shell’s working directory. Add pane layout and focus routing before attaching the process to the editor. The current terminal overlay already keeps Bash and its screen alive while the editor has focus. LSP servers and a terminal pane are future work.

## Current boundaries and constraints

- The graphical backend is Wayland-specific; it is **not** a portable windowing abstraction.
- Rendering uses either OpenGL ES through EGL or the CPU/`wl_shm` path, with automatic fallback to software. Vulkan device discovery exists, but there is no Vulkan renderer.
- PTY handling currently uses Unix APIs and launches Bash. Windows ConPTY support does not exist.
- The terminal parser and grid have their own tests, but full xterm/VT compatibility has not been established.

## How to contribute safely

1. For terminal compatibility, isolate the reproducer and add a focused regression test under the relevant module.
2. For input or selection changes, consider modifiers, autorepeat, resize, clipboard and scrollback interactions.
3. For renderer changes, measure CPU, allocation behavior, frame timing and rendering correctness; preserve a working fallback.
4. For portability or a new renderer, open a design issue describing the intended boundary **before** changing multiple modules.
5. Use the CI commands in [CONTRIBUTING.md](../CONTRIBUTING.md). A passing unit suite does not replace a real Wayland smoke test.

See the [roadmap](../ROADMAP.md) for proposals and [configuration guide](../CONFIGURATION.md) for user-facing settings.
