<#
.SYNOPSIS
    Windows launcher for dev-launcher via WSL2.

.DESCRIPTION
    dev-launcher requires a Unix environment (process groups, signals, termios).
    On Windows, this script forwards every invocation to the Linux binary installed
    inside WSL2, passing all arguments through transparently.

.EXAMPLE
    .\dev-launcher.ps1
    .\dev-launcher.ps1 --copilot-branch feat/my-feature
    .\dev-launcher.ps1 --workspace a1b2c3d4
#>

param([Parameter(ValueFromRemainingArguments)][string[]]$PassThrough)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── 1. WSL2 must be present ───────────────────────────────────────────────────

if (-not (Get-Command wsl -ErrorAction SilentlyContinue)) {
    Write-Host ""
    Write-Host "  ERROR  WSL2 is not installed." -ForegroundColor Red
    Write-Host ""
    Write-Host "  dev-launcher requires WSL2 to run on Windows."
    Write-Host "  Install it with:"
    Write-Host ""
    Write-Host "      wsl --install" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "  Then reboot and re-run this script."
    Write-Host "  Full guide: https://learn.microsoft.com/windows/wsl/install"
    Write-Host ""
    exit 1
}

# ── 2. Verify WSL2 (not WSL1) is the active version ──────────────────────────

$wslStatus = wsl --status 2>&1 | Out-String
if ($wslStatus -match "WSL 1" -and $wslStatus -notmatch "WSL 2") {
    Write-Host ""
    Write-Host "  WARNING  Your default WSL version appears to be WSL1." -ForegroundColor Yellow
    Write-Host "  Docker Desktop requires WSL2. Upgrade with:"
    Write-Host ""
    Write-Host "      wsl --set-default-version 2" -ForegroundColor Cyan
    Write-Host ""
}

# ── 3. dev-launcher must be installed inside WSL2 ────────────────────────────

$binaryPath = wsl --exec sh -c 'command -v dev-launcher 2>/dev/null' 2>$null
if (-not $binaryPath) {
    Write-Host ""
    Write-Host "  ERROR  dev-launcher is not installed inside WSL2." -ForegroundColor Red
    Write-Host ""
    Write-Host "  Download the Linux x86_64 binary from:"
    Write-Host "  https://github.com/AreDee-Bangs/dev-launcher/releases/latest"
    Write-Host ""
    Write-Host "  Then inside your WSL2 terminal run:"
    Write-Host ""
    Write-Host "      chmod +x dev-launcher-linux-x86_64" -ForegroundColor Cyan
    Write-Host "      sudo mv dev-launcher-linux-x86_64 /usr/local/bin/dev-launcher" -ForegroundColor Cyan
    Write-Host ""
    exit 1
}

# ── 4. Forward to WSL2 ───────────────────────────────────────────────────────
#
# --cd ~ ensures WSL2 starts in the Linux home directory, not a Windows path.
# FILIGRAN_WORKSPACE_ROOT is forwarded if set in the Windows environment so
# users who set it in PowerShell profile don't have to duplicate it in WSL2.

$env:WSLENV = "FILIGRAN_WORKSPACE_ROOT/u:FILIGRAN_LLM_KEY"

if ($PassThrough.Count -gt 0) {
    wsl --cd ~ dev-launcher @PassThrough
} else {
    wsl --cd ~ dev-launcher
}

exit $LASTEXITCODE
