# SPDX-License-Identifier: GPL-3.0-only
extends Control
## Cell-aligned renderer. libvterm, not font measurement, owns terminal columns.
signal dimensions_changed(cols: int, rows: int)
signal input_requested
signal selection_changed
const GUI_TOUCH_DEVICE := -1007
## The embedding host owns conversion of native touch into GUI pointer events.
var host_touch_device := GUI_TOUCH_DEVICE
const MONO_FONT_URI := "res://assets/terminal-fonts/DejaVuSansMono.ttf"
const MONO_FONT_SHA256 := "b4a6c3e4faab8773f4ff761d56451646409f29abedd68f05d38c2df667d3c582"
var screen: RefCounted
var font_size := 16 # Logical size; menu changes remain 10..28 regardless of dp.
var gui_scale := 1.0
var cell_size := Vector2(10,22)
var history_offset := 0
var selection_start := Vector2i(-1,-1)
var selection_end := Vector2i(-1,-1)
var _fonts: Array[Font]=[]
var _mono_font: FontFile
var _cjk_fallback: SystemFont
var _ascent:=16.0
var _padding:=Vector2(8,6)
var _dragging:=false
var _touch_drag:=false
var _drag_origin:=Vector2.ZERO
var _drag_last:=Vector2.ZERO
var _touch_scrolled:=false
var _touch_rows:=0
var _last_dimensions:=Vector2i.ZERO

func _ready() -> void:
    focus_mode=Control.FOCUS_ALL
    clip_contents=true
    custom_minimum_size=Vector2(80,72)
    mouse_filter=Control.MOUSE_FILTER_STOP
    accessibility_name="终端输出"
    # Raw bytes avoid platform-dependent Godot font import products. ASCII can
    # never fall back to a proportional system face on a minimal Linux install.
    if FileAccess.get_sha256(MONO_FONT_URI)!=MONO_FONT_SHA256:
        push_error("The packaged terminal monospace font is absent or changed")
        return
    _mono_font=FontFile.new()
    _mono_font.data=FileAccess.get_file_as_bytes(MONO_FONT_URI)
    _mono_font.allow_system_fallback=false
    _cjk_fallback=SystemFont.new()
    _cjk_fallback.font_names=PackedStringArray(["Noto Sans CJK SC","Noto Sans SC","PingFang SC"])
    _cjk_fallback.allow_system_fallback=true
    _mono_font.fallbacks=[_cjk_fallback]
    for style in range(4):
        var font:=FontVariation.new()
        font.base_font=_mono_font
        font.variation_embolden=0.5 if style&1 else 0.0
        font.variation_transform=Transform2D(Vector2(1,0),Vector2(-0.18 if style&2 else 0.0,1),Vector2.ZERO)
        _fonts.append(font)
    resized.connect(_measure)
    _measure()

func set_font_size(value: int) -> void:
    font_size=clampi(value,10,28)
    if is_inside_tree(): _measure()

func set_gui_scale(value: float) -> void:
    if not is_finite(value): return
    var next:=clampf(value,.5,4.0)
    if is_equal_approx(gui_scale,next): return
    gui_scale=next
    if is_inside_tree(): _measure()

func effective_font_size() -> int:
    return maxi(1,roundi(font_size*gui_scale))

func _measure() -> void:
    if _fonts.is_empty(): return
    var pixels:=effective_font_size()
    _padding=Vector2(8,6)*gui_scale
    # The bundled face alone defines the one-cell advance. Fallback glyphs may
    # affect row ascent/height, never column count or libvterm's two-cell CJK.
    cell_size=Vector2(ceilf(_mono_font.get_char_size(48,pixels).x),
        ceilf(maxf(_mono_font.get_height(pixels),_cjk_fallback.get_height(pixels)))+ceilf(3*gui_scale))
    custom_minimum_size=Vector2(cell_size.x*10+_padding.x*2,cell_size.y*4+_padding.y*2)
    _ascent=ceilf(maxf(_mono_font.get_ascent(pixels),_cjk_fallback.get_ascent(pixels)))+ceilf(gui_scale)
    var dimensions:=Vector2i(clampi(int((size.x-_padding.x*2)/cell_size.x),10,240),clampi(int((size.y-_padding.y*2)/cell_size.y),4,100))
    if dimensions!=_last_dimensions:
        _last_dimensions=dimensions
        dimensions_changed.emit(dimensions.x,dimensions.y)
    queue_redraw()

func terminal_dimensions() -> Vector2i:
    return _last_dimensions if _last_dimensions!=Vector2i.ZERO else Vector2i(40,24)

func refresh() -> void:
    if screen!=null:
        history_offset=0 if screen.alternate else mini(history_offset,screen.scrollback.size())
    queue_redraw()

