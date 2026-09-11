# Konata_Mirror - Dev Mode 启动脚本
Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  Konata_Mirror (镜中此方) - Dev Mode" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# 加载 Rust 环境
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# 检查 cargo
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "[ERROR] cargo not found. Please install Rust: https://rustup.rs" -ForegroundColor Red
    Read-Host "Press Enter to exit"
    exit 1
}

# 检查 pnpm
if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    Write-Host "[ERROR] pnpm not found. Please install: npm install -g pnpm" -ForegroundColor Red
    Read-Host "Press Enter to exit"
    exit 1
}

# 进入项目目录
Set-Location $PSScriptRoot

# 安装依赖
if (-not (Test-Path "node_modules")) {
    Write-Host "[INFO] Installing dependencies..." -ForegroundColor Yellow
    pnpm install
    Write-Host ""
}

# 启动
Write-Host "[INFO] Starting Tauri dev server..." -ForegroundColor Green
Write-Host "[INFO] First launch may take 3-5 min to compile. Subsequent launches ~5s." -ForegroundColor DarkGray
Write-Host ""
pnpm tauri dev

Read-Host "Press Enter to exit"
