<#
.SYNOPSIS
Rebuild, verify, upgrade, or extend a vendored crate from its crates.io source plus the
patch series in vendor/patches/<crate>.

.EXAMPLE
./vendor/patch.ps1 verify gpui-pre
./vendor/patch.ps1 apply gpui-pre
./vendor/patch.ps1 update gpui-pre 0.3.7
./vendor/patch.ps1 record gpui-pre "C16 text-derived labels"
#>
param(
    [Parameter(Mandatory)][ValidateSet('apply', 'verify', 'update', 'record')][string]$Command,
    [Parameter(Mandatory)][string]$Crate,
    [string]$Argument
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $root "vendor/$Crate"
$patches = Join-Path $root "vendor/patches/$Crate"
$versionFile = Join-Path $patches 'VERSION'

function Invoke-Git {
    & git -c core.autocrlf=false -c core.safecrlf=false @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed" }
}

function Get-Pristine([string]$version) {
    $crateFile = Get-ChildItem (Join-Path $env:CARGO_HOME_OR_DEFAULT 'registry/cache') -Recurse -Filter "$Crate-$version.crate" -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $work = Join-Path ([IO.Path]::GetTempPath()) "vendor-$Crate-$version-$PID"
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    New-Item -ItemType Directory $work | Out-Null
    $archive = Join-Path $work 'source.crate'
    if ($crateFile) {
        Copy-Item $crateFile.FullName $archive
    }
    else {
        Invoke-WebRequest "https://static.crates.io/crates/$Crate/$Crate-$version.crate" -OutFile $archive
    }
    tar -xzf $archive -C $work
    if ($LASTEXITCODE -ne 0) { throw "could not extract $Crate $version" }
    Join-Path $work "$Crate-$version"
}

# A scratch repository holding pristine sources with the series applied as commits.
function New-Series([string]$version, [switch]$Stop) {
    $source = Get-Pristine $version
    Push-Location $source
    try {
        Invoke-Git init -q
        Invoke-Git add -A
        Invoke-Git -c user.name=pristine -c user.email=pristine@local commit -q -m "$Crate $version"
        foreach ($patch in Get-ChildItem $patches -Filter '*.patch' | Sort-Object Name) {
            & git -c core.autocrlf=false am -q --3way --keep-cr $patch.FullName
            if ($LASTEXITCODE -ne 0) {
                Write-Host "CONFLICT in $($patch.Name). Resolve in $source, then 'git am --continue'," -ForegroundColor Yellow
                Write-Host "and export the series with: git format-patch -N --zero-commit --no-signature --no-stat -o <patches> <root>" -ForegroundColor Yellow
                if ($Stop) { exit 1 }
                throw "patch $($patch.Name) does not apply to $Crate $version"
            }
        }
    }
    finally {
        Pop-Location
    }
    $source
}

function Copy-Tree([string]$from, [string]$to) {
    if (Test-Path $to) { Remove-Item -Recurse -Force $to }
    New-Item -ItemType Directory $to | Out-Null
    Get-ChildItem -Force $from | Where-Object Name -ne '.git' | Copy-Item -Destination $to -Recurse -Force
}

# Hash of every file's relative path and bytes, ignoring a scratch `.git` directory.
function Get-TreeHash([string]$path) {
    $full = (Resolve-Path $path).Path.TrimEnd('\', '/')
    $sha = [Security.Cryptography.SHA256]::Create()
    $lines = Get-ChildItem -Recurse -File -Force $full |
        Where-Object { $_.FullName -notmatch '[\\/]\.git([\\/]|$)' } |
        ForEach-Object {
            $relative = $_.FullName.Substring($full.Length + 1).Replace('\', '/')
            $digest = [BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($_.FullName)))
            "$relative $digest"
        } |
        Sort-Object -CaseSensitive
    [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($lines -join "`n")))
}

if (-not $env:CARGO_HOME_OR_DEFAULT) {
    $env:CARGO_HOME_OR_DEFAULT = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
}
$version = (Get-Content $versionFile -Raw).Trim()

switch ($Command) {
    'apply' {
        $series = New-Series $version -Stop
        Copy-Tree $series $vendor
        Write-Host "vendor/$Crate rebuilt from $Crate $version and $((Get-ChildItem $patches -Filter '*.patch').Count) patches"
    }
    'verify' {
        $series = New-Series $version
        $expected = Get-TreeHash $series
        $actual = Get-TreeHash $vendor
        if ($expected -ne $actual) {
            Write-Host "vendor/$Crate differs from $Crate $version plus vendor/patches/$Crate." -ForegroundColor Red
            Write-Host "Record the change as a patch: ./vendor/patch.ps1 record $Crate <title>" -ForegroundColor Red
            exit 1
        }
        Write-Host "vendor/$Crate matches $Crate $version plus its patch series"
    }
    'update' {
        if (-not $Argument) { throw 'update needs the new version' }
        $series = New-Series $Argument -Stop
        Copy-Tree $series $vendor
        Set-Content -NoNewline -Path $versionFile -Value $Argument
        Write-Host "vendor/$Crate moved to $Crate $Argument with every patch applied"
    }
    'record' {
        if (-not $Argument) { throw 'record needs a patch title' }
        $series = New-Series $version -Stop
        Push-Location $series
        try {
            Get-ChildItem -Force | Where-Object Name -ne '.git' | Remove-Item -Recurse -Force
            Get-ChildItem -Force $vendor | Copy-Item -Destination $series -Recurse -Force
            Invoke-Git add -A
            & git diff --cached --quiet
            if ($LASTEXITCODE -eq 0) { Write-Host 'vendor matches the series; nothing to record'; return }
            Invoke-Git -c user.name="$(git -C $root config user.name)" -c user.email="$(git -C $root config user.email)" commit -q -m $Argument
            $number = (Get-ChildItem $patches -Filter '*.patch').Count + 1
            Invoke-Git format-patch -1 -q --zero-commit --no-signature --no-stat --start-number $number -o $patches HEAD
            $file = Get-ChildItem $patches -Filter ('{0:D4}-*.patch' -f $number) | Select-Object -First 1
            Write-Host "recorded $($file.Name)"
        }
        finally {
            Pop-Location
        }
    }
}
