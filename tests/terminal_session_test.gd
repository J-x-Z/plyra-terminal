# SPDX-License-Identifier: GPL-3.0-only
extends SceneTree
const Session = preload("res://scripts/terminal/terminal_session.gd")
var checks := 0
var failures := 0
var live: Node
var live_ready := false
var live_output := false
var deadline := 0
var lifecycle_stage := "shell_exit"
var retired_at := 0
var owned_helper := -1
var owned_shell := -1
var owned_background := -1
func check(value: bool, id: String) -> void:
    checks += 1
    if not value: failures += 1; push_error(id)
func key(code: Key, unicode: int = 0, control: bool = false, alt: bool = false, shift: bool = false) -> InputEventKey:
    var event := InputEventKey.new()
    event.pressed = true; event.keycode = code; event.physical_keycode = code; event.unicode = unicode
    event.ctrl_pressed = control; event.alt_pressed = alt; event.shift_pressed = shift
    return event
func ready() -> Dictionary:
    return {"type": "ready", "protocol": 1, "pid": 42, "shell": "/bin/sh", "cols": 80, "rows": 24}
func _initialize() -> void:
    _run.call_deferred()
func _run() -> void:
    check(Session.encode_key(key(KEY_A, 97)).get_string_from_utf8() == "a", "ordinary key UTF8")
    check(Session.encode_key(key(KEY_NONE, 0x4f60)).get_string_from_utf8() == "你", "Unicode key UTF8")
    check(Session.encode_key(key(KEY_C, 99, true)) == PackedByteArray([3]), "Ctrl-C physical key")
    check(Session.encode_key(key(KEY_D, 100, true)) == PackedByteArray([4]), "Ctrl-D physical key")
    check(Session.encode_key(key(KEY_SPACE, 32, true)) == PackedByteArray([0]), "Ctrl-space NUL byte without String.chr0")
    check(Session.encode_key(key(KEY_SPACE, 32, true, true)) == PackedByteArray([27, 0]), "Alt-Ctrl-space bytes")
    check(Session.encode_key(key(KEY_LEFT)).get_string_from_utf8() == "\u001b[D", "cursor normal mode")
    check(Session.encode_key(key(KEY_UP), true).get_string_from_utf8() == "\u001bOA", "cursor application mode")
    check(Session.encode_key(key(KEY_RIGHT, 0, true)).get_string_from_utf8() == "\u001b[1;5C", "modified arrow CSI")
    check(Session.encode_key(key(KEY_HOME)).get_string_from_utf8() == "\u001b[H", "Home")
    check(Session.encode_key(key(KEY_END)).get_string_from_utf8() == "\u001b[F", "End")
    check(Session.encode_key(key(KEY_PAGEUP)).get_string_from_utf8() == "\u001b[5~", "PageUp")
    check(Session.encode_key(key(KEY_PAGEDOWN)).get_string_from_utf8() == "\u001b[6~", "PageDown")
    check(Session.encode_key(key(KEY_INSERT)).get_string_from_utf8() == "\u001b[2~", "Insert")
    check(Session.encode_key(key(KEY_DELETE)).get_string_from_utf8() == "\u001b[3~", "Delete")
    check(Session.encode_key(key(KEY_BACKSPACE)) == PackedByteArray([127]), "Backspace")
    check(Session.encode_key(key(KEY_TAB)).get_string_from_utf8() == "\t", "Tab")
    check(Session.encode_key(key(KEY_TAB, 0, false, false, true)).get_string_from_utf8() == "\u001b[Z", "Shift Tab")
    check(Session.encode_key(key(KEY_ENTER)).get_string_from_utf8() == "\r", "Enter")
    check(Session.encode_key(key(KEY_ESCAPE)) == PackedByteArray([27]), "Escape")
    check(Session.encode_key(key(KEY_X, 120, false, true)) == PackedByteArray([27, 120]), "Alt letter")
    for function in range(12):
        check(not Session.encode_key(key(KEY_F1 + function)).is_empty(), "function F%d" % (function + 1))
    var released := key(KEY_A, 97); released.pressed = false
    check(Session.encode_key(released).is_empty(), "key up ignored")
    var meta := key(KEY_A, 97); meta.meta_pressed = true
    check(Session.encode_key(meta).is_empty(), "meta shortcut not forwarded")
    check(Session._valid_utf8("a你好🙂é".to_utf8_buffer()), "UTF8 valid Chinese emoji combining")
    for invalid in [PackedByteArray([128]), PackedByteArray([192, 128]), PackedByteArray([237, 160, 128]), PackedByteArray([244, 144, 128, 128]), PackedByteArray([240, 159]), PackedByteArray([226, 40, 161])]:
        check(not Session._valid_utf8(invalid), "malformed UTF8 rejected before decode")
    var session := Session.new()
    var ready_bytes := (JSON.stringify(ready()) + "\n").to_utf8_buffer()
    for byte in ready_bytes:
        check(session._consume_bytes(PackedByteArray([byte])), "bytewise partial JSON ready")
    check(session.running and session.shell_name == "/bin/sh", "ready event applied only after delimiter")
    var event := {"type": "screen", "cols": 80, "rows": 24, "cursor": {"row": 1, "col": 2, "visible": true}, "alternate": false, "title": "中文🙂", "full": true, "lines": [{"row": 0, "runs": [{"text": "你好", "width": 4, "fg": "#ffffff", "bg": "#000000"}]}]}
    var screen_bytes := (JSON.stringify(event) + "\n").to_utf8_buffer()
    for byte in screen_bytes:
        check(session._consume_bytes(PackedByteArray([byte])), "bytewise UTF8 JSON framing")
    check(session.screen.title == "中文🙂" and session.screen.plain_text().begins_with("你好\n"), "partial UTF8 not decoded early")
    check(session._consume_bytes('{"type":"bell"}\n{"type":"scrollback_clear"}\n'.to_utf8_buffer()), "multiple events in one read")
    check(not session._consume_bytes('{"type":"screen","rows":999999}\n'.to_utf8_buffer()), "invalid screen rejected")
    session.free()
    var bad_utf8 := Session.new()
    check(not bad_utf8._consume_bytes(PackedByteArray([255, 10])), "invalid framed UTF8 rejected")
    bad_utf8.free()
    var too_large := Session.new()
    var huge := PackedByteArray(); huge.resize(Session.MAX_JSON_LINE + 1); huge.fill(32)
    check(not too_large._consume_bytes(huge), "unfinished line bounded")
    too_large.free()
    var invalid_ready := Session.new()
    var wrong := ready(); wrong.protocol = 2
    check(not invalid_ready._handle_event(wrong), "protocol version checked")
    check(not invalid_ready._handle_event({"type": "screen"}), "screen before ready rejected")
    check(invalid_ready._handle_event(ready()), "valid protocol ready")
    check(not invalid_ready._handle_event(ready()), "duplicate ready rejected")
    check(invalid_ready._handle_event({"type": "exit", "code": 37}) and not invalid_ready.running and invalid_ready.exit_code == 37, "exit code retained")
    invalid_ready.free()
    if "--terminal-live" in OS.get_cmdline_user_args():
        live = Session.new(); root.add_child(live)
        check(live.start_session(80, 24), "actual helper starts")
        if live.status != "starting": _finish(); return
        deadline = Time.get_ticks_msec() + 10000
        return
    _finish()
