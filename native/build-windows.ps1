# Green, no-install Windows x64 build. Run this on the Windows machine: tauri-build compiles the
# app icon into a Win32 resource and needs llvm-rc/rc.exe from the Windows SDK, which macOS lacks.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Invoke-Step {
  param([string]$Name, [scriptblock]$Body)
  & $Body
  if ($LASTEXITCODE -ne 0) { throw "$Name failed with exit code $LASTEXITCODE" }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  throw 'Rust not found. Install from https://rustup.rs, then reopen PowerShell.'
}
if (-not (Get-Command cargo-tauri -ErrorAction SilentlyContinue)) {
  throw 'cargo-tauri not installed. Run: cargo install tauri-cli --locked'
}

Invoke-Step 'cargo tauri build' { cargo tauri build --no-bundle }

$exe = Join-Path $PSScriptRoot 'target\release\mouseborder-native.exe'
if (-not (Test-Path $exe)) { throw "build finished but $exe is missing" }
$out = Join-Path $PSScriptRoot '..\MouseBorder-Windows-x64.exe'
Copy-Item $exe $out
Write-Host ("Built {0} ({1:N1} MiB)" -f $out, ((Get-Item $out).Length / 1MB))

if (-not (Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -ErrorAction SilentlyContinue).pv) {
  Write-Warning 'WebView2 Runtime looks missing. Install it once from https://developer.microsoft.com/microsoft-edge/webview2/ or the window will not open.'
}
Write-Host 'Double-click the exe to run it: no installer, no registry writes, no shortcuts.'
Write-Host 'The Mac side connects to this machine, so allow inbound TCP 39876 from an elevated prompt:'
Write-Host '  New-NetFirewallRule -DisplayName MouseBorder -Direction Inbound -Protocol TCP -LocalPort 39876 -Action Allow -Profile Private'
Write-Host 'Settings persist at %APPDATA%\com.mouseborder.native\settings.json'
