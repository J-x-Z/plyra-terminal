# SPDX-License-Identifier: GPL-3.0-only
extends Node
## Real PTY terminal helper owner. The helper receives bytes/key names; neither
## terminal output nor UI text is passed to a shell -c command by this client.
const ScreenModel = preload("res://scripts/terminal/terminal_screen.gd")
const ASSET_ROOT := "res://assets/terminal/"
const MAX_MANIFEST_BYTES := 65536
const MAX_HELPER_BYTES := 16777216
const MAX_JSON_LINE := 1048576
const READ_BYTES_PER_FRAME := 65536
const MAX_EVENTS_PER_FRAME := 128
const MAX_PENDING_COMMAND_BYTES := 131072
const INPUT_CHUNK_BYTES := 256
const START_TIMEOUT_MS := 5000
const WRITE_TIMEOUT_MS := 3000
signal screen_changed
signal status_changed
signal bell
# Dropping a FileAccess closes EOF but does not waitpid the direct helper. This
# one owner survives a terminal window, polls only registered direct children
# without blocking, and prevents zombie helpers while the desktop stays alive.
class HelperReaper extends Node:
    const GROUP := "plyra_terminal_owned_helpers_v1"
    const MAX_HELPERS := 64
    var pids: Dictionary = {}
    var delay := 0.0
    func _ready() -> void:
        add_to_group(GROUP)
        process_mode = Node.PROCESS_MODE_ALWAYS
    func watch(pid: int) -> void:
        if pid > 0: pids[pid] = true
    func _process(delta: float) -> void:
        delay += delta
        if delay < 0.05: return
        delay = 0.0
        for pid in pids.keys():
            if not OS.is_process_running(pid): pids.erase(pid)
        if pids.is_empty(): queue_free()

var screen: RefCounted = ScreenModel.new()
var status := "idle"
var status_message := "终端未启动"
var shell_name := ""
var running := false
var exit_code := -1
var helper_pid := -1
var shell_pid := -1
var _stdio: FileAccess
var _stderr: FileAccess
var _output := PackedByteArray()
var _commands: Array[PackedByteArray] = []
var _command_bytes := 0
var _started_at := 0
var _write_stalled_at := 0
var _last_cols := 80
var _last_rows := 24
var _stderr_tail := PackedByteArray()
var _received_exit := false
var _helper_owner: WeakRef

func _ready() -> void:
    set_process(false)

func start_session(cols: int = 80, rows: int = 24) -> bool:
    if not is_inside_tree() or _stdio != null or running or status == "starting": return false
    var reaper: HelperReaper
    for owner in get_tree().get_nodes_in_group(HelperReaper.GROUP):
        if owner is HelperReaper and not owner.is_queued_for_deletion(): reaper = owner; break
    if reaper != null and reaper.pids.size() >= HelperReaper.MAX_HELPERS:
        _set_status("unavailable", "同时运行的终端已达到上限")
        return false
    var executable := _prepare_executable()
    if executable.is_empty():
        _set_status("unavailable", "本平台的终端运行组件不可用或校验失败")
        return false
    _last_cols = clampi(cols, ScreenModel.MIN_COLS, ScreenModel.MAX_COLS)
    _last_rows = clampi(rows, ScreenModel.MIN_ROWS, ScreenModel.MAX_ROWS)
    screen.reset(_last_cols, _last_rows)
    shell_name = ""; exit_code = -1; shell_pid = -1; _received_exit = false
    _output.clear(); _commands.clear(); _command_bytes = 0; _stderr_tail.clear(); _write_stalled_at = 0
    # Godot's nonblocking FileAccessUnixPipe uses FIONREAD then one read/write.
    # https://docs.godotengine.org/en/stable/classes/class_os.html#class-os-method-execute-with-pipe
    # Pinned implementation: ed1daf0bf001b61586d9930840f2f1394092c079,
    # drivers/unix/file_access_unix_pipe.cpp:47-62,133-173.
    var owned := OS.execute_with_pipe(executable, PackedStringArray(["--cols", str(_last_cols), "--rows", str(_last_rows)]), false)
    if owned.is_empty() or not owned.get("stdio") is FileAccess or not owned.get("stderr") is FileAccess:
        _set_status("error", "无法启动终端运行组件")
        return false
    _stdio = owned.stdio; _stderr = owned.stderr; helper_pid = int(owned.get("pid", -1))
    if reaper == null:
        reaper = HelperReaper.new(); get_tree().root.add_child(reaper)
    reaper.watch(helper_pid); _helper_owner = weakref(reaper)
    _started_at = Time.get_ticks_msec()
    _set_status("starting", "正在启动本地 Shell…")
    set_process(true)
    screen_changed.emit()
    return true

