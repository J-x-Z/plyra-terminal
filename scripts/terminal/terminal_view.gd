# SPDX-License-Identifier: GPL-3.0-only
extends VBoxContainer
const Session = preload("res://scripts/terminal/terminal_session.gd")
const Canvas = preload("res://scripts/terminal/terminal_canvas.gd")
const InputField = preload("res://scripts/terminal/terminal_input.gd")
const Style = preload("res://scripts/terminal/terminal_style.gd")
var terminal: Node
var canvas: Control
var input_field: LineEdit
var input_guard: Callable
var composition_busy: Callable
## Optional host adapters. Empty values use this module's theme and native keys.
var host_theme_factory: Callable
var host_soft_key_guard: Callable
var host_touch_device := Canvas.GUI_TOUCH_DEVICE
var heading: Label
var launch_button: Button
var menu: MenuButton
var controls: HBoxContainer
var ctrl_button: Button
var alt_button: Button
var enter_button: Button
var gui_scale:=1.0
var _header: HBoxContainer
var _shortcut_scroll: ScrollContainer
var _buttons: Array[Button]=[]
var _bell_until:=0

func _ready() -> void:
    theme=_theme_for_host()
    add_theme_constant_override("separation",0)
    size_flags_horizontal=Control.SIZE_EXPAND_FILL
    size_flags_vertical=Control.SIZE_EXPAND_FILL
    terminal=Session.new();terminal.name="PTYSession";add_child(terminal)
    _header=HBoxContainer.new();add_child(_header)
    heading=Label.new();heading.text="终端 · 本机 Shell";heading.size_flags_horizontal=Control.SIZE_EXPAND_FILL
    heading.clip_text=true;heading.text_overrun_behavior=TextServer.OVERRUN_TRIM_ELLIPSIS
    _header.add_child(heading)
    launch_button=_button("启动",_header)
    launch_button.size_flags_horizontal=Control.SIZE_SHRINK_CENTER
    launch_button.accessibility_name="启动本机终端会话"
    launch_button.pressed.connect(open_session)
    menu=MenuButton.new();menu.text="...";menu.focus_mode=Control.FOCUS_NONE
    menu.accessibility_name="终端菜单";_header.add_child(menu);_buttons.append(menu)
    var popup:=menu.get_popup()
    for label in ["复制选区 / 当前屏幕","粘贴","选中当前屏幕","增大字号","减小字号","回到最新输出","清空滚动历史","结束会话"]: popup.add_item(label)
    popup.id_pressed.connect(_menu_action)
    canvas=Canvas.new();canvas.host_touch_device=host_touch_device;canvas.name="TerminalOutput";canvas.screen=terminal.screen;canvas.size_flags_vertical=Control.SIZE_EXPAND_FILL
    add_child(canvas)
    canvas.dimensions_changed.connect(func(cols: int,rows: int): terminal.resize_terminal(cols,rows))
    canvas.input_requested.connect(focus_input)
    _shortcut_scroll=ScrollContainer.new()
    _shortcut_scroll.vertical_scroll_mode=ScrollContainer.SCROLL_MODE_DISABLED
    _shortcut_scroll.horizontal_scroll_mode=ScrollContainer.SCROLL_MODE_AUTO
    add_child(_shortcut_scroll)
    controls=HBoxContainer.new();controls.add_theme_constant_override("separation",0);controls.size_flags_horizontal=Control.SIZE_EXPAND_FILL
    _shortcut_scroll.add_child(controls)
    _special("Esc","Escape")
    ctrl_button=_button("Ctrl",controls);ctrl_button.toggle_mode=true;ctrl_button.toggled.connect(func(_pressed: bool): focus_input())
    alt_button=_button("Alt",controls);alt_button.toggle_mode=true;alt_button.toggled.connect(func(_pressed: bool): focus_input())
    for pair in [["Tab","Tab"],["←","Left"],["↓","Down"],["↑","Up"],["→","Right"]]: _special(pair[0],pair[1])
    var entry:=HBoxContainer.new();entry.add_theme_constant_override("separation",0);add_child(entry)
    input_field=InputField.new();input_field.name="TerminalInput";input_field.custom_minimum_size.y=44
    input_field.size_flags_horizontal=Control.SIZE_EXPAND_FILL;input_field.input_allowed=Callable(self,"_can_input")
    input_field.soft_control_key=Callable(self,"handle_key")
    input_field.soft_key_guard=host_soft_key_guard
    entry.add_child(input_field);input_field.committed.connect(_committed)
    enter_button=_button("回车",entry);enter_button.name="TerminalEnter";enter_button.size_flags_horizontal=Control.SIZE_SHRINK_CENTER
    enter_button.accessibility_name="终端回车";enter_button.tooltip_text="Enter · 执行当前命令"
    enter_button.pressed.connect(func(): _named("Enter"))
    terminal.screen_changed.connect(_screen_changed)
    terminal.status_changed.connect(_status_changed)
    terminal.bell.connect(func(): _bell_until=Time.get_ticks_msec()+250;heading.text="终端 · 响铃")
    _apply_gui_scale()
    _status_changed()

