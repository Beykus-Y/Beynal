# Beyrix Terminal Compatibility Matrix

This document tracks Beyrix's terminal emulator compatibility against primary specifications (XTerm Control Sequences, ECMA-48 / VT100–VT520, Kitty Keyboard & Graphics Protocols, OSC 8/52/133, Unicode UAX #11 / #29) and compares behavior with modern terminals (Kitty, Ghostty, WezTerm, Alacritty, Foot, xterm).

Status values:
- **Supported**: Implemented, wired end-to-end, and verified by automated/real-app tests.
- **Partial**: Implemented in part or with known limitations noted explicitly.
- **Unsupported**: Not implemented (and honestly reported as unrecognized/unsupported in queries).
- **Experimental**: Implemented behind opt-in or evolving behavior.
- **Not applicable**: Intentionally out of scope for this platform/stage.

---

## 1. Environment, Identification & Terminfo

| Feature | Specification | Baseline (`a67f1ca`) | Problematic Apps (Baseline) | Implementation | Tests | Status (Target / Current) |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| `TERM` value | terminfo / ncurses | `TERM=ansi` hardcoded | `nano`, `btop`, `tmux`, `nvim`, `mc`, `htop` | `TERM=xterm-256color` default; bundled `beyrix` terminfo in RPM | `infocmp`, `tput`, PTY tests | Supported |
| `COLORTERM` | Truecolor convention | Unset (`unsetenv`) | `btop`, `bat`, `delta`, `nvim`, `codex` | `COLORTERM=truecolor` | PTY env test | Supported |
| `TERM_PROGRAM` / version | Terminal identification | Missing | Shell scripts, prompt frameworks | `TERM_PROGRAM=Beyrix`, `TERM_PROGRAM_VERSION` | PTY env test | Supported |
| `DA1` (`CSI c`) | XTerm / VT | `\x1b[?1;2c` (VT100) | Apps probing VT220+ / ANSI color | Honest VT220+ attributes (`\x1b[?62;22c`) | Conformance test | Supported |
| `DA2` (`CSI > c`) | XTerm / VT | Ignored | `tmux`, `vim`, `nvim` | `\x1b[>1;100;0c` | Conformance test | Supported |
| `XTVERSION` (`CSI > q`) | XTerm | Ignored | Modern TUIs (`nvim`, `yazi`, `fzf`) | `\x1bP>|Beyrix <ver>\x1b\\` | Conformance test | Supported |
| `DECRQM` (`CSI ? Ps $ p`) | VT520 / XTerm | Only 7 modes | `nvim`, `tmux`, `fish`, `btop` | Accurate report for all supported modes (0 for unsupported) | Conformance test | Supported |
| `DECRQSS` (`DCS $ q ... ST`) | VT420 / XTerm | Empty DCS hooks | `tmux`, `vim` | SGR (`m`), DECSTBM (`r`), DECSCUSR (` q`) | Conformance test | Supported |
| `XTGETTCAP` (`DCS + q ... ST`) | XTerm | Empty DCS hooks | `tmux`, `nvim`, `kitty` clients | `TN`, `RGB`, `Tc`, `colors`, `Su`, `Sync`, `setrgbf`, `setrgbb` | Conformance test | Supported |

---

## 2. Core VT / ECMA-48 Sequences