func resize_terminal(cols: int, rows: int) -> void:
    cols = clampi(cols, ScreenModel.MIN_COLS, ScreenModel.MAX_COLS)
    rows = clampi(rows, ScreenModel.MIN_ROWS, ScreenModel.MAX_ROWS)
    if cols == _last_cols and rows == _last_rows: return
    _last_cols = cols; _last_rows = rows
    if _stdio != null: _queue_command("resize %d %d" % [cols, rows])

func write_input(bytes: PackedByteArray) -> bool:
    return _queue_bytes("input", bytes)

func paste_text(text: String) -> bool:
    # Explicit paste only. Bracketed-paste handling belongs to libvterm/backend.
    return _queue_bytes("paste", text.to_utf8_buffer())

func _queue_bytes(kind: String, bytes: PackedByteArray) -> bool:
    if not running or _stdio == null or bytes.is_empty() or bytes.size() > 65536: return false
    var pending: Array[PackedByteArray] = []
    var count := 0
    var offset := 0
    while offset < bytes.size():
        var end := mini(offset + INPUT_CHUNK_BYTES, bytes.size())
        if kind == "paste":
            # Bracketed-paste markers must not bisect a UTF-8 character.
            while end < bytes.size() and end > offset and (bytes[end] & 192) == 128: end -= 1
        var chunk := bytes.slice(offset, end)
        var command := (kind + " " + Marshalls.raw_to_base64(chunk) + "\n").to_ascii_buffer()
        pending.append(command); count += command.size(); offset += chunk.size()
    if _command_bytes + count > MAX_PENDING_COMMAND_BYTES: return false
    _commands.append_array(pending); _command_bytes += count
    return true

func send_named_key(key: String, modifiers: int = 0) -> bool:
    if not running or not key in ["Up", "Down", "Left", "Right", "Home", "End", "PageUp", "PageDown", "Insert", "Delete", "Backspace", "Tab", "Enter", "Escape", "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12"]: return false
    return _queue_command("key %s %d" % [key, modifiers & 7])

func send_key(event: InputEventKey) -> bool:
    if not event.pressed or event.meta_pressed: return false
    var name := key_name(event)
    if not name.is_empty():
        var modifiers := (1 if event.shift_pressed else 0) | (2 if event.alt_pressed else 0) | (4 if event.ctrl_pressed else 0)
        return send_named_key(name, modifiers)
    var encoded := encode_key(event, screen.application_cursor)
    return write_input(encoded) if not encoded.is_empty() else false

static func key_name(event: InputEventKey) -> String:
    var key := event.keycode if event.keycode != KEY_NONE else event.physical_keycode
    var keys := {KEY_UP: "Up", KEY_DOWN: "Down", KEY_LEFT: "Left", KEY_RIGHT: "Right", KEY_HOME: "Home", KEY_END: "End", KEY_PAGEUP: "PageUp", KEY_PAGEDOWN: "PageDown", KEY_INSERT: "Insert", KEY_DELETE: "Delete", KEY_BACKSPACE: "Backspace", KEY_TAB: "Tab", KEY_ENTER: "Enter", KEY_KP_ENTER: "Enter", KEY_ESCAPE: "Escape"}
    if keys.has(key): return keys[key]
    if key >= KEY_F1 and key <= KEY_F12: return "F%d" % (key - KEY_F1 + 1)
    return ""

