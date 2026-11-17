# SPDX-License-Identifier: GPL-3.0-only
extends SceneTree
## Actual standalone scene, native window/GPU and real PTY. Input is injected;
## this is not physical keyboard, system IME candidate or phone acceptance.
const MainScene=preload("res://standalone/main.tscn")
var app: Control
var checks: Array=[]
var failures:=0
var output:=""
var finished:=false
var pids:={"helper":-1,"shell":-1}

func _initialize() -> void:
    for argument in OS.get_cmdline_user_args():
        if argument.begins_with("--probe-output="): output=argument.trim_prefix("--probe-output=")
    if output.is_empty() or not output.is_absolute_path() or DisplayServer.get_name()=="headless": quit(2);return
    create_timer(24).timeout.connect(func():
        if not finished: check(false,"owned_deadline"); _finish())
    _run.call_deferred()
func check(value: bool,label: String,detail: Dictionary={}) -> void:
    checks.append({"case":label,"passed":value,"detail":detail})
    if not value: failures+=1;push_error(label)
func settle(count: int=6) -> void:
    for unused in count: await process_frame
func wait_for(predicate: Callable,ms: int=4000) -> bool:
    var deadline:=Time.get_ticks_msec()+ms
    while not predicate.call() and Time.get_ticks_msec()<deadline: await process_frame
    return bool(predicate.call())
func enter() -> void:
    var event:=InputEventKey.new();event.pressed=true;event.keycode=KEY_ENTER;event.physical_keycode=KEY_ENTER
    root.push_input(event,true)
    event=event.duplicate();event.pressed=false;root.push_input(event,true)
func commit(value: String) -> void:
    app.terminal_view.input_field.insert_text_at_caret(value)
    app.terminal_view.input_field.text_changed.emit(app.terminal_view.input_field.text)
    await settle(2)
func _run() -> void:
    Engine.max_fps=60
    root.size=Vector2i(1000,700)
    app=MainScene.instantiate();root.add_child(app)
    await settle()
    var ui: Control=app.terminal_view
    var session: Node=ui.terminal
    check(await wait_for(func(): return session.running),"standalone explicit launch starts PTY")
    if not session.running: _finish();return
    pids={"helper":session.helper_pid,"shell":session.shell_pid}
    ui.focus_input();await settle(2)
    check(ui.input_field.has_focus(),"native window input field focus")
    await commit("printf '\\033[2J\\033[H\\033[36mPlyra %s\\033[0m\\n中文终端 · UTF-8 ✓\\n\\nNative window / Metal / PTY\\n' TERMINAL")
    enter()
    check(await wait_for(func(): return session.screen.plain_text(true).contains("Plyra TERMINAL") and session.screen.plain_text(true).contains("中文终端")),"LineEdit commit and Enter produce ASCII and CJK output")
    var first:=Vector2i(session.screen.cols,session.screen.rows)
    root.size=Vector2i(1120,780)
    await settle(12)
    check(await wait_for(func(): return Vector2i(session.screen.cols,session.screen.rows)!=first),"native resize reaches PTY",{"before":str(first),"after":str(Vector2i(session.screen.cols,session.screen.rows))})
    await commit("printf '\\nWindow resized · %s\\n' '窗口已缩放'; stty size")
    enter()
    check(await wait_for(func(): return session.screen.plain_text(true).contains("窗口已缩放")),"post-resize terminal remains interactive")
    await settle(12)
    await RenderingServer.frame_post_draw
    var snapshot:=root.get_texture().get_image()
    check(snapshot!=null and not snapshot.is_empty(),"native viewport readback")
    if snapshot!=null and not snapshot.is_empty():
        check(snapshot.save_png(output.path_join("terminal-native.png"))==OK,"saved native viewport screenshot",{"size":str(snapshot.get_size())})
    _finish()
func _finish() -> void:
    if finished: return
    finished=true
    var report:={"schema":"plyra.terminal.native-window","version":1,"passed":failures==0,"checks":checks,"failures":failures,"engine":Engine.get_version_info(),"display":DisplayServer.get_name(),"rendering_method":RenderingServer.get_current_rendering_method(),"rendering_driver":RenderingServer.get_current_rendering_driver_name(),"profile":OS.get_user_data_dir(),"owned_pids":pids,"native_window":true,"native_pty":true,"injected_input":true,"physical_keyboard":false,"phone":false,"close_requested":true,"accessibility_supported":is_accessibility_supported(),"accessibility_enabled":is_accessibility_enabled()}
    var file:=FileAccess.open(output.path_join("WINDOW-RESULT.json"),FileAccess.WRITE)
    if file!=null: file.store_string(JSON.stringify(report,"  ")+"\n");file.close()
    print(JSON.stringify(report))
    if is_instance_valid(app): app.request_close()
    else: quit(1)

# FVPROV1 4591 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNTc3NTRhNTliYjM0ZmIyMjg2Yjg4Yjc1NzI2ZDI5ODgyOGE1ZTQ1ZmIwOTE4MmJmOThjYWRiNGM4MDUxODliNyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiY2JhMjM1ZjBiZjNhN2RlMzA5YWU3NjY2NjU4NjQ2ZmJhMTRhODVjZjZlZDcyNDFlZjY1ZDQ3MzMyOWVmMmMzZWMyMDk4NGNmMzQzZGMzMDQzNjJlOGQwZTJlN2UxNTdjMDdlYTM4OWI3MmFlMzdhZDQ2OWEyMDk5YmI4ODUzMDEifV19XQ== FVPROV1-END
