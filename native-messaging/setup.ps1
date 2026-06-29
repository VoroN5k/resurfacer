# Resurfacer native messaging host setup (Windows)
#
# Run ONCE after:
#   1. cargo build --release         (or cargo build for a debug binary)
#   2. Load the extension in Chrome/Firefox (see below)
#   3. Run this script
#
# Chrome/Edge setup:
#   - Open chrome://extensions (or edge://extensions)
#   - Enable "Developer mode"
#   - Click "Load unpacked" - select the extension/ folder
#   - Copy the Extension ID shown on the card
#
# Firefox setup:
#   - Run this script with -Target firefox first (it copies background.js into extension-firefox/)
#   - Open about:debugging#/runtime/this-firefox
#   - Click "Load Temporary Add-on…"
#   - Select extension-firefox/manifest.json   ← NOT extension/manifest.json
#   - The Extension ID is fixed: resurfacer@resurfacer.local (set in manifest.json)
#   - NOTE: temporary add-ons are removed on browser restart; for persistence
#     the extension must be signed or installed via about:config xpinstall.signatures.required=false
#
# Usage examples:
#   .\setup.ps1                            # Chrome + Edge + Firefox (prompts for Chrome ID)
#   .\setup.ps1 -Target firefox            # Firefox only (no Chrome ID needed)
#   .\setup.ps1 -ExtensionId abc...xyz     # Skip the prompt
#   .\setup.ps1 -Target chrome -ExtensionId abc...xyz

param(
    [string]$ExtensionId = "",
    # chrome | edge | firefox | all   ("all" = chrome + edge + firefox)
    [string]$Target = "all"
)

$ErrorActionPreference = "Stop"

$needsChromiumId = ($Target -eq "chrome" -or $Target -eq "edge" -or $Target -eq "all")
$needsFirefox    = ($Target -eq "firefox" -or $Target -eq "all")

# Locate the built executable
$repoRoot   = Split-Path $PSScriptRoot -Parent
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

Write-Host "Found exe: $exePath"

# Get Chrome extension ID (only when Chrome or Edge is a target)
if ($needsChromiumId) {
    if (-not $ExtensionId) {
        $ExtensionId = Read-Host "Paste your Chrome/Edge extension ID (from chrome://extensions)"
    }
    $ExtensionId = $ExtensionId.Trim()
    if ($ExtensionId -notmatch '^[a-p]{32}$') {
        Write-Error "Extension ID looks wrong ('$ExtensionId'). It should be 32 lowercase letters a-p."
        exit 1
    }
}

# Write the manifest next to the exe
# A single manifest file works for all browsers:
#   allowed_origins   - read by Chrome/Edge
#   allowed_extensions - read by Firefox
# Each browser ignores the key it doesn't understand

$manifestPath = Join-Path (Split-Path $exePath) "com.resurfacer.host.json"

$manifestObj = [ordered]@{
    name        = "com.resurfacer.host"
    description = "Resurfacer native messaging host - tab hygiene daemon"
    path        = $exePath
    type        = "stdio"
}

if ($needsChromiumId) {
    $manifestObj["allowed_origins"] = @("chrome-extension://$ExtensionId/")
}

# Firefox extension ID is static (set in extension/manifest.json - gecko.id)
if ($needsFirefox) {
    $manifestObj["allowed_extensions"] = @("resurfacer@resurfacer.local")
}

$manifest = $manifestObj | ConvertTo-Json -Depth 5
[System.IO.File]::WriteAllText($manifestPath, $manifest, [System.Text.Encoding]::UTF8)
Write-Host "Manifest written: $manifestPath"

# Register in the Windows registry
$regKey = "com.resurfacer.host"

if ($Target -eq "chrome" -or $Target -eq "all") {
    $path = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$regKey"
    New-Item -Path $path -Force | Out-Null
    Set-ItemProperty -Path $path -Name "(default)" -Value $manifestPath
    Write-Host "Registered for Chrome:   $path"
}

if ($Target -eq "edge" -or $Target -eq "all") {
    $path = "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\$regKey"
    New-Item -Path $path -Force | Out-Null
    Set-ItemProperty -Path $path -Name "(default)" -Value $manifestPath
    Write-Host "Registered for Edge:     $path"
}

if ($needsFirefox) {
    # Sync background.js from extension/ into extension-firefox/ so the two
    # manifests share the same script source without a build step
    $ffDir = Join-Path $repoRoot "extension-firefox"
    $ffBg  = Join-Path $ffDir "background.js"
    Copy-Item (Join-Path $repoRoot "extension\background.js") $ffBg -Force
    Write-Host "Synced background.js - extension-firefox/"

    $path = "HKCU:\Software\Mozilla\NativeMessagingHosts\$regKey"
    New-Item -Path $path -Force | Out-Null
    Set-ItemProperty -Path $path -Name "(default)" -Value $manifestPath
    Write-Host "Registered for Firefox:  $path"
}

Write-Host ""
Write-Host "Setup complete."
Write-Host ""
if ($needsChromiumId) {
    Write-Host "Chrome/Edge: restart the browser, then open chrome://extensions and"
    Write-Host "             click 'service worker' under Resurfacer to see logs."
}
if ($needsFirefox) {
    Write-Host "Firefox:     restart the browser (or reload the temporary add-on in"
    Write-Host "             about:debugging), then open the browser console with"
    Write-Host "             Ctrl+Shift+J to see background script logs."
}