static func encode_key(event: InputEventKey, application_cursor: bool = false) -> PackedByteArray:
    if not event.pressed or event.meta_pressed: return PackedByteArray()
    var name := key_name(event)
    var sequence := ""
    var modifiers := 1 + (1 if event.shift_pressed else 0) + (2 if event.alt_pressed else 0) + (4 if event.ctrl_pressed else 0)
    var finals := {"Up": "A", "Down": "B", "Right": "C", "Left": "D", "Home": "H", "End": "F"}
    if finals.has(name):
        sequence = ("\u001b[1;%d%s" % [modifiers, finals[name]]) if modifiers > 1 else ("\u001bO" if application_cursor else "\u001b[") + finals[name]
    elif name in ["Insert", "Delete", "PageUp", "PageDown"]:
        var numbers := {"Insert": 2, "Delete": 3, "PageUp": 5, "PageDown": 6}
        sequence = "\u001b[%d%s~" % [numbers[name], ";%d" % modifiers if modifiers > 1 else ""]
    elif name.begins_with("F"):
        var number := int(name.substr(1))
        if number <= 4:
            var final: String = ["P", "Q", "R", "S"][number - 1]
            sequence = "\u001b[1;%d%s" % [modifiers, final] if modifiers > 1 else "\u001bO" + final
        else:
            var number_code: int = [15, 17, 18, 19, 20, 21, 23, 24][number - 5]
            sequence = "\u001b[%d%s~" % [number_code, ";%d" % modifiers if modifiers > 1 else ""]
    elif name == "Backspace": sequence = "\u007f"
    elif name == "Tab": sequence = "\u001b[Z" if event.shift_pressed else "\t"
    elif name == "Enter": sequence = "\r"
    elif name == "Escape": sequence = "\u001b"
    else:
        var key := event.physical_keycode if event.physical_keycode != KEY_NONE else event.keycode
        if event.ctrl_pressed:
            if key >= KEY_A and key <= KEY_Z: sequence = String.chr(key - KEY_A + 1)
            elif key == KEY_SPACE or key == KEY_AT or key == KEY_2: return PackedByteArray([27, 0]) if event.alt_pressed else PackedByteArray([0])
            elif key in [KEY_BRACKETLEFT, KEY_BACKSLASH, KEY_BRACKETRIGHT, KEY_ASCIICIRCUM, KEY_UNDERSCORE]:
                sequence = String.chr({KEY_BRACKETLEFT: 27, KEY_BACKSLASH: 28, KEY_BRACKETRIGHT: 29, KEY_ASCIICIRCUM: 30, KEY_UNDERSCORE: 31}[key])
            elif key == KEY_6: sequence = String.chr(30)
            elif key == KEY_MINUS: sequence = String.chr(31)
            elif key == KEY_QUESTION or key == KEY_8: sequence = String.chr(127)
        elif event.unicode >= 32 and event.unicode != 127: sequence = String.chr(event.unicode)
    if sequence.is_empty(): return PackedByteArray()
    # Alt is encoded in CSI modifiers above; ordinary and single-byte keys use ESC.
    if event.alt_pressed and not finals.has(name) and not name in ["Insert", "Delete", "PageUp", "PageDown"] and not name.begins_with("F"):
        sequence = "\u001b" + sequence
    return sequence.to_utf8_buffer()

func _queue_command(command: String) -> bool:
    if _stdio == null: return false
    var bytes := (command + "\n").to_ascii_buffer()
    # POSIX PIPE_BUF is at least 512. A single <=512-byte nonblocking write is
    # atomic, avoiding an unknowable partial write in FileAccess's bool API.
    if bytes.size() > 512 or _command_bytes + bytes.size() > MAX_PENDING_COMMAND_BYTES: return false
    _commands.append(bytes); _command_bytes += bytes.size()
    return true