func scroll_history(delta: int) -> void:
    if screen==null or screen.alternate: return
    history_offset=clampi(history_offset+delta,0,screen.scrollback.size())
    selection_start=Vector2i(-1,-1);selection_end=selection_start
    selection_changed.emit();queue_redraw()

func follow_output() -> void:
    history_offset=0
    selection_start=Vector2i(-1,-1);selection_end=selection_start
    selection_changed.emit();queue_redraw()

func _line_at(row: int) -> Dictionary:
    if screen==null: return {"runs":[]}
    var index: int=screen.scrollback.size()-history_offset+row
    if index<0: return {"runs":[]}
    if index<screen.scrollback.size(): return screen.scrollback[index]
    index-=screen.scrollback.size()
    return screen.lines[index] if index<screen.lines.size() else {"runs":[]}

func _draw() -> void:
    # Default background transmits the separate blurred optical surface. An
    # application's explicit ANSI background/inverse cells still draw intact.
    draw_rect(Rect2(Vector2.ZERO,size),Color(.02,.035,.05,.04))
    if screen==null or _fonts.is_empty(): return
    for row in range(screen.rows):
        var line:=_line_at(row)
        var column:=0
        var y:=_padding.y+row*cell_size.y
        for run in line.runs:
            var extent: float=float(run.width)*cell_size.x
            var foreground: Color=run.bg if run.inverse else run.fg
            var background: Color=run.fg if run.inverse else run.bg
            var rect:=Rect2(Vector2(_padding.x+column*cell_size.x,y),Vector2(extent,cell_size.y))
            if _opaque_background(run): draw_rect(rect,background)
            var style: int=(1 if run.bold else 0)|(2 if run.italic else 0)
            if _ascii_cells(run.text,int(run.width)):
                # The measured cell advance is rounded; drawing a merged font
                # string would accumulate its fractional advance across columns.
                for offset in range(run.text.length()):
                    draw_string(_fonts[style],Vector2(rect.position.x+offset*cell_size.x,y+_ascent),run.text.substr(offset,1),HORIZONTAL_ALIGNMENT_LEFT,-1,effective_font_size(),foreground)
            else:
                # A fallback is centered within the cells owned by libvterm.
                # A rare oversized glyph is fitted locally, not allowed to move
                # the following column or redefine the monospace grid.
                var pixels:=effective_font_size()
                var advance:=_fonts[style].get_string_size(run.text,HORIZONTAL_ALIGNMENT_LEFT,-1,pixels).x
                if advance>extent:
                    pixels=maxi(1,floori(pixels*extent/advance))
                    advance=_fonts[style].get_string_size(run.text,HORIZONTAL_ALIGNMENT_LEFT,-1,pixels).x
                draw_string(_fonts[style],Vector2(rect.position.x+maxf(0,(extent-advance)*.5),y+_ascent),run.text,HORIZONTAL_ALIGNMENT_LEFT,-1,pixels,foreground)
            if run.underline: draw_line(Vector2(rect.position.x,y+cell_size.y-2*gui_scale),Vector2(rect.end.x,y+cell_size.y-2*gui_scale),foreground,gui_scale)
            column+=int(run.width)
    if selection_start.x>=0:
        var first:=selection_start;var last:=selection_end
        if first.y>last.y or (first.y==last.y and first.x>last.x): first=selection_end;last=selection_start
        for row in range(first.y,last.y+1):
            var left:=first.x if row==first.y else 0
            var right: int=last.x if row==last.y else screen.cols-1
            draw_rect(Rect2(Vector2(_padding.x+left*cell_size.x,_padding.y+row*cell_size.y),Vector2((right-left+1)*cell_size.x,cell_size.y)),Color(.35,.65,.88,.35))
    if history_offset==0 and screen.cursor.visible:
        draw_rect(Rect2(Vector2(_padding.x+int(screen.cursor.col)*cell_size.x,_padding.y+int(screen.cursor.row)*cell_size.y),Vector2(2*gui_scale,cell_size.y)),Color("8bd5ff"))
    if history_offset>0:
        draw_rect(Rect2(Vector2(size.x-3*gui_scale,_padding.y),Vector2(2*gui_scale,maxf(6*gui_scale,size.y*.10))),Color("80b5cf"))

func _cell(at: Vector2) -> Vector2i:
    if screen==null: return Vector2i.ZERO
    return Vector2i(clampi(int((at.x-_padding.x)/cell_size.x),0,screen.cols-1),clampi(int((at.y-_padding.y)/cell_size.y),0,screen.rows-1))