func _process(_delta: float) -> bool:
    if lifecycle_stage != "shell_exit": return _lifecycle_tick()
    if live == null: return false
    if live.running and not live_ready:
        live_ready = true
        check(live.write_input("printf '\\033[32mPLYRA_%s_OK_%s\\033[0m\\n' CLIENT 你好; stty size\r".to_utf8_buffer()), "actual shell input queued")
        live.resize_terminal(100, 30)
    if live_ready and not live_output and live.screen.plain_text(true).contains("PLYRA_CLIENT_OK_你好") and live.screen.cols == 100 and live.screen.rows == 30:
        live_output = true
        check(live.screen.cols == 100 and live.screen.rows == 30, "actual resize observed")
        check(live.paste_text("printf 'PLYRA_%s_OK_%s\\n' PASTE 你好"), "actual bracketed paste queued")
        check(live.send_named_key("Enter"), "actual named enter")
        check(live.write_input("exit 37\r".to_utf8_buffer()), "actual exit requested")
    if live.status == "exited":
        check(live_ready and live_output and live.exit_code == 37, "actual owned shell exit 37")
        check(live.screen.plain_text(true).contains("PLYRA_PASTE_OK_你好"), "actual pasted UTF8 reaches visible model")
        live.close_session(); live.queue_free(); live = null
        _start_lifecycle("close_session")
    elif live.status in ["error", "unavailable"] or Time.get_ticks_msec() >= deadline:
        check(false, "actual helper failed or timeout: " + live.status_message)
        live.close_session(); live.queue_free(); live = null
        _finish()
    return false