func _theme_for_host() -> Theme:
    if host_theme_factory.is_valid():
        var supplied: Variant=host_theme_factory.call(gui_scale)
        if supplied is Theme: return supplied
    return Style.theme(gui_scale)

func set_gui_scale(value: float) -> void:
    if not is_finite(value): return
    var next:=clampf(value,.5,4.0)
    if is_equal_approx(gui_scale,next): return
    gui_scale=next
    if is_instance_valid(canvas): _apply_gui_scale()

func _apply_gui_scale() -> void:
    # Rebuild from logical constants, never multiply already-scaled state.
    # The AppView owns only its outer frame; it does not scale this subtree.
    theme=_theme_for_host()
    _header.add_theme_constant_override("separation",roundi(4*gui_scale))
    _header.custom_minimum_size.y=44*gui_scale
    heading.add_theme_font_size_override("font_size",roundi(14*gui_scale))
    for button in _buttons:
        button.custom_minimum_size=Vector2(44,44)*gui_scale
        button.add_theme_font_size_override("font_size",roundi(14*gui_scale))
    enter_button.custom_minimum_size.x=64*gui_scale
    # Reserve a scrollbar band without shrinking the actual 44dp touch row.
    _shortcut_scroll.custom_minimum_size.y=56*gui_scale
    input_field.custom_minimum_size.y=44*gui_scale
    input_field.add_theme_font_size_override("font_size",roundi(16*gui_scale))
    canvas.set_gui_scale(gui_scale)

func _button(text: String,parent: Node) -> Button:
    var button:=Button.new();button.text=text;button.custom_minimum_size=Vector2(44,44)*gui_scale
    button.size_flags_horizontal=Control.SIZE_EXPAND_FILL;button.focus_mode=Control.FOCUS_NONE
    button.add_theme_font_size_override("font_size",roundi(14*gui_scale));parent.add_child(button);_buttons.append(button);return button

func _special(label: String,key: String) -> void:
    var button:=_button(label,controls)
    button.accessibility_name="终端 "+key
    button.pressed.connect(func(): _named(key))

func _live() -> bool:
    if not is_inside_tree() or is_queued_for_deletion() or not is_visible_in_tree(): return false
    var ancestor:=get_parent()
    while ancestor!=null:
        if ancestor.is_queued_for_deletion(): return false
        ancestor=ancestor.get_parent()
    return not input_guard.is_valid() or bool(input_guard.call())

func _can_input() -> bool:
    return _live() and terminal!=null and terminal.running

func _composing() -> bool:
    return input_field.has_ime_text() or (composition_busy.is_valid() and bool(composition_busy.call()))

func open_session() -> void:
    # Restoring a layout creates an inert view. Only an explicit tile/button
    # activation calls this method and starts a process.
    if not _live() or terminal.running or terminal.status=="starting": return
    var dimensions: Vector2i=canvas.terminal_dimensions()
    if terminal.start_session(dimensions.x,dimensions.y): canvas.follow_output()

func focus_input() -> void:
    if _live(): input_field.grab_focus()

func _committed(text: String) -> void:
    if not _can_input(): return
    canvas.follow_output()
    var value:=text
    if ctrl_button.button_pressed and value.length()==1:
        var code:=value.to_upper().unicode_at(0)
        if code>=65 and code<=90: value=String.chr(code-64)
        elif code==32 or code==64:
            terminal.write_input(PackedByteArray([27,0]) if alt_button.button_pressed else PackedByteArray([0]))
            _clear_modifiers();return
    if alt_button.button_pressed: value="\u001b"+value
    if not terminal.write_input(value.to_utf8_buffer()): heading.text="终端输入队列已满，请重试"
    _clear_modifiers()

func _clear_modifiers() -> void:
    ctrl_button.set_pressed_no_signal(false);alt_button.set_pressed_no_signal(false)

func _named(name: String) -> void:
    if not _can_input() or _composing(): return
    input_field.flush_committed()
    if not _can_input(): return
    var modifiers: int=(4 if ctrl_button.button_pressed else 0)|(2 if alt_button.button_pressed else 0)
    terminal.send_named_key(name,modifiers);_clear_modifiers();canvas.follow_output()

