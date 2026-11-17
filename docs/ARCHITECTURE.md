# Plyra Terminal architecture

The active desktop target is macOS Apple Silicon. The user deferred macOS Intel and prioritized completing the macOS application before expanding platform validation. The existing Godot project and its Mobile integration remain available alongside the Cargo workspace.

## Components

| Component | Responsibility |
| --- | --- |
| `apps/desktop` | winit event loop, content-only workspace, tabs, split tree, input routing, on-demand settings and system panels |
| `terminal-core` | Shared screen, bounded history, selections, validated screen protocol, OSC 8 metadata |
| `session-manager` | Owned PTYs, structured executable arguments, input queues, resize, mouse and focus protocols, Shell lifecycle |
| `terminal-render` | wgpu/Metal presentation, glyphon/cosmic-text glyph layout, clipped pane text, selections, IME preedit, WGSL material |
| `system-api` | Native snapshots, bounded log and connection queries, filesystem events, managed recovery, sandboxed plugins, broker client |
| `privilege-policy` | Process and service capabilities and target identity checks |
| `privilege-broker` | Separate authenticated IPC process, expiring confirmations, target revalidation, revocation, audit records |
| `apps/android` | Android entry point and platform smoke implementation, with separate lifecycle validation |

## Terminal data flow

The macOS application starts the bundled C helper. The helper owns a real PTY and the extracted libvterm 0.3.3 parser. It sends bounded JSON screen and scrollback events to the Rust reader thread. The reader validates the representation, updates the shared screen and coalesces window wakeups. PTY backpressure bounds queued output while preserving output before history retention limits apply.

Local OSC 8 IDs live on native libvterm cells. Cursor moves, alternate buffers, scrollback and reflow preserve their association with glyphs. The small vendor patch and original source hashes are recorded in `tools/mobile/terminal/vendor/PROVENANCE.json`.

The portable backend uses portable-pty and vt100. It shares the screen model and lifecycle API with the native helper. Each backend requires its own compatibility evidence.

The default macOS zsh session uses generated startup wrappers inside the application's support directory. The wrappers source the user's startup files and emit program-name-only OSC 133 observations. These events provide a command timeline with reader-observed timestamps. They retain no complete command arguments and have the trust level of PTY output.

## Workspace and rendering

Each pane owns one Session and its current selection, history offset and preedit state. Tabs hold split trees with resizable dividers. The default window hides native decorations and persistent workspace controls. Shortcuts open commands, search, settings and system panels on demand. Settings store layout and appearance atomically. Restoring a saved layout starts fresh Shell processes.

The GPU surface composes terminal backgrounds, sharp foreground glyphs, preedit and egui controls in one frame. Native AppKit material samples the desktop behind the window. Off mode and unavailable transparency use an opaque GPU background. The default appearance follows the user's iTerm display reference with neutral black tint, approximately 49.7% transparency, gray foreground and cursor, and the installed JetBrains Mono font at 15 points. Balanced and Full preserve sharp glyphs. Full interaction highlights follow pointer events and honor application and OS reduced-motion preferences. Public AppKit material controls provide semantic blur. The iTerm blur radius remains a visual reference.

The normal event loop sleeps between actual events. Terminal output, input, changes in layout and explicit UI animations request frames. A terminal rectangle receives text only while terminal focus is active. Dialog text fields and system controls use the UI input path. Composition cancellation and late-commit filtering prevent text from crossing pane focus changes.

## System operations

Read-only queries run on worker threads and return structured results through the event-loop proxy. Snapshots identify their API source, sample time and collection coverage. Process and interface changes are sampled deltas. Filesystem events cover the managed directory through FSEvents. Connection queries use a bounded lsof invocation.

Process stops and service restarts cross authenticated IPC to the broker. A prepared operation binds a capability to a concrete target and returns a single-use confirmation ticket. The broker revalidates the target at execution time. Administrator authorization uses the macOS native prompt and a separate signed broker. Revocation ends that authorization session.

Recovery journals cover tracked file edits inside the managed directory. Hash conflicts preserve later external changes. Plugins execute in an explicit user-mode Seatbelt sandbox with bounded runtime and optional managed-write or network capabilities. Detailed boundaries are documented in `SECURITY.md`.

## Build and evidence

Dependencies are pinned in `Cargo.lock`. `tools/glass_licenses.py` collects registry declarations and bundled notice texts. `tools/build_macos_app.py` builds, bundles and locally signs the Apple Silicon application and its two helpers. Current packages use ad-hoc signing. Native functional evidence, cross-build results and untested platform requirements are recorded separately in the Gate reports.
