# SPDX-License-Identifier: GPL-3.0-only
# Register the extracted Plyra Terminal package in the current user's Start menu.
$ErrorActionPreference = 'Stop'
$terminal = Join-Path $PSScriptRoot 'glass-terminal.exe'
$icon = Join-Path $PSScriptRoot 'glass-terminal.ico'
foreach ($file in @($terminal, $icon)) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "Missing package file: $file"
    }
}

$programs = [Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)
if ([string]::IsNullOrEmpty($programs)) {
    throw 'The current user has no Start menu Programs directory.'
}
$shortcutPath = Join-Path $programs 'Plyra Terminal.lnk'
$legacyPath = Join-Path $programs 'Glass Terminal.lnk'
$shell = New-Object -ComObject WScript.Shell
if (-not (Test-Path -LiteralPath $shortcutPath) -and (Test-Path -LiteralPath $legacyPath -PathType Leaf)) {
    $legacy = $shell.CreateShortcut($legacyPath)
    if ($legacy.TargetPath -eq $terminal -and $legacy.WorkingDirectory -eq $PSScriptRoot -and
        $legacy.Description -eq 'Glass Terminal' -and [string]::IsNullOrEmpty($legacy.Arguments) -and
        ($legacy.IconLocation -replace ',\s*0$', '') -eq $icon) {
        Move-Item -LiteralPath $legacyPath -Destination $shortcutPath
    }
}
$shortcut = $shell.CreateShortcut($shortcutPath)
if (Test-Path -LiteralPath $shortcutPath) {
    if ($shortcut.TargetPath -ne $terminal) {
        throw "Start menu shortcut belongs to another target: $shortcutPath"
    }
} else {
    $shortcut.TargetPath = $terminal
    $shortcut.WorkingDirectory = $PSScriptRoot
}
$shortcut.Description = 'Plyra Terminal'
$shortcut.IconLocation = "$icon,0"
$shortcut.Save()
Write-Output $shortcutPath

# FVPROV1 1702 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiN2Q5YTRmMmY3OGQ1NDllYzg1MmQ1ODc0Yzg1ZTE4ZDIxOThjNTMyMTA3ZTlmMzgxNjMxOWZiZjI2MjUyMWZiMyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMzEwZTZhMThmNDNhYjhjZGIwMjhjZTRiODNlOTQzNDI4NTIwNjJjMTkwNTMwZjkwZTY2YTg1OGY5M2FmYjgyNWZhNDI3MTVhNTQ3ZmFjNmVjZGRlY2UyNzc4YjE2NjVlZjdkZGE2N2RhODY0OGUyNTZmYzIyZDMxNTU4N2Q3MGUifV19XQ== FVPROV1-END
