# Beyrix roadmap

This is a proposed direction, not a schedule or a promise of implementation.

## Current baseline — Linux/Wayland

- Stabilize terminal input, output, selection, resize and scrollback behavior.
- Improve compatibility with real-world terminal applications.
- Add reproducible builds, CI and practical installation instructions.
- Document known limitations and gather independent user feedback.

## Rendering

- Profile the current CPU/wl_shm path before changing it.
- Detect graphics capabilities and evaluate Vulkan and/or OpenGL backends.
- Keep a tested software fallback where practical.
- Validate correctness, driver compatibility, frame pacing and performance before claiming acceleration.

## Cross-platform

- Separate terminal core, PTY/process handling and display backends.
- Design and test a Windows-compatible PTY/rendering approach.
- Add native Windows builds **only when there is a working implementation**.

## Ecosystem and community

- Publish alpha binaries and distro packages after smoke tests.
- Improve documentation, issue templates, contributor onboarding and release notes.
- Prioritize clearly scoped beginner-friendly issues.

To propose a change, create a GitHub issue describing the user problem, constraints and success criteria. Maintainers decide priorities in public.
