# Konata_Mirror Build Script
# Usage: .\build.ps1

$ErrorActionPreference = "Stop"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  Konata_Mirror Build Script" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# Load Rust environment
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# Check cargo
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "[ERROR] cargo not found. Install Rust from https://rustup.rs" -ForegroundColor Red
    Read-Host "Press Enter to exit"
    exit 1
}

# Check pnpm
if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    Write-Host "[ERROR] pnpm not found. Run: npm install -g pnpm" -ForegroundColor Red
    Read-Host "Press Enter to exit"
    exit 1
}

# Enter project directory
Set-Location $PSScriptRoot

# Install dependencies if needed
if (-not (Test-Path "node_modules")) {
    Write-Host "[INFO] Installing dependencies..." -ForegroundColor Yellow
    pnpm install
    Write-Host ""
}

# Read version
$pkg = Get-Content package.json | ConvertFrom-Json
$version = $pkg.version
Write-Host "[INFO] Building version: $version" -ForegroundColor Green
Write-Host ""

# Clean old build
Write-Host "[INFO] Cleaning old build artifacts..." -ForegroundColor Yellow
if (Test-Path "dist") {
    Remove-Item -Recurse -Force "dist"
}

# Build
Write-Host "[INFO] Starting Tauri build (release mode)..." -ForegroundColor Green
Write-Host "[INFO] First build may take 5-15 minutes." -ForegroundColor DarkGray
Write-Host ""

pnpm tauri build

if ($LASTEXITCODE -eq 0) {
    Write-Host ""
    Write-Host "========================================" -ForegroundColor Green
    Write-Host "  Build Successful!" -ForegroundColor Green
    Write-Host "========================================" -ForegroundColor Green
    Write-Host ""

    $bundleDir = "src-tauri/target/release/bundle"
    if (Test-Path $bundleDir) {
        Write-Host "[INFO] Build artifacts:" -ForegroundColor Cyan
        Get-ChildItem -Recurse $bundleDir -Include "*.exe", "*.msi", "*.zip" | ForEach-Object {
            $sizeMB = [math]::Round($_.Length / 1MB, 2)
            Write-Host "  $($_.FullName) ($sizeMB MB)" -ForegroundColor White
        }
    }
} else {
    Write-Host ""
    Write-Host "[ERROR] Build failed. Check the error messages above." -ForegroundColor Red
}

Write-Host ""
Read-Host "Press Enter to exit"