func _flush_commands() -> void:
    var sent := 0
    while not _commands.is_empty() and sent < 64 and _stdio != null:
        var command: PackedByteArray = _commands[0]
        if not _stdio.store_buffer(command):
            if _write_stalled_at == 0: _write_stalled_at = Time.get_ticks_msec()
            if Time.get_ticks_msec() - _write_stalled_at > WRITE_TIMEOUT_MS: _fail("终端输入管道无法继续写入")
            return
        _commands.pop_front(); _command_bytes -= command.size(); sent += 1; _write_stalled_at = 0

func _process(_delta: float) -> void:
    if _stdio == null: return
    _flush_commands()
    if _stdio == null: return
    var available := mini(int(_stdio.get_length()), READ_BYTES_PER_FRAME)
    if available > 0:
        var chunk := _stdio.get_buffer(available)
        if not chunk.is_empty() and not _consume_bytes(chunk):
            _fail("终端返回了无效或超出限制的数据")
            return
    else:
        # Complete lines already buffered are still dispatched within a bounded
        # per-frame budget even when no more bytes are currently available.
        if not _consume_bytes(PackedByteArray()):
            _fail("终端返回了无效或超出限制的数据")
            return
    if _stderr != null:
        var error_bytes := mini(int(_stderr.get_length()), 4096)
        if error_bytes > 0:
            _stderr_tail.append_array(_stderr.get_buffer(error_bytes))
            if _stderr_tail.size() > 4096: _stderr_tail = _stderr_tail.slice(_stderr_tail.size() - 4096)
    if _stdio == null: return
    if status == "starting" and Time.get_ticks_msec() - _started_at > START_TIMEOUT_MS:
        _fail("终端启动超时")
    elif helper_pid > 0 and not _helper_is_running() and _stdio.get_length() == 0:
        if not _output.is_empty():
            # Drain complete buffered events on later frames, but a truncated
            # final line can never become complete after the owned child exits.
            if _output.find(10) < 0: _fail("终端输出意外中断")
        elif not _received_exit: _fail("终端运行组件已退出")
        else: _release_pipes()

func _helper_is_running() -> bool:
    var owner: Variant = _helper_owner.get_ref() if _helper_owner != null else null
    return is_instance_valid(owner) and owner.pids.has(helper_pid)

func _consume_bytes(chunk: PackedByteArray) -> bool:
    if _output.size() + chunk.size() > MAX_JSON_LINE + READ_BYTES_PER_FRAME: return false
    _output.append_array(chunk)
    var handled := 0
    while handled < MAX_EVENTS_PER_FRAME:
        var newline := _output.find(10)
        if newline < 0: return _output.size() <= MAX_JSON_LINE
        if newline > MAX_JSON_LINE: return false
        var raw := _output.slice(0, newline)
        _output = _output.slice(newline + 1)
        if not _valid_utf8(raw): return false
        var parser := JSON.new()
        if parser.parse(raw.get_string_from_utf8()) != OK or not parser.data is Dictionary: return false
        if not _handle_event(parser.data): return false
        handled += 1
    return _output.size() <= MAX_JSON_LINE

func _handle_event(event: Dictionary) -> bool:
    match event.get("type"):
        "ready":
            if running or not ScreenModel._integer(event.get("protocol"), 1, 1) or not event.get("shell") is String or event.shell.length() > 4096 \
                or not ScreenModel._integer(event.get("pid"), 1, 2147483647) \
                or not ScreenModel._integer(event.get("cols"), ScreenModel.MIN_COLS, ScreenModel.MAX_COLS) \
                or not ScreenModel._integer(event.get("rows"), ScreenModel.MIN_ROWS, ScreenModel.MAX_ROWS): return false
            screen.reset(int(event.cols), int(event.rows))
            shell_pid = int(event.pid); shell_name = ScreenModel._safe_title(event.shell)
            running = true; _set_status("running", shell_name); screen_changed.emit()
        "screen":
            if not running or not screen.apply_screen(event): return false
            screen_changed.emit()
        "scrollback":
            if not running or not screen.append_scrollback(event): return false
            screen_changed.emit()
        "scrollback_clear":
            if not running: return false
            screen.clear_scrollback(); screen_changed.emit()
        "bell":
            if running: bell.emit()
        "exit":
            if not ScreenModel._integer(event.get("code"), 0, 255): return false
            exit_code = int(event.code); running = false; _received_exit = true
            _set_status("exited", "Shell 已退出（%d）" % exit_code)
        "error":
            if not event.get("message") is String or event.message.length() > 4096: return false
            _fail(ScreenModel._safe_title(event.message))
        _: return false
    return true

