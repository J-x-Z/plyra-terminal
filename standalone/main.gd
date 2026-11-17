# SPDX-License-Identifier: GPL-3.0-only
extends Control
## This application adapter owns startup, native-key routing and OS-window close.
## Embedding TerminalView alone remains inert until open_session is called.
const View=preload("res://scripts/terminal/terminal_view.gd")
const Style=preload("res://scripts/terminal/terminal_style.gd")
var terminal_view: Control
var panel: PanelContainer
var _closing:=false
var _screen_index:=-2
var _scale:=1.0
var _scale_override:=0.0

func _ready() -> void:
    get_tree().auto_accept_quit=false
    get_window().title="Plyra Terminal"
    get_window().close_requested.connect(request_close)
    for argument in OS.get_cmdline_user_args():
        if argument.begins_with("--gui-scale="):
            _scale_override=clampf(argument.trim_prefix("--gui-scale=").to_float(),.5,4.0)
    panel=PanelContainer.new();panel.name="TerminalPanel";panel.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
    add_child(panel)
    terminal_view=View.new();terminal_view.name="TerminalView"
    # Godot emulates physical touch as mouse device -1 in standalone mode.
    terminal_view.host_touch_device=InputEvent.DEVICE_ID_EMULATION
    terminal_view.input_guard=func(): return not _closing
    panel.add_child(terminal_view)
    Style.install_backdrop(panel)
    _refresh_scale()
    # The app launch is the explicit user action; resource instantiation is not.
    _start.call_deferred()

func _start() -> void:
    if _closing: return
    terminal_view.open_session()
    terminal_view.focus_input()

func _process(_delta: float) -> void:
    if get_window().current_screen!=_screen_index: _refresh_scale()

func _refresh_scale() -> void:
    _screen_index=get_window().current_screen
    var value:=_scale_override
    if value<=0.0:
        value=1.0
        if DisplayServer.get_name()!="headless":
            if OS.get_name()=="macOS": value=DisplayServer.screen_get_scale(_screen_index)
            else: value=clampf(DisplayServer.screen_get_dpi(_screen_index)/96.0,1.0,3.0)
    _scale=clampf(value,.5,4.0)
    terminal_view.set_gui_scale(_scale)
    Style.set_backdrop_scale(panel,_scale)
    get_window().min_size=Vector2i(roundi(320*_scale),roundi(280*_scale))

func _input(event: InputEvent) -> void:
    # Native LineEdit owns ordinary text and OS IME composition. Control keys
    # have this single route; TerminalInput's optional soft-event route is unset.
    if _closing or terminal_view.menu.get_popup().visible: return
    if event is InputEventKey and terminal_view.handle_key(event):
        get_viewport().set_input_as_handled()

func _notification(what: int) -> void:
    if what==NOTIFICATION_WM_CLOSE_REQUEST: request_close()

func request_close() -> void:
    if _closing: return
    _closing=true
    if is_instance_valid(terminal_view): terminal_view.close_session()
    # Keep the tree alive for bounded native helper cleanup and waitpid polling.
    var deadline:=Time.get_ticks_msec()+4000
    while not get_tree().get_nodes_in_group("plyra_terminal_owned_helpers_v1").is_empty() and Time.get_ticks_msec()<deadline:
        await get_tree().process_frame
    get_tree().quit()

# FVPROV1 3146 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMDRhZDE5M2U0NzVkY2Y5NmFiYTBkYTA0YTQxOTk5NmYxNzM1MDU1ZWJkNWI5ZDU1MWM5MzkyODhjNmYyZTY1YiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNzIzYjMwNjQzYWIyZjQwZGRjZDk3ZTU4ZDAxNjMzNmU1MGU1ZDY4OGFlNWI5MGEyNGNjODZkZmNkNjA4NzYxZjVmODNiYmJkOGYwNzI4YzMwMTRlMjQzZjRjNjYwOTAzNGEwYjI2YzJhYmU4ZWI3MzY0OGY5YTcwOTFiNDg5MGEifV19XQ== FVPROV1-END