func _start_lifecycle(kind: String) -> void:
    lifecycle_stage = kind; live_ready = false; retired_at = 0
    owned_helper = -1; owned_shell = -1; owned_background = -1
    live = Session.new(); root.add_child(live)
    check(live.start_session(80, 24), "lifecycle helper starts " + kind)
    deadline = Time.get_ticks_msec() + 10000

func _lifecycle_tick() -> bool:
    if retired_at > 0:
        var processes: Array = []
        # Godot only tracks its direct children; shell/jobs are grandchildren.
        # ps is a read-only fixture observation of these three captured PIDs.
        var result := OS.execute("/bin/ps", PackedStringArray(["-p", "%d,%d,%d" % [owned_helper, owned_shell, owned_background], "-o", "pid="]), processes, false)
        var gone: bool = result in [0, 1] and "".join(processes).strip_edges().is_empty()
        if gone:
            check(true, "owned helper/shell/background exit after " + lifecycle_stage)
            print(JSON.stringify({"event": "owned_lifecycle", "stage": lifecycle_stage, "helper": owned_helper, "shell": owned_shell, "background": owned_background, "ps_absent": true}))
            if lifecycle_stage == "close_session": _start_lifecycle("exit_tree")
            else: _finish()
        elif Time.get_ticks_msec() - retired_at > 3000:
            check(false, "owned helper/shell/background remained after " + lifecycle_stage)
            _finish()
        return false
    if live == null: return false
    if live.running and not live_ready:
        live_ready = true
        check(live.write_input("sleep 30 & printf 'PLYRA_BG_%s_READY\\n' $!\r".to_utf8_buffer()), "owned background starts " + lifecycle_stage)
    if live_ready:
        var pattern := RegEx.new(); pattern.compile("PLYRA_BG_([0-9]+)_READY")
        var matched := pattern.search(live.screen.plain_text(true))
        if matched != null:
            owned_background = int(matched.get_string(1)); owned_helper = live.helper_pid; owned_shell = live.shell_pid
            check(owned_background > 1 and owned_helper > 1 and owned_shell > 1, "owned background identity " + lifecycle_stage)
            if lifecycle_stage == "close_session":
                live.close_session()
                check(live._stdio == null and live._stderr == null and not live.running, "close detaches both handles")
            live.queue_free(); live = null
            retired_at = Time.get_ticks_msec()
    if live != null and (live.status in ["error", "unavailable"] or Time.get_ticks_msec() > deadline):
        check(false, "lifecycle failure " + live.status_message)
        live.close_session(); live.queue_free(); live = null; _finish()
    return false

func _finish() -> void:
    print(JSON.stringify({"test": "terminal_session", "checks": checks, "failures": failures, "live": "--terminal-live" in OS.get_cmdline_user_args()}))
    quit(0 if failures == 0 else 1)

# FVPROV1 10848 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMTM5ODZjN2Q2MzZhOGZkYWZlZmZlZDk2ODg5YmUwNzY1ZThhYzRlYjM4MDlmZWI1Mzk5N2U1MmU5YmVmNDA1ZiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjRjYjgwNmVmMDllOTEwZGRiNjhkMjNiMWU1ZDFkYTc5ZGE0YmYxZjFkMGY3MWNjMDMxNjAyNmRmZDc0Mzc3ZTdlOGYxNDhmMDk4ZDE1NTI0ODNmYmRlOTA3NTcxZGNhYTVhMDg3ZWRiNmEyZDM0MDE5NmYyNDY2M2E3ZTkwMGIifV19XQ== FVPROV1-END
