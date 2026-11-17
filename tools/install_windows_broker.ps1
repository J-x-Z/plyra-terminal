#requires -Version 5.1
#requires -RunAsAdministrator
param(
    [Parameter(Mandatory = $true)][string]$PackageDirectory,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-fA-F]{64}$')][string]$ExpectedManifestSha256
)
$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitProcess) { throw 'Use 64-bit PowerShell for this installation.' }
$package = [IO.Path]::GetFullPath($PackageDirectory)
$manifestPath = Join-Path $package 'package-manifest.json'
$manifestBytes = [IO.File]::ReadAllBytes($manifestPath)
if ($manifestBytes.Length -gt 1048576) { throw 'Package manifest exceeds its byte budget.' }
$sha = [Security.Cryptography.SHA256]::Create()
try { $actualManifestHash = [BitConverter]::ToString($sha.ComputeHash($manifestBytes)).Replace('-', '') }
finally { $sha.Dispose() }
if ($actualManifestHash -ne $ExpectedManifestSha256) {
    throw 'The package manifest differs from the reviewed hash.'
}
$manifest = [Text.Encoding]::UTF8.GetString($manifestBytes) | ConvertFrom-Json
if ($manifest.schema -ne 'glass-terminal.windows-package.v1') { throw 'Unsupported package manifest.' }
$names = @()
foreach ($entry in $manifest.files) {
    if ($entry.name -notmatch '^(glass-terminal\.exe|plyra-privilege-broker\.exe|[A-Za-z0-9_.-]+\.dll|GLASS_THIRD_PARTY_NOTICES\.txt|LICENSE)$' -or $names -contains $entry.name) {
        throw 'The package contains an unexpected or duplicate binary name.'
    }
    $names += $entry.name
    $path = Join-Path $package $entry.name
    if ((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Package binaries cannot be reparse points.' }
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $entry.sha256 -or (Get-Item -LiteralPath $path).Length -ne $entry.bytes) {
        throw "Package binary hash or size mismatch ($($entry.name))."
    }
}
if ($names -notcontains 'glass-terminal.exe' -or $names -notcontains 'plyra-privilege-broker.exe' -or $names -notcontains 'GLASS_THIRD_PARTY_NOTICES.txt' -or $names -notcontains 'LICENSE') {
    throw 'Terminal, broker, and license notices are required.'
}
$trustedOwners = @('S-1-5-18', 'S-1-5-32-544', ([Security.Principal.NTAccount]::new('NT SERVICE\TrustedInstaller')).Translate([Security.Principal.SecurityIdentifier]).Value)
function Assert-ProtectedDirectory([string]$Path, [bool]$Leaf) {
    $item = Get-Item -LiteralPath $Path -Force
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Installation ancestor is not a regular directory ($Path)." }
    $acl = Get-Acl -LiteralPath $Path
    $ownerSid = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    if ($trustedOwners -notcontains $ownerSid) { throw "Installation ancestor has an untrusted owner ($Path)." }
    $raw = [Security.AccessControl.RawSecurityDescriptor]::new($acl.GetSecurityDescriptorBinaryForm(), 0)
    if ($null -eq $raw.DiscretionaryAcl) { throw "Installation ancestor has a null DACL ($Path)." }
    $dangerous = 0x50000000 -bor [long]([Security.AccessControl.FileSystemRights]::Delete -bor
        [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership -bor
        [Security.AccessControl.FileSystemRights]::WriteData -bor [Security.AccessControl.FileSystemRights]::WriteAttributes -bor
        [Security.AccessControl.FileSystemRights]::WriteExtendedAttributes -bor [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles)
    if ($Leaf) { $dangerous = $dangerous -bor [long][Security.AccessControl.FileSystemRights]::AppendData }
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) { continue }
        if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and
            ([long]$rule.FileSystemRights -band $dangerous) -ne 0 -and $trustedOwners -notcontains $rule.IdentityReference.Value) {
            throw "Installation ancestor permits unprivileged modification ($Path)."
        }
    }
}
$programFiles = [Environment]::GetFolderPath('ProgramFiles')
$ancestors = [Collections.Generic.List[string]]::new()
for ($directory = [IO.DirectoryInfo]::new($programFiles); $null -ne $directory; $directory = $directory.Parent) {
    $ancestors.Insert(0, $directory.FullName)
}
foreach ($ancestor in $ancestors) { Assert-ProtectedDirectory $ancestor ($ancestor -eq $programFiles) }
$destination = Join-Path $programFiles 'GlassTerminal'
if (Test-Path -LiteralPath $destination) {
    Assert-ProtectedDirectory $destination $true
} else {
    [IO.Directory]::CreateDirectory($destination) | Out-Null
    $directoryAcl = [Security.AccessControl.DirectorySecurity]::new()
    $directoryAcl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)')
    Set-Acl -LiteralPath $destination -AclObject $directoryAcl
}
$owner = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
$fileAcl = [Security.AccessControl.FileSecurity]::new()
$fileAcl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;BU)')
$fileAcl.SetOwner($owner)
$lockPath = Join-Path $destination '.install.lock'
if (Test-Path -LiteralPath $lockPath) {
    $lockItem = Get-Item -LiteralPath $lockPath -Force
    if ($lockItem.PSIsContainer -or ($lockItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Installation lock must be a regular file.'
    }
}
$installLock = [IO.FileStream]::new($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
$stage = Join-Path $destination ('.staging-' + [Guid]::NewGuid().ToString('N'))
$stagedFiles = Join-Path $stage 'files'
$backups = Join-Path $stage 'backups'
$failedFiles = Join-Path $stage 'failed'
$changes = [Collections.Generic.List[object]]::new()
$preserveStage = $false
function Install-StagedFile([string]$Name) {
    $temporary = Join-Path $stagedFiles $Name
    $target = Join-Path $destination $Name
    $hadOriginal = Test-Path -LiteralPath $target
    $change = [PSCustomObject]@{
        Name = $Name
        Target = $target
        Backup = Join-Path $backups $Name
        HadOriginal = $hadOriginal
        OldAcl = $(if ($hadOriginal) { Get-Acl -LiteralPath $target } else { $null })
        Applied = $false
    }
    $changes.Add($change)
    if ($hadOriginal) {
        [IO.File]::Replace($temporary, $target, $change.Backup)
    } else {
        [IO.File]::Move($temporary, $target)
    }
    $change.Applied = $true
    # ReplaceFile preserves destination metadata. Normalize the committed file too.
    Set-Acl -LiteralPath $target -AclObject $fileAcl
}
try {
    foreach ($existing in Get-ChildItem -LiteralPath $destination -Force) {
        if ($existing.PSIsContainer -or ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            ($names -notcontains $existing.Name -and $existing.Name -notin @('package-manifest.json', '.install.lock'))) {
            throw 'The installation contains an unrelated entry. Review it before updating.'
        }
    }
    foreach ($directory in @($stagedFiles, $backups, $failedFiles)) {
        [IO.Directory]::CreateDirectory($directory) | Out-Null
    }
    # Complete and verify staging before changing any installed entry.
    foreach ($entry in $manifest.files) {
        $temporary = Join-Path $stagedFiles $entry.name
        [IO.File]::Copy((Join-Path $package $entry.name), $temporary, $false)
        if ((Get-FileHash -LiteralPath $temporary -Algorithm SHA256).Hash -ne $entry.sha256 -or (Get-Item -LiteralPath $temporary).Length -ne $entry.bytes) {
            throw 'Package entry changed while staging.'
        }
        Set-Acl -LiteralPath $temporary -AclObject $fileAcl
    }
    $stagedManifest = Join-Path $stagedFiles 'package-manifest.json'
    [IO.File]::WriteAllBytes($stagedManifest, $manifestBytes)
    Set-Acl -LiteralPath $stagedManifest -AclObject $fileAcl
    foreach ($entry in $manifest.files) { Install-StagedFile $entry.name }
    Install-StagedFile 'package-manifest.json'
} catch {
    $installationError = $_
    $rollbackErrors = [Collections.Generic.List[string]]::new()
    for ($index = $changes.Count - 1; $index -ge 0; $index--) {
        $change = $changes[$index]
        try {
            if ($change.HadOriginal) {
                if (Test-Path -LiteralPath $change.Backup) {
                    if (Test-Path -LiteralPath $change.Target) {
                        [IO.File]::Replace($change.Backup, $change.Target, (Join-Path $failedFiles $change.Name))
                    } else {
                        [IO.File]::Move($change.Backup, $change.Target)
                    }
                    Set-Acl -LiteralPath $change.Target -AclObject $change.OldAcl
                } elseif ($change.Applied) {
                    throw 'The previous file backup is unavailable.'
                }
            } elseif ($change.Applied -and (Test-Path -LiteralPath $change.Target)) {
                [IO.File]::Delete($change.Target)
            }
        } catch {
            $rollbackErrors.Add("$($change.Name) ($($_.Exception.Message))")
        }
    }
    if ($rollbackErrors.Count -gt 0) {
        $preserveStage = $true
        throw "Installation failed ($($installationError.Exception.Message)). Rollback incomplete ($($rollbackErrors -join '; ')). Backups retained in $stage"
    }
    throw $installationError
} finally {
    try {
        if (-not $preserveStage -and (Test-Path -LiteralPath $stage)) {
            Remove-Item -LiteralPath $stage -Recurse -Force
        }
    } finally {
        $installLock.Dispose()
    }
}
Write-Output "Installed terminal and UAC broker in $destination"
Write-Output 'Runtime authorization verifies owner, DACL, non-reparse ancestors, and pinned executable files before requesting UAC.'

# FVPROV1 9954 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNGZiODQ4NDczMjYyZWY3MTUxZDdlYjEyNGNiZDRmNDM1NWJlZDQ1MzMwN2MyYjg0NDMwZTViYzEwNDMyNTcwMyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZTAyZDZiYTY2YmIyOGEzZTA5YjQ4NDk1NzNkMDkwY2ZmYjJmNzVlMGJmMThhMjBkMjUyOTVlMTEzMjNhYzM0Y2JlODNlY2QxMDg5NTZmNWEyNTdhMGE4ODFlNGU4YjQ0ODc0ZjJlNWJkN2Q4NWY2N2VmZDcxMzIxMmYxMWViMDUifV19XQ== FVPROV1-END
