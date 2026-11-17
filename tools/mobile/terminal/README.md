# Plyra local terminal adapter

This is a real local terminal emulator, not a command imitation or per-line
`execute()` wrapper. A small native helper owns `forkpty()`, starts the current
user's interactive shell, and passes its byte stream through upstream libvterm.
The Godot client renders libvterm's screen cells and sends keyboard/input bytes.
There is no network connection, privilege escalation, device operation or system
installation in this component.

## Library and build

The unmodified vendored library is Paul Evans's **libvterm 0.3.3**, retrieved from
the [author's release page](https://www.leonerd.org.uk/code/libvterm/) and its
[official release archive](https://www.leonerd.org.uk/code/libvterm/libvterm-0.3.3.tar.gz).
The archive SHA-256 is
`09156f43dd2128bd347cbeebe50d9a571d32c64e0cf18d211197946aff7226e0`.
`vendor/PROVENANCE.json` records every retained file and archive correspondence.
The MIT license is preserved in the vendor tree and as a packaged asset.
Only the upstream `include/`, `src/` and `LICENSE` are retained; no upstream files
were changed and no build-generated upstream files are needed.

The static Linux helper also includes the toolchain runtime. `LICENSE.musl`
preserves the local Zig musl sysroot's complete COPYRIGHT. `LICENSE.compiler-rt`
contains Zig's MIT license plus upstream LLVM compiler-rt's Apache/LLVM-exception
and legacy MIT/UIUC notices, with provenance in `vendor/runtime-licenses/`.
These are fixed packaged sidecars pinned by `manifest.runtime_licenses`; the
build recipe copies and verifies them. The license-refresh receipt records the original notice-only update; a later
explicit `bg_default` protocol addition has its own build and PTY receipts. No libc++/libunwind is linked by this
C-only recipe. Availability of an archive is not proof every member is linked.


On the existing macOS arm64 development host:

```sh
python3 tools/mobile/terminal/build.py --build-dir ../plyra-terminal-builds/legacy-pty/build-N
python3 tools/mobile/terminal/test_backend.py --binary assets/terminal/bin/plyra-terminal-macos-arm64 --output ../plyra-terminal-builds/legacy-pty/backend-test-N
```

Use a new directory for each receipt. The recipe uses the existing Apple C
compiler for Mach-O arm64 and the existing Zig compiler for **static musl Linux
AArch64**. Compiler caches and intermediate outputs stay on the external project
volume. It installs neither a compiler nor a VM. The Linux artifact's ELF machine,
class, endianness and absence of `PT_INTERP` are checked. A cross-build alone does
not establish Linux or physical-phone runtime acceptance.

`assets/terminal/manifest.json` pins the two small binary artifacts, source hashes,
protocol, limits and license. The client chooses the current platform, verifies
the resource bytes and materializes an owned executable cache. Unsupported
platforms report that the backend is unavailable.

## Protocol version 1

Arguments: `--cols N --rows N`; defaults 80 × 24. Columns are 10..240 and rows
4..100. stdin and stdout are nonblocking pipes. Every stdin command is one ASCII
line; terminal data is strict RFC 4648 Base64, up to 65,536 decoded bytes:

- `input BASE64`: write the exact bytes to the PTY.
- `paste BASE64`: bracket through libvterm's current bracketed-paste mode.
- `key NAME [MOD]`: use libvterm's mode-aware keyboard encoder. Names are `Up`,
  `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`, `Insert`, `Delete`,
  `Backspace`, `Tab`, `Enter`, `Escape`, `F1`..`F12`. Modifier bits: Shift 1,
  Alt 2, Ctrl 4. This handles application-cursor mode as well as normal CSI keys.
- `resize COLS ROWS`: resize both libvterm and the real PTY. `TIOCSWINSZ` delivers
  `SIGWINCH` to the foreground process group.
- `close`: close this terminal session. stdin EOF is equally authoritative.

stdout is UTF-8 JSONL. Strings are escaped; malformed/truncated title UTF-8 is
replaced, never passed through as invalid JSON. Events:

- `ready`: protocol, child PID, shell absolute path, columns and rows.
- `screen`: dimensions, cursor `{row,col,visible}`, alternate-screen flag, title,
  `full` and changed `lines`. Each line has `row` and `runs`, each run `text`,
  total cell `width`, resolved RGB `fg`/`bg`, `bold`, `italic`, `underline`,
  `inverse`, and `strike`. Optional `bg_default` is the original libvterm default-
  background flag captured before RGB conversion; explicit ANSI backgrounds with
  the same RGB remain distinct runs. Consumers missing the flag should stay
  opaque. Inverse text must never acquire default-background transparency. Combining marks share a cell; CJK wide glyphs occupy
  two. Only plain ASCII single-width cells are merged; non-ASCII, wide and
  combining graphemes are separate runs so custom drawing can position them
  exactly. A full snapshot is emitted in chunks of at most eight rows: only its first
  chunk has `full:true`; subsequent chunks update the same grid.
- `scrollback`: `lines` of runs, and `dropped` count if the bounded transit ring
  overflowed under backpressure. Old lines retain their original column width.
- `scrollback_clear`: application requested erasure of saved lines (CSI 3 J).
- `exit`: the shell exit code, or 128 + its terminating signal. Explicit close/
  parent EOF uses code 0 unless a shell exit had already been observed.
- `error`: a recoverable protocol error or backend failure description.

One JSONL event is at most 1 MiB, pending stdout at most 8 MiB, pending PTY input
at most 256 KiB, and pending scrollback at most 128 rows × 240 cells. PTY parsing
is bounded to 256 KiB per poll cycle and screen snapshots coalesce at about
16 ms. The client independently caps retained history. Invalid commands are
rejected; repeated output-queue exhaustion closes the owned session safely.

The shell is `$SHELL` when it is an executable absolute path, otherwise the passwd
shell or `/bin/sh`; it is executed directly with `-i`, with `TERM=xterm-256color`
and `COLORTERM=truecolor`. Startup cwd is `$HOME` (passwd HOME fallback). Existing
interactive-shell startup files retain their normal behavior. This is a normal
user shell with that user's permissions, not a sandbox.

On close/EOF/signal, the helper enumerates only processes still belonging to its
PTY child's new session, then sends bounded HUP/TERM/KILL cleanup and reaps the
shell. It also closes the PTY, which delivers ordinary terminal hangup semantics.
It never targets the Godot process or arbitrary user PIDs. Programs deliberately
escaping with a new session are outside terminal session cleanup, as with normal
terminal emulators. No clipboard/OSC52 integration is installed. Mouse reporting,
hyperlinks, image protocols and scrollback reflow are not implemented in this
first integration; libvterm still handles ordinary interactive/full-screen text
applications and alternate screens.

## Tests and limitations

`test_backend.py` uses a fresh external HOME and `/bin/sh`, never user documents.
It checks actual `isatty()` behavior on all three descriptors, real shell cwd and
TERM, ANSI colors/styles, UTF-8 CJK/combining characters, resize plus SIGWINCH,
Ctrl-C interruption, alternate-screen entry/exit, exact application-cursor and
bracketed-paste bytes, malformed-command rejection, scrollback/clear, shell exit
code and shell/background-job cleanup for both explicit close and parent EOF.
The first fixture revisions caught a test EOF-drain loop and an interactive Bash
history-expansion premise; they are not counted as accepted runs.

Godot integration, touch/IME behavior, native application rendering and Linux
runtime acceptance require their own receipts; they are not implied by native
backend tests.