static func _valid_utf8(bytes: PackedByteArray) -> bool:
    var index := 0
    while index < bytes.size():
        var lead: int = bytes[index]
        if lead < 128: index += 1; continue
        var count := 0
        var low := 128
        var high := 191
        if lead >= 194 and lead <= 223: count = 1
        elif lead >= 224 and lead <= 239:
            count = 2
            if lead == 224: low = 160
            elif lead == 237: high = 159
        elif lead >= 240 and lead <= 244:
            count = 3
            if lead == 240: low = 144
            elif lead == 244: high = 143
        else: return false
        if index + count >= bytes.size() or bytes[index + 1] < low or bytes[index + 1] > high: return false
        for offset in range(2, count + 1):
            if bytes[index + offset] < 128 or bytes[index + offset] > 191: return false
        index += count + 1
    return true

func close_session() -> void:
    if _stdio != null:
        # EOF is the authoritative native cleanup trigger. Never wait on the UI
        # thread or signal a saved PID (it could have been reused).
        _commands.clear(); _command_bytes = 0
        _queue_command("close"); _flush_commands()
    _release_pipes()
    running = false
    if status != "idle": _set_status("closed", "终端已关闭")

func _fail(message: String) -> void:
    running = false
    _set_status("error", message)
    _release_pipes()

func _release_pipes() -> void:
    if _stdio != null: _stdio.close(); _stdio = null
    if _stderr != null: _stderr.close(); _stderr = null
    _commands.clear(); _command_bytes = 0; _output.clear(); helper_pid = -1; _helper_owner = null
    set_process(false)

func _exit_tree() -> void:
    close_session()

func _set_status(value: String, message: String) -> void:
    status = value; status_message = message; status_changed.emit()

static func _digest(bytes: PackedByteArray) -> String:
    var hash := HashingContext.new()
    if hash.start(HashingContext.HASH_SHA256) != OK or hash.update(bytes) != OK: return ""
    return hash.finish().hex_encode()

static func _safe_absolute(path: String) -> bool:
    if not path.is_absolute_path() or path.simplify_path() != path or path.contains("\\"): return false
    var cursor_path := "/"
    for part in path.substr(1).split("/", false):
        var directory := DirAccess.open(cursor_path)
        if directory == null or directory.is_link(part): return false
        cursor_path = cursor_path.path_join(part)
    return true

static func _platform_key() -> String:
    if OS.get_name() == "macOS" and OS.has_feature("arm64"): return "macos_arm64"
    if OS.get_name() == "Linux":
        if OS.has_feature("arm64"): return "linux_arm64"
        if OS.has_feature("x86_64"): return "linux_x86_64"
    return ""

