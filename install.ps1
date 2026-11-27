<#
.SYNOPSIS
  openbutlerd installer for Windows.

.DESCRIPTION
  irm https://raw.githubusercontent.com/m4rs1l/openbutler-daemon/main/install.ps1 -OutFile $env:TEMP\openbutlerd-install.ps1
  & $env:TEMP\openbutlerd-install.ps1 [-Version <x.y.z>] [-Prefix <dir>] [-NoModifyPath] [-Quiet]

  The archive carries the daemon, the bundled sidecar and the Node runtime it
  runs on; all three land beside the binary, which is where the daemon looks for
  them.
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Prefix = "",
    [switch]$NoModifyPath,
    [switch]$Quiet
)

$ErrorActionPreference = "Stop"

$App = "openbutlerd"
$Repo = if ($env:OPENBUTLER_DAEMON_REPO) { $env:OPENBUTLER_DAEMON_REPO } else { "m4rs1l/openbutler-daemon" }
$Root = if ($Prefix) { $Prefix } elseif ($env:OPENBUTLER_INSTALL_DIR) { $env:OPENBUTLER_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".openbutler" }
$Bin = Join-Path $Root "bin"
$Marker = Join-Path $Root ".installed-openbutlerd"

function Say([string]$Text) { if (-not $Quiet) { Write-Host $Text } }
function Die([string]$Text) { Write-Error "Error: $Text"; exit 1 }

function Show-Bar([int]$Percent, [string]$Phase, [switch]$Finish) {
    if ($Quiet) { return }
    # Only draw on a real console: a piped or redirected run gets plain lines,
    # never a thousand carriage returns in a log file.
    if ([Console]::IsOutputRedirected) { return }
    $filled = [int]($Percent * 20 / 100)
    if ($filled -gt 20) { $filled = 20 }
    $bar = ("=" * $filled) + ("-" * (20 - $filled))
    Write-Host ("`r[{0}] {1,3}% {2}" -f $bar, $Percent, $Phase) -NoNewline
    # The line ends only where the bar itself ends, so a redirected run stays
    # free of blank lines.
    if ($Finish) { Write-Host "" }
}

function Get-LatestVersion {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "openbutlerd-installer" }
    if (-not $release.tag_name) { Die "could not read the latest release of $Repo." }
    return ($release.tag_name -replace '^v', '')
}

function Download([string]$Url, [string]$Path, [string]$Name) {
    Add-Type -AssemblyName System.Net.Http
    $client = [System.Net.Http.HttpClient]::new()
    try {
        $response = $client.GetAsync($Url, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        if (-not $response.IsSuccessStatusCode) { Die "download failed ($($response.StatusCode)): $Url" }
        $total = $response.Content.Headers.ContentLength
        $input = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $output = [System.IO.File]::Create($Path)
        try {
            $buffer = New-Object byte[] 81920
            $done = 0
            while (($read = $input.Read($buffer, 0, $buffer.Length)) -gt 0) {
                $output.Write($buffer, 0, $read)
                $done += $read
                if ($total -gt 0) {
                    $percent = [int]($done * 100 / $total)
                    if ($percent -gt 99) { $percent = 99 }
                    Show-Bar $percent "downloading $Name"
                }
            }
        } finally {
            $output.Dispose()
            $input.Dispose()
        }
        Show-Bar 100 "downloading $Name" -Finish
    } finally {
        $client.Dispose()
    }
}

if (-not $Version) { $Version = Get-LatestVersion } else { $Version = $Version -replace '^v', '' }
$Asset = "$App-$Version-windows-amd64.zip"

if ((Test-Path $Marker) -and (Test-Path (Join-Path $Bin "$App.exe")) -and (Test-Path (Join-Path $Bin "sidecar\dist\entry.js"))) {
    $installed = (Get-Content $Marker | Where-Object { $_ -like "version=*" } | Select-Object -First 1) -replace '^version=', ''
    if ($installed -eq $Version) {
        Say "$App $Version is already installed at $(Join-Path $Bin "$App.exe"); nothing to do."
        exit 0
    }
}

$running = @(Get-Process -Name $App -ErrorAction SilentlyContinue | Where-Object {
    $path = $null
    try { $path = $_.Path } catch { }
    $path -and $path.StartsWith($Bin, [StringComparison]::OrdinalIgnoreCase)
})
if ($running.Count -gt 0) {
    Die "$App is running from this prefix (pid $($running[0].Id)); stop it first, then run this installer again."
}

$base = "https://github.com/$Repo/releases/download/v$Version"
$tmp = Join-Path $env:TEMP ("openbutlerd-install-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Download "$base/$Asset" (Join-Path $tmp $Asset) $Asset
    Download "$base/SHA256SUMS" (Join-Path $tmp "SHA256SUMS") "SHA256SUMS"

    $expected = (Get-Content (Join-Path $tmp "SHA256SUMS") | Where-Object { ($_ -split '\s+')[1] -eq $Asset } | Select-Object -First 1)
    if (-not $expected) { Die "SHA256SUMS does not list $Asset." }
    $expected = ($expected -split '\s+')[0]
    $actual = (Get-FileHash (Join-Path $tmp $Asset) -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) { Die "checksum mismatch for $Asset (expected $expected, got $actual)." }

    Show-Bar 100 "installing" -Finish

    Expand-Archive -Path (Join-Path $tmp $Asset) -DestinationPath $tmp
    $stage = Join-Path $tmp "$App-$Version-windows-amd64"
    if (-not (Test-Path $stage)) { Die "the archive did not contain $App-$Version-windows-amd64." }

    New-Item -ItemType Directory -Force -Path $Bin | Out-Null
    Copy-Item (Join-Path $stage "$App.exe") (Join-Path $Bin "$App.exe") -Force
    foreach ($dir in @("sidecar", "node")) {
        $target = Join-Path $Bin $dir
        if (Test-Path $target) { Remove-Item -Recurse -Force $target }
        Copy-Item -Recurse (Join-Path $stage $dir) $target
    }

    $reported = (& (Join-Path $Bin "$App.exe") --version 2>&1 | Out-String).Trim()
    if ($reported -notmatch (" " + [regex]::Escape($Version) + " ")) {
        Die "the installed binary does not report $Version (it said: $reported)."
    }

    @(
        "version=$Version"
        "asset=windows-amd64"
        "installed_at=$((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))"
    ) | Set-Content -Path $Marker -Encoding ascii

    if (-not $NoModifyPath) {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (-not $userPath) { $userPath = "" }
        $entries = $userPath -split ';' | Where-Object { $_ }
        if ($entries -notcontains $Bin) {
            $updated = (@($entries) + $Bin) -join ';'
            [Environment]::SetEnvironmentVariable("Path", $updated, "User")
            Say "PATH updated for this user account ($Bin)."
        }
    }

    Say "$App $Version installed at $(Join-Path $Bin "$App.exe")"
    if (-not $NoModifyPath) {
        Say "Restart the terminal so the updated PATH applies."
    }
    Say "Next: install the CLI from m4rs1l/openbutler-cli, then create the account with: openbutler admin create <username>"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
