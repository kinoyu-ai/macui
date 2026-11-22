# MacUI implementation plan

- Build a Rust Win32 dock with cached app icons, a translucent rounded surface and an event-driven message loop.
- Let the user launch and pin items, change appearance and quit directly from the dock. Store preferences per user.
- Give a waiting helper ownership of taskbar hiding and restoration, including when the UI is killed.
- Build a release executable, verify normal and forced exits, and measure both processes together during idle.