| Feature | Specification | Baseline (`a67f1ca`) | Problematic Apps (Baseline) | Implementation | Tests | Status |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| Cursor movement (`CUU/CUD/CUF/CUB/CUP/HVP/CHA/VPA/CNL/CPL`) | ECMA-48 | Supported | — | `src/terminal.rs` | Unit + conformance | Supported |
| Cursor save/restore (`DECSC`/`DECRC`, `CSI s`/`u`) | VT100 / SCO | Position only (no attrs/origin/charset) | `vim`, `nano`, `tmux` | Full saved state (pos, SGR, origin, charset, pending wrap) | Conformance test | Supported |
| Insert/Delete Char (`ICH`/`DCH`) & Erase Char (`ECH`) | ECMA-48 | Supported | — | `src/terminal.rs` | Unit + conformance | Supported |
| Insert/Delete Line (`IL`/`DL`) | ECMA-48 | Supported | — | `src/terminal.rs` | Unit + conformance | Supported |
| Erase Display (`ED` 0/1/2/3) & Line (`EL` 0/1/2) | ECMA-48 | Supported | — | `src/terminal.rs` | Unit + conformance | Supported |
| Insert/Replace Mode (`IRM`, `CSI 4 h/l`) | ECMA-48 | Unsupported | `bash`/readline, `less`, `nano` | Shift line right on `put_char` when IRM enabled | Conformance test | Supported |
| Automatic Newline Mode (`LNM`, `CSI 20 h/l`) | ECMA-48 | Unsupported | Legacy apps | LF implies CR when enabled | Conformance test | Supported |
| Scroll Up/Down (`SU`/`SD`) & Scroll Regions (`DECSTBM`) | VT100 | Supported (no DECOM interaction) | `tmux`, `less`, `vim` | Full `DECSTBM` + `DECOM` + `DECLRMM` | Conformance test | Supported |
| Horizontal Margins (`DECLRMM` `?69`, `DECSLRM`) | VT420 | Unsupported | `tmux` panes, `vttest` | Left/right margin tracking + `CSI s` disambiguation | Conformance test | Supported |
| Tab Stops (`HTS` `ESC H`, `TBC` `CSI g`, `CHT` `CSI I`, `CBT` `CSI Z`) | VT100 / ECMA-48 | Fixed 8-col tabs only | `nano`, `vttest`, `less` | Dynamic per-column tab stop table | Conformance test | Supported |
| Index (`IND` `ESC D`), Reverse Index (`RI` `ESC M`), Next Line (`NEL` `ESC E`) | VT100 | `IND` and `RI` only; no `NEL` | `ncurses`, `less` | Add `NEL` (`ESC E`) | Conformance test | Supported |
| Repeat Character (`REP`, `CSI b`) | ECMA-48 | Unsupported | `btop`, `tmux`, `ncurses` (runs of box-drawing chars!) | Track last printed char and repeat `N` times | Conformance test | Supported |
| DEC Special Graphics (`ESC ( 0`, `ESC ( B`, `G0`/`G1`, `SO`/`SI`) | VT100 | Unsupported | `mc`, `htop`, `dialog`, `ncurses` borders | Full VT100 line-drawing table + G0/G1 designation | Conformance test | Supported |
| Terminal Reset (`RIS` `ESC c`) & Soft Reset (`DECSTR` `CSI ! p`) | VT100 / VT220 | Unsupported | `reset`, `tput reset`, `tmux` | Full & soft reset restoring modes, margins, tabs, SGR | Conformance test | Supported |
| Window / Grid Size Queries (`CSI 14 t`, `16 t`, `18 t`) | XTerm | Unsupported | `chafa`, `yazi`, `fastfetch`, `nvim` | Report actual pixel & cell grid dimensions | Conformance test | Supported |

---

## 3. DEC Private Modes

| Mode | Name | Baseline (`a67f1ca`) | Problematic Apps (Baseline) | Implementation | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `?1` | `DECCKM` Application Cursor Keys | Unsupported | `less`, `nano`, `vim`, `btop`, `tmux` | Track mode & route into keyboard encoder | Supported |
| `?6` | `DECOM` Origin Mode | Unsupported | `vttest`, `ncurses`, `tmux` | Relative addressing & clamping inside scroll margins | Supported |
| `?7` | `DECAWM` Auto Wrap Mode | Always on | `vttest`, `ncurses` bottom-right cell | Toggleable wrap; no-wrap clamps at right margin | Supported |
| `?9` | X10 Mouse Reporting | Unsupported | Legacy TUI | Press-only X10 reporting | Supported |
| `?25` | `DECTCEM` Cursor Visibility | Supported | — | `cursor_visible` | Supported |
| `?45` | Reverse Wraparound | Unsupported | `bash`/readline long lines | BS at col 0 wraps to prev wrapped line | Supported |
| `?47` / `?1047` / `?1048` / `?1049` | Alternate Screen & Save Cursor | All collapsed to 1049 | `vim`, `less`, `tmux`, `screen` | Distinct semantics per mode (`47`, `1047`, `1048`, `1049`) | Supported |
| `?67` | `DECBKM` Backarrow Key Mode | Unsupported | `emacs`, `vttest` | Backspace sends `0x08` when set, `0x7f` when reset | Supported |
| `?69` | `DECLRMM` Left/Right Margin Mode | Unsupported | `tmux`, `vttest` | Enables `DECSLRM` (`CSI Pl ; Pr s`) | Supported |
| `?1000`/`1002`/`1003` | Mouse Tracking (Click/Drag/Motion) | Supported | — | `src/mouse.rs` + `src/terminal.rs` | Supported |
| `?1004` | Focus Reporting | Unsupported | `nvim`, `tmux`, `btop`, `lazygit` | Emit `\x1b[I` / `\x1b[O` on Wayland keyboard focus | Supported |
| `?1005`/`1006`/`1015`/`1016` | Mouse Encodings (UTF-8, SGR, URXVT, SGR-Pixels) | Only 1006 | `htop`, `mc`, `nvim`, `yazi` | Full encoding enum in `src/mouse.rs` | Supported |
| `?1007` | Alternate Scroll Mode | Unsupported | `less`, `man`, `git log` | Wheel sends Up/Down arrows on alt screen when mouse off | Supported |
| `?2004` | Bracketed Paste | Supported | — | `bracketed_paste` + breakout sanitization | Supported |
| `?2026` | Synchronized Output | Unsupported | `btop`, `nvim`, `yazi`, `codex`, `claude` | Defer redraw during sync frame with 200ms safety timeout | Supported |
| `?2031` | Color Scheme Preference Updates | Unsupported | `nvim`, modern TUIs | Query `CSI ? 996 n` + dark/light mode response | Supported |