static func _prepare_executable() -> String:
    var platform_key := _platform_key()
    if platform_key.is_empty(): return ""
    var manifest_file := FileAccess.open(ASSET_ROOT + "manifest.json", FileAccess.READ)
    if manifest_file == null or manifest_file.get_length() <= 0 or manifest_file.get_length() > MAX_MANIFEST_BYTES: return ""
    var manifest_bytes := manifest_file.get_buffer(manifest_file.get_length()); manifest_file.close()
    if not _valid_utf8(manifest_bytes): return ""
    var parser := JSON.new()
    if parser.parse(manifest_bytes.get_string_from_utf8()) != OK or not parser.data is Dictionary: return ""
    var manifest: Dictionary = parser.data
    if manifest.get("schema") != "plyra.mobile.terminal-helper" or manifest.get("version") != 1 or manifest.get("protocol") != 1 \
        or not manifest.get("runtime_files") is Dictionary or not manifest.runtime_files.get(platform_key) is Dictionary: return ""
    var item: Dictionary = manifest.runtime_files[platform_key]
    var expected_name: String = {"macos_arm64": "plyra-terminal-macos-arm64", "linux_arm64": "plyra-terminal-linux-arm64", "linux_x86_64": "plyra-terminal-linux-x86_64"}[platform_key]
    if item.get("path") != "bin/" + expected_name or not ScreenModel._integer(item.get("bytes"), 64, MAX_HELPER_BYTES) \
        or not item.get("sha256") is String or item.sha256.length() != 64: return ""
    for character in item.sha256:
        if not character in "0123456789abcdef": return ""
    var binary := FileAccess.open(ASSET_ROOT + item.path, FileAccess.READ)
    if binary == null or binary.get_length() != int(item.bytes): return ""
    var bytes := binary.get_buffer(int(item.bytes)); binary.close()
    if bytes.size() != int(item.bytes) or _digest(bytes) != item.sha256: return ""
    if platform_key.begins_with("macos"):
        if bytes.slice(0, 4) != PackedByteArray([207, 250, 237, 254]): return ""
    elif bytes.slice(0, 7) != PackedByteArray([127, 69, 76, 70, 2, 1, 1]): return ""
    var base := ProjectSettings.globalize_path("user://terminal-runtime")
    if not _safe_absolute(base): return ""
    if not DirAccess.dir_exists_absolute(base) and DirAccess.make_dir_absolute(base) != OK: return ""
    if not _safe_absolute(base) or FileAccess.set_unix_permissions(base, 448) != OK: return ""
    var directory_path := base.path_join(item.sha256)
    if not _safe_absolute(directory_path): return ""
    if not DirAccess.dir_exists_absolute(directory_path) and DirAccess.make_dir_absolute(directory_path) != OK: return ""
    if not _safe_absolute(directory_path) or FileAccess.set_unix_permissions(directory_path, 448) != OK: return ""
    var target := directory_path.path_join(expected_name)
    if not _safe_absolute(target): return ""
    if FileAccess.file_exists(target):
        var existing := FileAccess.open(target, FileAccess.READ)
        if existing == null or existing.get_length() != bytes.size(): return ""
        var matches: bool = _digest(existing.get_buffer(bytes.size())) == item.sha256
        existing.close()
        if not matches or FileAccess.get_unix_permissions(target) != 448: return ""
        return target
    # The new cache pathname was checked, and existing files are never replaced.
    # Same-user concurrent pathname replacement remains a filesystem boundary,
    # not a claim of a race-free openat/O_EXCL sandbox.
    var output := FileAccess.open(target, FileAccess.WRITE)
    if output == null: return ""
    if not output.store_buffer(bytes): output.close(); return ""
    output.close()
    if not _safe_absolute(target) or FileAccess.set_unix_permissions(target, 448) != OK or FileAccess.get_sha256(target) != item.sha256: return ""
    return target

# FVPROV1 21794 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiODMwOTdiZTA5ODFiNmUxZTM3ODYwMzA0NzU2ZDliY2ZiNjA5MjZkYmM1YjAyM2U1ZjVmMjIyNWFlMzE2MmYwZiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMTQxOGI2MmNmODU2N2Y2YWY4YWI4ZjlmMGFkOWViZjFkZTFiZGFmMWUzNmJlMTA0OGE4ZjY0MGFiYmYyYjdmNzlmYTNkYjliYjliNmFiNTVhNjhlYmRlZWUxOWY5MmVhMDdjODY3NjcwM2Q2ZDA5OTE0Mzc4ODJkOWRmYTE5MGUifV19XQ== FVPROV1-END
