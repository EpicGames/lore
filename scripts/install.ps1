#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Install the Lore CLI (and, with -Demo, a local loreserver) from GitHub Releases.
.DESCRIPTION
    PowerShell peer of scripts/install.sh. Works on Windows PowerShell 5.1 and PowerShell 7+.

    Quick start:
      irm https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.ps1 | iex
      $env:LORE_DEMO=1; irm https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.ps1 | iex

    For parameters and their env-var equivalents, run with -Help.
.EXAMPLE
    .\install.ps1 -Demo
#>
[CmdletBinding()]
param(
    [switch] $Demo,
    [switch] $Server,
    [string] $Version    = $env:LORE_VERSION,
    [string] $InstallDir = $env:LORE_INSTALL_DIR,
    [string] $Repo       = $env:LORE_REPO,
    [string] $Token      = $env:GITHUB_TOKEN,
    [switch] $Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# 5.1 defaults to TLS 1.0, which github.com rejects; force 1.2. Harmless on 7+.
[Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

function Say { param([string]$Msg) [Console]::Error.WriteLine($Msg) }
function Die { param([string]$Msg) [Console]::Error.WriteLine("error: $Msg"); exit 1 }

function Show-Usage {
@"
Install the Lore CLI (and, with -Demo, a local loreserver) from GitHub Releases.

Usage: install.ps1 [-Demo] [-Server] [-Version <v>] [-InstallDir <dir>] [-Repo <owner/repo>] [-Token <t>]

Every parameter has an env-var equivalent; the parameter wins when both are set:

  -Demo              LORE_DEMO         also install and launch a local loreserver (1/true/yes/on/enabled)
  -Server            LORE_SERVER       only install loreserver (skip the lore CLI and auto-launch)
  -Version <v>       LORE_VERSION      install a specific release tag (default: latest)
  -InstallDir <dir>  LORE_INSTALL_DIR  where binaries go (default: %USERPROFILE%\bin)
  -Repo <owner/repo> LORE_REPO         source repository (default: EpicGames/lore)
  -Token <t>         GITHUB_TOKEN      token for private repos / higher rate limit (defaults to `gh auth token`)
  -Help                                show this help
"@ -split "`n" | ForEach-Object { [Console]::Error.WriteLine($_) }
}
if ($Help) { Show-Usage; exit 0 }

if (-not $Repo)       { $Repo = 'EpicGames/lore' }
if (-not $Version)    { $Version = 'latest' }
if (-not $InstallDir) { $InstallDir = Join-Path $env:USERPROFILE 'bin' }
# -Demo is a switch; also honor $env:LORE_DEMO (accepts 1/true/yes/on/enabled).
$DemoOn   = $Demo.IsPresent   -or (@('1','true','yes','on','enabled') -contains "$($env:LORE_DEMO)".ToLower())
$ServerOn = $Server.IsPresent -or (@('1','true','yes','on','enabled') -contains "$($env:LORE_SERVER)".ToLower())

$GrpcPort = 41337
$HttpPort = 41339

# Fall back to the gh CLI's token when none was supplied. Pin github.com: we only
# ever call api.github.com, and gh may be active on a different host (e.g. an
# enterprise GHE), whose token would not authenticate.
if (-not $Token) {
    $ghCmd = Get-Command gh -ErrorAction SilentlyContinue
    if ($ghCmd) {
        $t = (gh auth token --hostname github.com 2>$null)
        if ($LASTEXITCODE -eq 0 -and $t) { $Token = $t.Trim(); Say "using GitHub token from gh CLI" }
    }
}

switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { $arch = 'x86_64' }
    'ARM64' { $arch = 'aarch64' }
    default { Die "unsupported architecture $($env:PROCESSOR_ARCHITECTURE)" }
}
$Triple = "$arch-pc-windows-msvc"

$Work = Join-Path ([IO.Path]::GetTempPath()) ("lore-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $Work | Out-Null

# Build request headers, optionally with the bearer token. -UseBasicParsing is
# passed on every Invoke-WebRequest below for 5.1 (avoids the IE parsing engine).
function Get-Headers {
    param([string]$Accept, [switch]$Auth)
    $h = @{ 'User-Agent' = 'lore-install.ps1' }
    if ($Accept) { $h['Accept'] = $Accept }
    if ($Auth -and $Token) { $h['Authorization'] = "Bearer $Token" }
    return $h
}

# Fetch release metadata once. Invoke-RestMethod parses the JSON into objects, so
# we select the asset directly below -- no field-order scraping like install.sh's
# awk needs.
function Get-Release {
    $api = "https://api.github.com/repos/$Repo/releases"
    if ($Version -eq 'latest') { $api += '/latest' } else { $api += "/tags/$Version" }
    return Invoke-RestMethod -Uri $api -Headers (Get-Headers 'application/vnd.github+json' -Auth)
}

# The asset's API url (.../releases/assets/<id>), NOT browser_download_url, so the
# download can send Accept: application/octet-stream + bearer -- the only way a
# private-repo asset downloads.
function Get-AssetUrl {
    param([object]$Release, [string]$Bin)
    $a = $Release.assets | Where-Object { $_.name -match "^$Bin-v?\d.*-$Triple\.zip$" } | Select-Object -First 1
    if ($a) { return $a.url } else { return $null }
}

# Resolve a single 302 without following it, returning the Location (5.1 path).
# Under -ErrorAction Stop, PS 5.1 throws InvalidOperationException (no .Response)
# on the blocked redirect; -ErrorAction SilentlyContinue returns the 302 response
# directly so we can read the Location header.
function Resolve-Redirect {
    param([string]$Url, [hashtable]$Headers)
    $r = Invoke-WebRequest -Uri $Url -Headers $Headers -MaximumRedirection 0 -UseBasicParsing -ErrorAction SilentlyContinue
    if ($r -and $r.Headers -and $r.Headers['Location']) { return [string]$r.Headers['Location'] }
    return $null
}

function Save-Asset {
    param([string]$Url, [string]$OutFile)
    if (-not $Token) {
        Invoke-WebRequest -Uri $Url -Headers (Get-Headers 'application/octet-stream') -OutFile $OutFile -UseBasicParsing
        return
    }
    $authHeaders = Get-Headers 'application/octet-stream' -Auth
    if ($PSVersionTable.PSVersion.Major -ge 7) {
        # PS7 drops Authorization on the cross-host CDN redirect by default.
        Invoke-WebRequest -Uri $Url -Headers $authHeaders -OutFile $OutFile -UseBasicParsing
    } else {
        # 5.1: follow the redirect manually so the bearer is NOT sent to the signed
        # CDN URL, which rejects requests carrying an extra Authorization header.
        $loc = Resolve-Redirect -Url $Url -Headers $authHeaders
        if ($loc) { Invoke-WebRequest -Uri $loc -OutFile $OutFile -UseBasicParsing }
        else      { Invoke-WebRequest -Uri $Url -Headers $authHeaders -OutFile $OutFile -UseBasicParsing }
    }
}

# Download, unpack, and install <bin>.exe into $InstallDir, replacing any existing copy.
function Install-Binary {
    param([object]$Release, [string]$Bin)
    $exe     = "$Bin.exe"
    $binPath = Join-Path $InstallDir $exe
    $url = Get-AssetUrl -Release $Release -Bin $Bin
    if (-not $url) { Die "no $Bin release found for $Triple (repo=$Repo version=$Version)" }

    if (Get-Command $Bin -ErrorAction SilentlyContinue) {
        $cur = (& $Bin --version 2>$null); if (-not $cur) { $cur = $Bin }
        Say "$cur found - updating"
    } else { Say "installing $Bin" }

    $zip = Join-Path $Work "$Bin.zip"
    Save-Asset -Url $url -OutFile $zip
    $out = Join-Path $Work "$Bin-unzipped"
    Expand-Archive -Path $zip -DestinationPath $out -Force
    # Asset may be flat ($out\<bin>.exe) or nested under a versioned folder
    # ($out\<bin>-<ver>-<triple>\<bin>.exe). Find it wherever it landed.
    $src = Get-ChildItem -Path $out -Recurse -Filter $exe -File | Select-Object -First 1
    if (-not $src) { Die "extracted archive did not contain $exe" }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Path $src.FullName -Destination $binPath -Force

    $ver = (& $binPath --version 2>$null); if (-not $ver) { $ver = $Bin }
    Say "installed $ver -> $binPath"
}

# Prepend $InstallDir to the persisted User PATH (if absent) and the live session.
function Add-ToPath {
    $sep = ';'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $present = $false
    if ($userPath) {
        foreach ($p in ($userPath -split $sep)) {
            if ($p -and ($p.TrimEnd('\') -ieq $InstallDir.TrimEnd('\'))) { $present = $true; break }
        }
    }
    if (-not $present) {
        if ($userPath) { $new = "$InstallDir$sep$userPath" } else { $new = $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $new, 'User')
        Say "added $InstallDir to your User PATH - open a new terminal to pick it up"
    }
    # Always fix the live session so the just-installed binaries resolve now.
    if (-not (($env:Path -split $sep) | Where-Object { $_.TrimEnd('\') -ieq $InstallDir.TrimEnd('\') })) {
        $env:Path = "$InstallDir$sep$env:Path"
    }
}

# Install loreserver, launch it on its zero-config ports, and print what to try next.
function Invoke-Demo {
    param([object]$Release)
    Install-Binary -Release $Release -Bin 'loreserver'
    $serverExe = Join-Path $InstallDir 'loreserver.exe'
    # Send server logs to files instead of the console -- otherwise the periodic
    # store/memory stats bury the banner below. Default to a filter that drops just
    # that spam. Start-Process can't merge stdout
    # and stderr into one file, so we keep two.
    $log    = Join-Path $env:TEMP 'loreserver-demo.log'
    $errLog = Join-Path $env:TEMP 'loreserver-demo.err.log'
    if (-not $env:RUST_LOG) { $env:RUST_LOG = 'info' }

    $proc = Start-Process -FilePath $serverExe -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $log -RedirectStandardError $errLog
    try {
        $ready = $false
        for ($i = 0; $i -lt 20; $i++) {
            try {
                $r = Invoke-WebRequest -Uri "http://127.0.0.1:$HttpPort/health_check" -UseBasicParsing -TimeoutSec 2
                if ($r.StatusCode -eq 200) { $ready = $true; break }
            } catch { }
            if ($proc.HasExited) { break }   # server exited early; stop waiting
            Start-Sleep -Milliseconds 500
        }
        if (-not $ready) {
            Say "loreserver did not come up - last lines from $log / ${errLog}:"
            if (Test-Path $log)    { Get-Content -Tail 20 $log    | ForEach-Object { Say $_ } }
            if (Test-Path $errLog) { Get-Content -Tail 20 $errLog | ForEach-Object { Say $_ } }
            if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
            exit 1
        }
        Say @"

loreserver is running:
    gRPC/QUIC : lore://127.0.0.1:$GrpcPort
    HTTP      : http://127.0.0.1:$HttpPort   (health: /health_check)
    logs      : $log   (run: Get-Content -Wait $log)

Open a NEW terminal and try:
    curl.exe -i http://127.0.0.1:$HttpPort/health_check
    mkdir `$HOME\my-project; cd `$HOME\my-project
    lore repository create lore://127.0.0.1:$GrpcPort/my-project

Then continue the quickstart to add, commit, and push:
    https://epicgames.github.io/lore/tutorials/quickstart/

(Ctrl-C to stop the server)
"@
        Wait-Process -Id $proc.Id
    }
    finally {
        if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
    }
}

try {
    # Surface the real fetch error (404/403/network) with the private-repo hint,
    # matching install.sh; the generic catch below would drop the hint.
    try { $release = Get-Release }   # fetched once; all installs select from it
    catch {
        Die ("could not fetch $Version release for ${Repo}: $($_.Exception.Message)`n" +
             "hint: for a private repo set GITHUB_TOKEN or run 'gh auth login'")
    }

    if ($DemoOn) {
        $priorLore = $null
        $lc = Get-Command lore -ErrorAction SilentlyContinue
        if ($lc) { $priorLore = $lc.Source }
        Install-Binary -Release $release -Bin 'lore'
        Add-ToPath
        if ($priorLore -and ($priorLore -ine (Join-Path $InstallDir 'lore.exe'))) {
            Say "note: another 'lore' is at $priorLore - ensure $InstallDir comes first on PATH"
        }
        Invoke-Demo -Release $release   # waits; cleans up $Work in its finally
    } elseif ($ServerOn) {
        Install-Binary -Release $release -Bin 'loreserver'
        Add-ToPath
        Say ""
        Say "Done. ``loreserver`` unpacked in $InstallDir."
        Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
    } else {
        $priorLore = $null
        $lc = Get-Command lore -ErrorAction SilentlyContinue
        if ($lc) { $priorLore = $lc.Source }
        Install-Binary -Release $release -Bin 'lore'
        Add-ToPath
        if ($priorLore -and ($priorLore -ine (Join-Path $InstallDir 'lore.exe'))) {
            Say "note: another 'lore' is at $priorLore - ensure $InstallDir comes first on PATH"
        }
        Say ""
        Say "Done. Run 'lore --version', or re-run with -Demo to launch a local server."
        Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
    }
}
catch { Die $_.Exception.Message }
