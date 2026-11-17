# SPDX-License-Identifier: GPL-3.0-only
extends RefCounted
## Terminal-local dark controls; no Shell, preferences or IME dependencies.
const ACRYLIC=preload("res://shaders/terminal/acrylic.gdshader")
const INK=Color("f5f5f5")
const HOLDER_NAME="TerminalAcrylicSurface"

static func box(color: Color,radius: int=4,border: Color=Color.TRANSPARENT,inset: int=8,gui_scale: float=1.0) -> StyleBoxFlat:
    var dp:=clampf(gui_scale,.5,4.0)
    var style:=StyleBoxFlat.new();style.bg_color=color;style.set_corner_radius_all(roundi(radius*dp))
    style.set_border_width_all(maxi(1,roundi(dp)));style.border_color=border
    for side in ["left","right","top","bottom"]: style.set("content_margin_"+side,roundi(inset*dp))
    return style

static func theme(gui_scale: float=1.0,reduce_transparency: bool=false) -> Theme:
    var result:=Theme.new()
    var dp:=clampf(gui_scale,.5,4.0)
    result.default_font_size=maxi(1,roundi(14*dp))
    var font:=SystemFont.new()
    font.font_names=PackedStringArray(["Noto Sans CJK SC","Noto Sans SC","PingFang SC","sans-serif"])
    result.default_font=font
    for kind in ["Label","Button","MenuButton","LineEdit","PopupMenu"]:
        result.set_color("font_color",kind,INK)
        result.set_color("font_hover_color",kind,INK)
        result.set_color("font_pressed_color",kind,INK)
        result.set_color("font_focus_color",kind,INK)
        result.set_color("font_disabled_color",kind,Color("999999"))
    for kind in ["Button","MenuButton"]:
        result.set_stylebox("normal",kind,box(Color(1,1,1,.06),4,Color(1,1,1,.12),8,dp))
        result.set_stylebox("hover",kind,box(Color(1,1,1,.13),4,Color(1,1,1,.24),8,dp))
        result.set_stylebox("pressed",kind,box(Color(1,1,1,.19),4,Color(1,1,1,.35),8,dp))
        result.set_stylebox("disabled",kind,box(Color(1,1,1,.03),4,Color(1,1,1,.08),8,dp))
        result.set_stylebox("focus",kind,box(Color.TRANSPARENT,4,Color("a8d6ef"),8,dp))
    result.set_stylebox("normal","LineEdit",box(Color(0,0,0,.18),4,Color(1,1,1,.22),8,dp))
    result.set_stylebox("focus","LineEdit",box(Color.TRANSPARENT,4,Color("a8d6ef"),8,dp))
    result.set_color("font_placeholder_color","LineEdit",Color("b8b8b8"))
    result.set_color("caret_color","LineEdit",INK)
    result.set_stylebox("panel","PopupMenu",box(Color(.125,.125,.125,1.0 if reduce_transparency else .92),6,Color(1,1,1,.25),8,dp))
    result.set_stylebox("hover","PopupMenu",box(Color(1,1,1,.14),3,Color.TRANSPARENT,6,dp))
    return result

static func install_backdrop(panel: PanelContainer,gui_scale: float=1.0,reduce_transparency: bool=false) -> ColorRect:
    var holder:=panel.get_node_or_null(HOLDER_NAME) as Node2D
    if holder==null:
        holder=Node2D.new();holder.name=HOLDER_NAME
        panel.add_child(holder,false,Node.INTERNAL_MODE_FRONT)
        var copy:=BackBufferCopy.new();copy.name="BackdropCopy";copy.copy_mode=BackBufferCopy.COPY_MODE_VIEWPORT
        holder.add_child(copy)
        var backdrop:=ColorRect.new();backdrop.name="AcrylicBackdrop";backdrop.mouse_filter=Control.MOUSE_FILTER_IGNORE
        var material:=ShaderMaterial.new();material.shader=ACRYLIC;backdrop.material=material
        holder.add_child(backdrop)
        panel.resized.connect(func(): _resize(panel))
    panel.set_meta("terminal_reduced_transparency",reduce_transparency)
    set_backdrop_scale(panel,gui_scale)
    return holder.get_node("AcrylicBackdrop") as ColorRect

static func set_backdrop_scale(panel: PanelContainer,gui_scale: float) -> void:
    var holder:=panel.get_node_or_null(HOLDER_NAME) as Node2D
    if holder==null: return
    var reduced: bool=panel.get_meta("terminal_reduced_transparency",false)
    panel.add_theme_stylebox_override("panel",box(Color("202020") if reduced else Color.TRANSPARENT,12,Color(1,1,1,.25),8,gui_scale))
    var copy:=holder.get_node("BackdropCopy") as BackBufferCopy
    copy.copy_mode=BackBufferCopy.COPY_MODE_DISABLED if reduced else BackBufferCopy.COPY_MODE_VIEWPORT
    var backdrop:=holder.get_node("AcrylicBackdrop") as ColorRect
    backdrop.visible=not reduced
    var material:=backdrop.material as ShaderMaterial
    material.set_shader_parameter("base_color",Color("202020"))
    material.set_shader_parameter("background_transmission",.35)
    material.set_shader_parameter("corner_radius",12*gui_scale)
    _resize(panel)

static func _resize(panel: Control) -> void:
    var backdrop:=panel.get_node_or_null(HOLDER_NAME+"/AcrylicBackdrop") as ColorRect
    if backdrop==null: return
    backdrop.position=Vector2.ZERO;backdrop.size=panel.size
    (backdrop.material as ShaderMaterial).set_shader_parameter("surface_size",panel.size)

# FVPROV1 4640 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNTM5ODZiMjEyNGMyYWU1YmExYjFkMWE1MjhhMDVlMWI4M2VlZDU2Yjg5ZjJlOGFjZjM0YjRjMzU3YWM1M2QwYyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMDA5YWIzM2E1ZGU1OTM1NjFmMDVkYzk1MTEwOTNkMWY3YmI4NmFhZjMwNzIzZTE1YWNkMTA4ZjdlOTVkYjc5YjZiNTg3ZDhmODg5YWRlNzljYjdhZmRjNmZhMzE0Yzk0YTMxMzA5NDk5YmNkNjEwY2E2YzliNWFmZGZiYWZkMDkifV19XQ== FVPROV1-END
