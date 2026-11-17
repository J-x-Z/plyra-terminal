# SPDX-License-Identifier: GPL-3.0-only
extends SceneTree
const Screen = preload("res://scripts/terminal/terminal_screen.gd")
var checks := 0
var failures := 0
func check(value: bool, id: String) -> void:
    checks += 1
    if not value: failures += 1; push_error(id)
func run(text: String, width: int = -1) -> Dictionary:
    return {"text": text, "width": text.length() if width < 0 else width, "fg": "#d8dee9", "bg": "#11151b", "bold": false, "italic": false, "underline": false, "inverse": false}
func event(lines: Array, full: bool = true, cols: int = 80, rows: int = 24) -> Dictionary:
    return {"type": "screen", "cols": cols, "rows": rows, "cursor": {"row": 0, "col": 0, "visible": true}, "alternate": false, "title": "Terminal", "full": full, "lines": lines}
func _initialize() -> void:
    var model := Screen.new()
    check(model.lines.size() == 24 and model.cols == 80, "initial dimensions")
    check(model.apply_screen(event([{"row": 0, "runs": [run("hello")]}])), "ASCII screen")
    check(model.plain_text().begins_with("hello\n"), "display plain text")
    var semantic := run("default"); semantic.bg = "#10141c"; semantic.bg_default = true
    check(model.apply_screen(event([{"row": 2, "runs": [semantic]}], false)) and model.lines[2].runs[0].bg_default, "semantic default background preserved")
    var explicit := run("explicit"); explicit.bg = "#10141c"; explicit.bg_default = false
    check(model.apply_screen(event([{"row": 3, "runs": [explicit]}], false)) and not model.lines[3].runs[0].bg_default, "same RGB explicit ANSI remains distinct")
    var bad_flag := run("bad"); bad_flag.bg_default = 1
    check(not model.apply_screen(event([{"row": 4, "runs": [bad_flag]}], false)), "default-background marker requires actual bool")
    check(not model.lines[0].runs[0].bg_default, "missing background marker is conservatively opaque")
    check(model.lines[0].runs[0].fg is Color and model.lines[0].runs[0].fg == Color("d8dee9"), "colors projected not interpreted as markup")
    check(model.apply_screen(event([{"row": 1, "runs": [run("你好", 4), run("é", 1)]}], false)), "wide and combining text")
    check(model.lines[0].runs[0].text == "hello" and model.lines[1].runs.size() == 2, "dirty update retains untouched row")
    var before := model.plain_text()
    var bad := event([{"row": 0, "runs": [run("injected")]}, {"row": 1, "runs": [run("x", 81)]}], false)
    check(not model.apply_screen(bad) and model.plain_text() == before, "malformed later row cannot partially mutate")
    check(not model.apply_screen(event([{"row": 24, "runs": []}])), "out of range row")
    check(not model.apply_screen(event([{"row": 0, "runs": []}, {"row": 0, "runs": []}])), "duplicate row rejected")
    check(not model.apply_screen(event([], true, 100000000, 24)), "unbounded columns rejected")
    check(not model.apply_screen(event([], true, 80, 100000000)), "unbounded rows rejected")
    var invalid_cursor := event([]); invalid_cursor.cursor.col = 80
    check(not model.apply_screen(invalid_cursor), "cursor bounded")
    var invalid_bool := event([]); invalid_bool.alternate = 1
    check(not model.apply_screen(invalid_bool), "booleans strict")
    var invalid_color := run("a"); invalid_color.fg = "red"
    check(not model.apply_screen(event([{"row": 0, "runs": [invalid_color]}])), "color syntax bounded")
    check(not model.apply_screen(event([{"row": 0, "runs": [run("\u001b]52;clipboard")]}])), "escape payload cannot reach display run")
    var titled := event([]); titled.title = "A\u0007B\nC"
    check(model.apply_screen(titled) and model.title == "ABC", "title controls removed")
    var alternate := event([{"row": 0, "runs": [run("alternate")]}]); alternate.alternate = true
    check(model.apply_screen(alternate) and model.alternate, "alternate visible buffer")
    check(model.lines[1].runs.is_empty(), "full resets previous rows")
    check(model.append_scrollback({"lines": [{"runs": [run("earlier")]}], "dropped": 2}), "history append")
    check(model.scrollback.size() == 1 and model.history_dropped == 2 and model.plain_text(true).begins_with("earlier\n"), "history independently visible")
    check(not model.append_scrollback({"lines": [{"runs": [run("valid")]}, {"runs": [run("bad", 241)]}]}) and model.scrollback.size() == 1, "history append atomic validation")
    model.clear_scrollback()
    for batch in range(21):
        var history: Array = []
        for row in range(100): history.append({"runs": [run("x")]})
        check(model.append_scrollback({"lines": history}), "bounded history batch %d" % batch)
    check(model.scrollback.size() == 2000 and model.history_dropped == 100, "history line budget evicts oldest")
    model.clear_scrollback()
    for batch in range(10):
        var wide_history: Array = []
        for row in range(100): wide_history.append({"runs": [run("x".repeat(240))]})
        check(model.append_scrollback({"lines": wide_history}), "wide history batch %d" % batch)
    check(model._history_cells <= Screen.MAX_HISTORY_CELLS and model.scrollback.size() < 1000, "history cell budget")
    model.clear_scrollback()
    check(model.scrollback.is_empty() and model.history_dropped == 0 and model._history_cells == 0, "clear scrollback")
    model.reset(1, 500)
    check(model.cols == 10 and model.rows == 100 and not model.alternate and model.lines.size() == 100, "reset clamps and clears alternate")
    print(JSON.stringify({"test": "terminal_screen", "checks": checks, "failures": failures}))
    quit(0 if failures == 0 else 1)

# FVPROV1 5524 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYTkwYzhhMjZkYjVmYjg1OGM5OWU0MTViM2U4NWUxNTkxZTU1MDlkYWUwNzMwNzE3MjA2ZjRlODQ5NDM3NjVhMyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjI5N2M0ZTQxMjkwYzVhYjVhYzdmY2Q1YjZiYjk0MzlhZTY4MTM3NGY1OTQyODZjMDk0MWI3NjY3Y2VlY2MyMjljZjI5YTJjNmU3Y2Q0ZDdjOTBjNDY2NzRlZWRmMzU1YTU2MDI4YTc3NmFiMTRiYjQ3MDI0ODIyZGY1ZGU1MGMifV19XQ== FVPROV1-END
