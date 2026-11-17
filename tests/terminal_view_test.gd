# SPDX-License-Identifier: GPL-3.0-only
extends SceneTree
const View=preload("res://scripts/terminal/terminal_view.gd")
const Style=preload("res://scripts/terminal/terminal_style.gd")
class KeyGate extends Node:
    var target: Control
    func _input(event: InputEvent) -> void:
        if event is InputEventKey and is_instance_valid(target) and target.handle_key(event):
            get_viewport().set_input_as_handled()
var view: Control
var panel: PanelContainer
var gate: Node
var failures:=0
var checks: Array=[]
var finished:=false
var allowed:=true
var soft_count:=0

func _initialize() -> void:
    create_timer(18).timeout.connect(func():
        if not finished: check(false,"test_deadline"); _finish())
    _run.call_deferred()
func check(value: bool,label: String) -> void:
    checks.append({"case":label,"passed":value})
    if not value: failures+=1;push_error(label)
func settle(count: int=4) -> void:
    for unused in count: await process_frame
func wait_for(predicate: Callable,ms: int=3000) -> bool:
    var deadline:=Time.get_ticks_msec()+ms
    while not predicate.call() and Time.get_ticks_msec()<deadline: await process_frame
    return bool(predicate.call())
func key(code: Key,point: int=0,control: bool=false) -> void:
    var event:=InputEventKey.new();event.pressed=true;event.keycode=code;event.physical_keycode=code;event.unicode=point;event.ctrl_pressed=control
    root.push_input(event,true)
    event=event.duplicate();event.pressed=false;root.push_input(event,true)
func commit(value: String) -> void:
    view.input_field.insert_text_at_caret(value)
    view.input_field.text_changed.emit(view.input_field.text)
    await settle()
func _run() -> void:
    Engine.max_fps=120
    root.size=Vector2i(1000,700)
    panel=PanelContainer.new();panel.size=root.size;root.add_child(panel)
    Style.install_backdrop(panel)
    view=View.new();view.input_guard=func(): return allowed
    var injected:=Style.theme()
    view.host_theme_factory=func(_scale: float): return injected
    view.host_touch_device=-2011
    panel.add_child(view)
    gate=KeyGate.new();gate.target=view;root.add_child(gate)
    await settle()
    check(view.theme==injected,"host theme injection")
    check(view.canvas.host_touch_device==-2011,"host touch device injection")
    check(view.terminal.status=="idle" and view.terminal.helper_pid==-1,"embedding starts inert")
    check(not view.input_field.soft_key_guard.is_valid(),"soft route disabled without host authority")
    var holder:=panel.get_node(Style.HOLDER_NAME)
    check(holder.get_node("BackdropCopy").get_index()<holder.get_node("AcrylicBackdrop").get_index(),"acrylic copies before drawing surface")
    for geometry in [Vector2i(320,640),Vector2i(432,900),Vector2i(900,432)]:
        root.size=geometry;panel.size=geometry;await settle()
        check(view.get_combined_minimum_size().x<=geometry.x and panel.get_global_rect().grow(.1).encloses(view.input_field.get_global_rect()),"resize fits "+str(geometry))
    root.size=Vector2i(1000,700);panel.size=root.size;view.set_gui_scale(2.0);await settle()
    check(view.input_field.custom_minimum_size.y==88 and view.canvas.gui_scale==2.0,"DPI scale uses logical control and cell sizes")
    view.set_gui_scale(1.0);await settle()
    view.open_session()
    check(await wait_for(func(): return view.terminal.running),"explicit launch starts native PTY")
    if not view.terminal.running: _finish();return
    view.focus_input();await settle()
    await commit("printf 'INDEPENDENT_%s_你好\\n' TERMINAL")
    key(KEY_ENTER)
    check(await wait_for(func(): return view.terminal.screen.plain_text(true).contains("INDEPENDENT_TERMINAL_你好")),"native LineEdit commit and Enter reach PTY")
    for point in "printf 'HARDWARE_%s\\n' ONCEx": key(point.to_upper().unicode_at(0),point.unicode_at(0))
    key(KEY_BACKSPACE);key(KEY_ENTER)
    check(await wait_for(func(): return view.terminal.screen.plain_text(true).contains("\nHARDWARE_ONCE\n")),"hardware route types and deletes once")
    # A fake broker callback proves default hardware events cannot reenter this
    # soft path, while exactly one explicitly authorized event can.
    view.input_field.soft_control_key=func(_event): soft_count+=1;return true
    var soft:=InputEventKey.new();soft.pressed=true;soft.keycode=KEY_ENTER;soft.device=-2012
    view.input_field._gui_input(soft)
    check(soft_count==0,"unconfigured soft event ignored")
    view.input_field.soft_key_guard=func(event): return event.device==-2012
    view.input_field._gui_input(soft)
    var hardware:=soft.duplicate();hardware.device=0;view.input_field._gui_input(hardware)
    check(soft_count==1,"authorized soft event routed once; hardware excluded")
    view.input_field.soft_control_key=Callable(view,"handle_key")
    await commit("sleep 10");key(KEY_ENTER);await settle();key(KEY_C,99,true)
    await commit("printf 'INTERRUPT_%s\\n' OK");key(KEY_ENTER)
    check(await wait_for(func(): return view.terminal.screen.plain_text(true).contains("\nINTERRUPT_OK\n")),"Ctrl C interrupts foreground child")
    var before: int=view.terminal._command_bytes
    allowed=false;view.input_field.text="WITHHELD";view.input_field.flush_committed()
    check(view.terminal._command_bytes==before and view.input_field.text.is_empty(),"revoked input guard discards unsent text")
    allowed=true
    var pids: Dictionary={"helper":view.terminal.helper_pid,"shell":view.terminal.shell_pid}
    view.queue_free();await settle()
    check(await wait_for(func(): return get_nodes_in_group("plyra_terminal_owned_helpers_v1").is_empty()),"closing view reaps own PTY")
    print(JSON.stringify({"event":"owned_close","pids":pids}))
    _finish()
func _finish() -> void:
    if finished: return
    finished=true
    if is_instance_valid(view): view.close_session()
    print(JSON.stringify({"test":"terminal_view","checks":checks,"failures":failures,"passed":failures==0,"native_pty":true,"physical_input":false,"gpu":false}))
    if is_instance_valid(panel): panel.queue_free()
    if is_instance_valid(gate): gate.queue_free()
    await settle()
    quit(0 if failures==0 else 1)

# FVPROV1 6125 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYjI3ODgxOWU4MjI1YzA4ZDQwOTliZTEwNTRhMmFlN2EwYWFlOWRlOWM0YTQ4N2MxMGZiNjYwZWFiZGQzZmZkNSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZDIyNzFiMmYwNzA1MzI4MjBiOGY0MTlkNGUyYjdkOThiOWU4YmM1ZWNmZmYyZjFjZTIzYmNjOTc1NTczMjY1NTI3MmYzNDFmMWZkMTAyOTc2Y2IxNWRkNmUzODY3NDJhOGVjYTU4NzZmM2M5NGZlNTdlNGVjNmQ5NmU3MDhiMDYifV19XQ== FVPROV1-END