---

## 4. Keyboard & Input Protocols

| Feature | Specification | Baseline (`a67f1ca`) | Implementation | Status |
| :--- | :--- | :--- | :--- | :--- |
| Smart Ctrl+C / Ctrl+V | Beyrix UX requirement | Supported | Preserved (intentional selection, TUI-aware Ctrl+V, guaranteed fallbacks) | Supported |
| Application Cursor (`DECCKM`) & Keypad (`DECKPAM`/`DECKPNM`) | VT100 / XTerm | Unsupported | `TerminalInputModes` passed to `Keyboard::key_with_modes` | Supported |
| XTerm `modifyOtherKeys` (`CSI > 4 ; m`) | XTerm | Unsupported | Mode 0/1/2 encoding (`CSI 27 ; mod ; code ~`) | Supported |
| Kitty Keyboard Protocol (`CSI > / < / ? / = u`) | Kitty KKP spec | Unsupported | Per-screen stack (main/alt), flags `1|2|8|16` (bit 4 alternate keys honestly masked off & not advertised), press/repeat/release | Partial |
| Wayland Autorepeat | `wl_keyboard::repeat_info` | Supported | Preserved + KKP repeat event type (`:2`) | Supported |
| Non-Latin Ctrl Chords (RU/DE/etc.) | Beyrix UX | Supported | Evdev fallback preserved | Supported |
| AltGr / Compose / Dead keys / IME | XKB / Wayland | Partial (XKB state only) | XKB dead-key/AltGr preservation; Wayland text-input IME noted | Partial |

---

## 5. Text, SGR & Unicode

| Feature | Specification | Baseline (`a67f1ca`) | Implementation | Status |
| :--- | :--- | :--- | :--- | :--- |
| 16 ANSI, 256-color, 24-bit Truecolor (`;` and `:`) | ISO 8613-6 / XTerm | Supported | Preserved + SGR 58/59 underline color | Supported |
| Bold, Dim, Italic, Reverse, Conceal, Strikethrough, Blink, Overline | ECMA-48 / XTerm | Only Reverse (`7`/`27`) | `CellAttrs` bitflags + Bold/Italic font loading + GL & Software rendering | Supported |
| Underline styles (single, double, curly, dotted, dashed) + color | Kitty / VTE / XTerm | Unsupported | `UnderlineStyle` + `underline_color` in `Cell`, rendered in GL & Software | Supported |
| Combining marks & Wide chars (CJK, Emoji) | UAX #11 / #29 | Basic `unicode-width` | Variation selectors (VS15/VS16), ZWJ cluster attachment, box-drawing alignment | Supported |
| Font zoom (`Ctrl+`/`Ctrl-`/`Ctrl+0`) | Modern terminal UX | Unsupported | Live font size scaling + PTY resize | Supported |

---

## 6. OSC & Modern Terminal Extensions

| Feature | Specification | Baseline (`a67f1ca`) | Implementation | Status |
| :--- | :--- | :--- | :--- | :--- |
| `OSC 0 / 2` Window Title | XTerm | Unsupported | Updates `xdg_toplevel::set_title` | Supported |
| `OSC 4 / 104` Dynamic Palette Colors | XTerm | Unsupported | Query/set/reset 256-color palette | Supported |
| `OSC 7` Current Working Directory | OSC 7 | Unsupported | Parsed `file://` URI, exposed to X-Ray & shell state | Supported |
| `OSC 8` Hyperlinks | OSC 8 | Unsupported | Per-cell hyperlink ID, hover underline, Ctrl+Click safe open (`http/https/file`) | Supported |
| `OSC 9;4` Progress Indicator | ConEmu / Windows Terminal | Unsupported | Parsed state/progress + subtle top bar indicator | Supported |
| `OSC 10/11/12` & `110/111/112` Dynamic Colors | XTerm | Query 10/11 only | Query/set/reset FG, BG, Cursor | Supported |
| `OSC 52` Clipboard Integration | XTerm | Unsupported | Base64 write wired to Wayland clipboard (1 MiB cap); read disabled by default for security | Supported |
| `OSC 133` Shell Integration | FinalTerm / Contour | Partial (A/C anchors only) | Full A/B/C/D lifecycle + exit code + X-Ray integration | Supported |
| Kitty Graphics Protocol (`APC G`) | Kitty Graphics | Unsupported | Chunked RGBA/RGB/PNG transmit, placement, delete, scroll/alt-screen, memory cap (file/shm disabled safely) | Supported |
| SIXEL Graphics (`DCS ... q`) | VT340 | Unsupported | SIXEL decoder to inline `TerminalImage` with palette & memory limits | Supported |
| Scrollback Search & Command Jump | Modern UX | Unsupported | `Ctrl+Shift+F` incremental search + `Ctrl+Shift+Up/Down` OSC 133 jump | Supported |
