# Beyrix roadmap

This is a proposed direction, not a schedule or a promise of implementation.

## Current baseline — Linux/Wayland

- First public alpha (`v0.1.0-alpha.1`) with Linux x86_64 binaries, checksums and a tag-driven release workflow.
- Native Wayland window, PTY-backed Bash, OpenGL ES and software rendering, scrollback, selection, clipboard, SGR mouse reporting, layouts and a built-in editor.
- Stabilize terminal input, output, selection, resize and scrollback behavior based on early-user reports.
- Improve compatibility with real-world terminal applications and document known limitations precisely.

## Rendering

- Profile the current CPU/`wl_shm` and OpenGL paths before changing them.
- Detect graphics capabilities and evaluate Vulkan as a future backend.
- Keep a tested software fallback where practical.
- Validate correctness, driver compatibility, frame pacing and performance before claiming acceleration.

## Cross-platform

- Separate terminal core, PTY/process handling and display backends.
- Design and test a Windows-compatible PTY/rendering approach.
- Add native Windows builds **only when there is a working implementation**.

## Ecosystem and community

- Publish follow-up alpha and beta binaries after smoke tests.
- Improve documentation, issue templates, contributor onboarding and release notes.
- Add distro packages and establish reproducible-build checks.
- Prioritize clearly scoped beginner-friendly issues.

To propose a change, create a GitHub issue describing the user problem, constraints and success criteria. Maintainers decide priorities in public.