# SPDX-License-Identifier: GPL-3.0-only
extends SceneTree
const Main=preload("res://standalone/main.gd")
var app: Control
func _initialize() -> void:
    _run.call_deferred()
func _run() -> void:
    app=Main.new();root.add_child(app)
    var deadline:=Time.get_ticks_msec()+7000
    while not app.terminal_view.terminal.running and Time.get_ticks_msec()<deadline: await process_frame
    if not app.terminal_view.terminal.running:
        print(JSON.stringify({"test":"standalone_close","passed":false}));quit(1);return
    var session: Node=app.terminal_view.terminal
    print(JSON.stringify({"test":"standalone_close","passed":true,"helper":session.helper_pid,"shell":session.shell_pid,"close_requested":true}))
    app.request_close()

# FVPROV1 740 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMGQ2ZjczZGViMzZkNjE1OWU4ZjIzNTAzNDkzY2Q0MGZiOWNkYmM2YzU0NDY2MzlkZjRlN2ZmYTZhY2Y0MzFjMCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYTdmNDczOGFhMWU2M2M3YWRjZjk2ZTRkZDFlNTQ3MTE4MWU0YzNhNjdhM2MxMDMxOTk5MGUxZDQ4NGYxZmYyMGYzYzdmZTk1ZmM5M2ZlZjZkNTdlYjYzNTZiMjY3MjVhNzJiZThkZWEzNmNkYmRmMDlkZTQwODgxMDU2MTA3MDEifV19XQ== FVPROV1-END
