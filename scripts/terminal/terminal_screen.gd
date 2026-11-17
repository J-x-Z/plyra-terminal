# SPDX-License-Identifier: GPL-3.0-only
extends RefCounted
## Bounded, display-only projection of libvterm helper events. No terminal escape
## sequence, URL, clipboard request or output string is executed by this model.
const MIN_COLS := 10
const MAX_COLS := 240
const MIN_ROWS := 4
const MAX_ROWS := 100
const MAX_HISTORY_LINES := 2000
const MAX_HISTORY_CELLS := 200000
const MAX_HISTORY_COST := 16777216
const DEFAULT_FG := Color("d8dee9")
const DEFAULT_BG := Color("10141c")
var cols := 80
var rows := 24
var lines: Array = []
var scrollback: Array = []
var cursor := {"row": 0, "col": 0, "visible": true}
var alternate := false
var application_cursor := false
var title := ""
var history_dropped := 0
var _history_cells := 0
var _history_cost := 0

func _init() -> void:
    reset()

func reset(columns: int = 80, row_count: int = 24) -> void:
    cols = clampi(columns, MIN_COLS, MAX_COLS)
    rows = clampi(row_count, MIN_ROWS, MAX_ROWS)
    lines = _blank_grid(rows)
    scrollback.clear()
    cursor = {"row": 0, "col": 0, "visible": true}
    alternate = false; application_cursor = false; title = ""
    history_dropped = 0; _history_cells = 0; _history_cost = 0

static func _integer(value: Variant, low: int, high: int) -> bool:
    return (value is int or value is float) and is_finite(float(value)) \
        and float(value) == floor(float(value)) and value >= low and value <= high

static func _blank_grid(count: int) -> Array:
    var result: Array = []
    for unused in range(count): result.append({"runs": []})
    return result

static func _color(value: Variant) -> bool:
    if not value is String or value.length() != 7 or not value.begins_with("#"): return false
    for character in value.substr(1):
        if not character.to_lower() in "0123456789abcdef": return false
    return true

static func _visible_text(value: Variant) -> bool:
    if not value is String or value.length() > 4096 or value.to_utf8_buffer().size() > 16384: return false
    for index in range(value.length()):
        var point: int = value.unicode_at(index)
        if point < 32 or point == 127: return false
    return true

static func _project_line(value: Variant, width: int) -> Dictionary:
    if not value is Dictionary or not value.get("runs") is Array or value.runs.size() > width: return {}
    var runs: Array = []
    var cells := 0
    var cost := 0
    for raw in value.runs:
        if not raw is Dictionary or not _visible_text(raw.get("text")) \
            or not _integer(raw.get("width"), 1, width) or not _color(raw.get("fg")) or not _color(raw.get("bg")): return {}
        for flag in ["bold", "italic", "underline", "inverse", "bg_default"]:
            if raw.has(flag) and not raw[flag] is bool: return {}
        cells += int(raw.width)
        if cells > width: return {}
        var text: String = raw.text
        # A cell can contain combining characters; width is authoritative from
        # libvterm. Reject impossible blank payloads, not valid wide graphemes.
        if text.is_empty(): return {}
        cost += text.to_utf8_buffer().size() + 128
        runs.append({"text": text, "width": int(raw.width), "fg": Color(raw.fg), "bg": Color(raw.bg),
            "bold": raw.get("bold", false), "italic": raw.get("italic", false),
            "underline": raw.get("underline", false), "inverse": raw.get("inverse", false),
            "bg_default": raw.get("bg_default", false)})
    return {"runs": runs, "cells": cells, "cost": cost}

func apply_screen(event: Dictionary) -> bool:
    if not _integer(event.get("cols"), MIN_COLS, MAX_COLS) or not _integer(event.get("rows"), MIN_ROWS, MAX_ROWS): return false
    var next_cols := int(event.cols)
    var next_rows := int(event.rows)
    var point: Variant = event.get("cursor")
    if not point is Dictionary or not _integer(point.get("row"), 0, next_rows - 1) \
        or not _integer(point.get("col"), 0, next_cols - 1) or not point.get("visible") is bool: return false
    if not event.get("alternate") is bool or not event.get("title") is String or event.title.length() > 512: return false
    if event.has("full") and not event.full is bool: return false
    if event.has("application_cursor") and not event.application_cursor is bool: return false
    if not event.get("lines") is Array or event.lines.size() > next_rows: return false
    var changed: Dictionary = {}
    for raw in event.lines:
        if not raw is Dictionary or not _integer(raw.get("row"), 0, next_rows - 1): return false
        var index := int(raw.row)
        if changed.has(index): return false
        var projected := _project_line(raw, next_cols)
        if projected.is_empty(): return false
        changed[index] = projected
    # The complete event is checked before mutating the visible projection.
    var replace: bool = event.get("full", false) or next_cols != cols or next_rows != rows
    if replace: lines = _blank_grid(next_rows)
    cols = next_cols; rows = next_rows
    for index in changed: lines[index] = {"runs": changed[index].runs}
    cursor = {"row": int(point.row), "col": int(point.col), "visible": point.visible}
    alternate = event.alternate
    application_cursor = event.get("application_cursor", application_cursor)
    title = _safe_title(event.title)
    return true

static func _safe_title(raw: String) -> String:
    var output := ""
    for index in range(raw.length()):
        var point := raw.unicode_at(index)
        if point >= 32 and point != 127: output += String.chr(point)
    return output

func append_scrollback(event: Dictionary) -> bool:
    if not event.get("lines") is Array or event.lines.size() > 128: return false
    if event.has("dropped") and not _integer(event.dropped, 0, 2147483647): return false
    var projected: Array = []
    for line in event.lines:
        var item := _project_line(line, MAX_COLS)
        if item.is_empty(): return false
        projected.append(item)
    history_dropped = mini(2147483647, history_dropped + int(event.get("dropped", 0)))
    for item in projected:
        scrollback.append(item)
        _history_cells += item.cells
        _history_cost += item.cost
    while scrollback.size() > MAX_HISTORY_LINES or _history_cells > MAX_HISTORY_CELLS or _history_cost > MAX_HISTORY_COST:
        var retired: Dictionary = scrollback.pop_front()
        _history_cells -= retired.cells; _history_cost -= retired.cost
        history_dropped = mini(2147483647, history_dropped + 1)
    return true

func clear_scrollback() -> void:
    scrollback.clear(); _history_cells = 0; _history_cost = 0; history_dropped = 0

func plain_text(include_history: bool = false) -> String:
    var output := PackedStringArray()
    if include_history:
        for line in scrollback: output.append(_line_text(line))
    for line in lines: output.append(_line_text(line))
    return "\n".join(output)

static func _line_text(line: Dictionary) -> String:
    var text := ""
    for run in line.runs: text += run.text
    return text.rstrip(" ")

# FVPROV1 6998 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMzU5MDE4NDUyMGVkNzM2YWJmY2U5NzJkMzE5NTcyMWVkNzVhNjk2Zjg5MGQ3MGE2Mzc4YjRmNmQ1OGE0YjMzNSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiM2RiYjA3MDE1NGUyYjNmNmI3NzAzMGVkNWFmYTkyODUzZTU4MjYyNWI0NDhhYTFmNzQwYWNmYjIyNTM5NmQ4NDIzOGE5YjhjNzc5MTI1MzAyZDI2MzNjYmRjNTZiMWE4ZjM3YTZhYjNkNGFiZTcyMWZiZTFiYjM5ODAxZmRlMGQifV19XQ== FVPROV1-END
