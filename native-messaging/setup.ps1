# Resurfacer native messaging host setup (Windows, Chrome/Edge)
# Run this script ONCE after:
#   1. Building the daemon:  cargo build --release
#   2. Loading the extension in Chrome (chrome://extensions -> "Load unpacked" -> select the extension/ folder)
#   3. Copying the extension ID from the chrome://extensions page
#   4. Pasting the extension ID below when prompted.
#
# The script writes a filled-in NM host manifest next to the exe and registers
# it in the Windows registry for both Chrome and Edge.

param(
    [string]$ExtensionId = "",
    [string]$Target = "both"   # chrome | edge | both
)

$ErrorActionPreference = "Stop"

# ── Locate the built executable ───────────────────────────────────────────────
$repoRoot = Split-Path $PSScriptRoot -Parent
$exeRelease = Join-Path $repoRoot "target\release\resurfacer-core.exe"
$exeDebug   = Join-Path $repoRoot "target\debug\resurfacer-core.exe"

if (Test-Path $exeRelease) {
    $exePath = $exeRelease
} elseif (Test-Path $exeDebug) {
    $exePath = $exeDebug
    Write-Warning "Using debug build. Run 'cargo build --release' for production use."
} else {
    Write-Error "resurfacer-core.exe not found. Run 'cargo build' first."
    exit 1
}

# ── Get the extension ID ──────────────────────────────────────────────────────
if (-not $ExtensionId) {
    $ExtensionId = Read-Host "Paste your Chrome extension ID (from chrome://extensions)"
}
$ExtensionId = $ExtensionId.Trim()
if ($ExtensionId -notmatch '^[a-p]{32}$') {
    Write-Error "Extension ID looks wrong ('$ExtensionId'). It should be 32 lowercase letters a-p."
    exit 1
}

# ── Write the manifest next to the exe ───────────────────────────────────────
$manifestPath = Join-Path (Split-Path $exePath) "com.resurfacer.host.json"
$manifest = @{
    name            = "com.resurfacer.host"
    description     = "Resurfacer native messaging host"
    path            = $exePath
    type            = "stdio"
    allowed_origins = @("chrome-extension://$ExtensionId/")
} | ConvertTo-Json -Depth 5

[System.IO.File]::WriteAllText($manifestPath, $manifest, [System.Text.Encoding]::UTF8)
Write-Host "Manifest written: $manifestPath"

# ── Register in the Windows registry ─────────────────────────────────────────
$regKey = "com.resurfacer.host"

if ($Target -eq "chrome" -or $Target -eq "both") {
    $chromePath = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$regKey"
    New-Item    -Path $chromePath -Force | Out-Null
    Set-ItemProperty -Path $chromePath -Name "(default)" -Value $manifestPath
    Write-Host "Registered for Chrome: $chromePath"
}

if ($Target -eq "edge" -or $Target -eq "both") {
    $edgePath = "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\$regKey"
    New-Item    -Path $edgePath -Force | Out-Null
    Set-ItemProperty -Path $edgePath -Name "(default)" -Value $manifestPath
    Write-Host "Registered for Edge: $edgePath"
}

Write-Host ""
Write-Host "Setup complete. Restart Chrome/Edge for the change to take effect."
Write-Host "Then open chrome://extensions and click 'service worker' under Resurfacer"
Write-Host "to inspect the console output of the background service worker."
