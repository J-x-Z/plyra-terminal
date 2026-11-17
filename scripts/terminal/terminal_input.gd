# SPDX-License-Identifier: GPL-3.0-only
extends LineEdit
## Native IME/keyboard composition sink. Only committed text leaves this field;
## command history and passwords belong to the PTY, never a saved UI document.
signal committed(text: String)
var input_allowed: Callable
var soft_control_key: Callable
## Host authorizes only its own nested soft control events.
var soft_key_guard: Callable
var _draining := false

func _ready() -> void:
    max_length=1024
    placeholder_text="轻触输入"
    accessibility_name="终端输入"
    context_menu_enabled=false
    select_all_on_focus=false
    text_changed.connect(_text_changed)

func _gui_input(event: InputEvent) -> void:
    if not event is InputEventKey or not event.pressed or event.echo: return
    if event.keycode not in [KEY_ENTER,KEY_BACKSPACE,KEY_DELETE,KEY_LEFT,KEY_RIGHT]: return
    if not has_focus() or not is_visible_in_tree() or is_queued_for_deletion(): return
    if input_allowed.is_valid() and not input_allowed.call(): return
    if not soft_key_guard.is_valid() or not bool(soft_key_guard.call(event)) or not soft_control_key.is_valid(): return
    # Ordinary hardware keys already pass through the host's single handle_key route. Only the
    # broker's nested soft control-key event needs this terminal-specific route.
    if soft_control_key.call(event): accept_event()

func _text_changed(_text: String) -> void:
    if not _draining:
        _draining=true
        _drain.call_deferred()

func _drain() -> void:
    _draining=false
    if not is_inside_tree() or is_queued_for_deletion(): return
    if has_ime_text(): return
    var value:=text
    if value.is_empty(): return
    clear()
    if has_focus() and is_visible_in_tree() and (not input_allowed.is_valid() or input_allowed.call()):
        committed.emit(value)

func discard_pending() -> void:
    cancel_ime()
    clear()

func flush_committed() -> void:
    _drain()

# FVPROV1 1919 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZDkzOTg2NDhlMDQzMzk5NmJkMjFhZjFhYWI1NWI0ZDA1Y2VmNTgwZTA5MjZhZDZhNTBlYzhmZTlhOTdhMzE2OSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZDE5NDJlYjJiNzRjY2ZkYjMxYTllZjM1NDUxMzY0ZTJkNzQzZmRiNWZjMmYxY2FkNmRiMjk0ZjJkMjczNzJjZjVlNzhlMGZkZWYyNzE5MTZhZTU2MTdiZjRjZTRjZmFiMzgzZjViNzE5Y2E3NmM0N2Y0Njk5NmFkMmYyMTVlMDYifV19XQ== FVPROV1-END