static func _opaque_background(run: Dictionary) -> bool:
    # libvterm's semantic flag survives RGB conversion. Explicit ANSI colors
    # stay opaque even when they equal the default color numerically. Older
    # protocol producers without a flag keep the conservative opaque behavior.
    return not bool(run.get("bg_default",false)) or bool(run.get("inverse",false))

static func _ascii_cells(text: String,width: int) -> bool:
    if text.length()!=width: return false
    for index in range(text.length()):
        var point:=text.unicode_at(index)
        if point<32 or point>126: return false
    return true

func selected_text() -> String:
    if selection_start.x<0 or screen==null: return ""
    var first:=selection_start;var last:=selection_end
    if first.y>last.y or (first.y==last.y and first.x>last.x): first=selection_end;last=selection_start
    var output:=PackedStringArray()
    for row in range(first.y,last.y+1):
        var left:=first.x if row==first.y else 0
        var right: int=last.x if row==last.y else screen.cols-1
        var value:="";var column:=0
        for run in _line_at(row).runs:
            # ASCII merges are one column each; every other run is an exact
            # single libvterm grapheme, including its combining/wide suffixes.
            if _ascii_cells(run.text,int(run.width)):
                var start:=maxi(0,left-column);var end:=mini(int(run.width),right-column+1)
                if end>start: value+=run.text.substr(start,end-start)
            elif column<=right and column+int(run.width)>left: value+=run.text
            column+=int(run.width)
        output.append(value.rstrip(" "))
    return "\n".join(output)

func select_all_visible() -> void:
    if screen==null: return
    selection_start=Vector2i.ZERO;selection_end=Vector2i(screen.cols-1,screen.rows-1)
    selection_changed.emit();queue_redraw()

func _gui_input(event: InputEvent) -> void:
    if event is InputEventMouseButton:
        if event.pressed and event.button_index in [MOUSE_BUTTON_WHEEL_UP,MOUSE_BUTTON_WHEEL_DOWN]:
            scroll_history(3 if event.button_index==MOUSE_BUTTON_WHEEL_UP else -3);accept_event()
        elif event.button_index==MOUSE_BUTTON_LEFT:
            if event.pressed:
                _dragging=true;_touch_drag=event.device==host_touch_device;_touch_scrolled=false
                _touch_rows=0
                _drag_origin=event.position;_drag_last=event.position
                if not _touch_drag:
                    grab_focus();selection_start=_cell(event.position);selection_end=selection_start
            else:
                var inside: bool=event.position.is_finite() and Rect2(Vector2.ZERO,size).has_point(event.position)
                if _dragging and _touch_drag and inside: _touch_motion(event.position)
                if _dragging and inside and ((_touch_drag and not _touch_scrolled and event.position.distance_to(_drag_origin)<=12*gui_scale) or (not _touch_drag and event.position.distance_to(_drag_origin)<=6*gui_scale)): input_requested.emit()
                _dragging=false
                _touch_rows=0
            selection_changed.emit();queue_redraw();accept_event()
    elif event is InputEventMouseMotion and _dragging:
        if _touch_drag:
            _touch_motion(event.position)
        else: selection_end=_cell(event.position);selection_changed.emit();queue_redraw()
        accept_event()

func _touch_motion(position: Vector2) -> void:
    # Router retires native touch capture by moving outside before sending Up.
    # Treat that as cancellation, never as a huge scroll to the newest output.
    if not position.is_finite() or not Rect2(Vector2.ZERO,size).has_point(position):
        _dragging=false;_touch_rows=0
        return
    _drag_last=position
    var pixels: float=position.y-_drag_origin.y
    if absf(pixels)>12*gui_scale: _touch_scrolled=true
    if not _touch_scrolled: return
    # Quantize the whole captured displacement, retaining fractional rows and
    # reversals across delivery packets. Up also settles a coalesced endpoint.
    var rows:=int(pixels/cell_size.y)
    var lines:=rows-_touch_rows
    _touch_rows=rows
    if lines!=0: scroll_history(lines)

# FVPROV1 12406 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYmYzOTNlZDIxOThhYjVmYmY5NTk1YjI5MzFkNjA0NGMwZmU0N2JhYTBmMzc2YjgzM2U0NTk4MmU1NmNmNjBmMyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZGNmMWZhZjQ2Nzk0M2I3OTNkYmRiMzhlYTY2NWUwM2UzNTE2ZmFlY2JkZDZlNGIyOTRiNDBkMDMxY2ViMTA1MWM5MGEzNDFlNjdhMjY2NTgxODU2NzNlMzIwOWU0MTY3ZGIzNTFmYTM5MjBkMDFiZGYwY2NhOTJjZjA5ZDUxMDAifV19XQ== FVPROV1-END