func handle_key(event: InputEventKey, composing: bool = false) -> bool:
    if not _can_input() or not event.pressed or composing or _composing(): return false
    var owner:=get_viewport().gui_get_focus_owner()
    if owner!=input_field and owner!=canvas: return false
    var key: int=event.keycode if event.keycode!=KEY_NONE else event.physical_keycode
    var clipboard_shortcut: bool=event.meta_pressed or (event.ctrl_pressed and event.shift_pressed)
    if clipboard_shortcut and key==KEY_C: copy_text();return true
    if clipboard_shortcut and key==KEY_V: paste_clipboard();return true
    if clipboard_shortcut and key==KEY_A: canvas.select_all_visible();return true
    if event.shift_pressed and not event.ctrl_pressed and key in [KEY_PAGEUP,KEY_PAGEDOWN]:
        canvas.scroll_history((1 if key==KEY_PAGEUP else -1)*maxi(1,terminal.screen.rows-1));return true
    var named: String=Session.key_name(event)
    # Ordinary text goes through the native LineEdit and the existing Chinese
    # composition controller. Physical terminal control keys bypass editing.
    if owner==input_field and named.is_empty() and not event.ctrl_pressed and not event.alt_pressed and not event.meta_pressed and not ctrl_button.button_pressed and not alt_button.button_pressed: return false
    input_field.flush_committed()
    if not _can_input() or get_viewport().gui_get_focus_owner()!=owner: return false
    var effective: InputEventKey=event.duplicate()
    effective.ctrl_pressed=event.ctrl_pressed or ctrl_button.button_pressed
    effective.alt_pressed=event.alt_pressed or alt_button.button_pressed
    if terminal.send_key(effective): _clear_modifiers();canvas.follow_output();return true
    return false

func copy_text() -> void:
    if not _live(): return
    var text: String=canvas.selected_text()
    if text.is_empty(): text=terminal.screen.plain_text(false).strip_edges(false,true)
    if not text.is_empty() and DisplayServer.has_feature(DisplayServer.FEATURE_CLIPBOARD): DisplayServer.clipboard_set(text)

func paste_clipboard() -> void:
    if not _can_input() or _composing() or not DisplayServer.has_feature(DisplayServer.FEATURE_CLIPBOARD): return
    input_field.flush_committed()
    if not terminal.paste_text(DisplayServer.clipboard_get()): heading.text="粘贴内容为空、过长或输入队列已满"
    canvas.follow_output()

func _menu_action(id: int) -> void:
    if not _live(): return
    match id:
        0: copy_text()
        1: paste_clipboard()
        2: canvas.select_all_visible()
        3: canvas.set_font_size(canvas.font_size+1)
        4: canvas.set_font_size(canvas.font_size-1)
        5: canvas.follow_output()
        6: terminal.screen.clear_scrollback();canvas.follow_output()
        7: input_field.discard_pending();terminal.close_session()

func _screen_changed() -> void:
    canvas.refresh();_status_changed()

func _status_changed() -> void:
    if not is_instance_valid(heading): return
    var dimensions: String="%d×%d" % [terminal.screen.cols,terminal.screen.rows]
    if terminal.running:
        var caption: String=terminal.screen.title if not terminal.screen.title.is_empty() else terminal.shell_name.get_file()
        heading.text=caption+" · "+dimensions
    else: heading.text=terminal.status_message
    heading.tooltip_text=heading.text
    launch_button.disabled=terminal.running or terminal.status=="starting"
    launch_button.text="运行中" if terminal.running else ("启动" if terminal.status=="idle" else "重开")

func close_session() -> void:
    if is_instance_valid(input_field): input_field.discard_pending()
    if is_instance_valid(terminal): terminal.close_session()

func _exit_tree() -> void:
    close_session()

# FVPROV1 12198 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZGExMWQyNTMzNTVhOTE4ZGNlZjM1ODhjZjNjMDhkZjM1NjM3NGRkNGRlZmE0ODA3MTFmYmNjZTc3Mjg4ZTBkZCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOWYwMTU2NWYwNDE0ZjYzM2I3YTdjMTdhNTk5NDI4NDczODBmZmIzZmU5N2FiMGI3YzczNWQ3MDYxOGZiMmQ0ZDcyYjY4ZTczYjk3OTVkOWE4ZGFhMGIyYjc1ZWJiZWVmNjQyNzU5OTkyN2I0MjE4YmY4NTg0MjUwODM4N2NkMGQifV19XQ== FVPROV1-END
