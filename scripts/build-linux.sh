#!/usr/bin/env bash
set -euo pipefail

# Linux 发布构建脚本
#
# 默认构建 deb + AppImage；需要 rpm 时：
#   ./scripts/build-linux.sh deb appimage rpm
#
# 依赖见 docs/linux.md。

cd "$(dirname "$0")/.."

if ! command -v pnpm >/dev/null 2>&1; then
  echo "未找到 pnpm，请先安装：npm install -g pnpm" >&2
  exit 1
fi

BUNDLES=("$@")
if [ ${#BUNDLES[@]} -eq 0 ]; then
  BUNDLES=(deb appimage)
fi

BUNDLE_ARG=$(IFS=,; echo "${BUNDLES[*]}")

echo "==> 安装前端依赖"
pnpm install --frozen-lockfile

echo "==> 构建 Linux 安装包：${BUNDLE_ARG}"
pnpm tauri build --bundles "${BUNDLE_ARG}"

echo
echo "构建产物："
for bundle in "${BUNDLES[@]}"; do
  echo "  src-tauri/target/release/bundle/${bundle}/"
done
