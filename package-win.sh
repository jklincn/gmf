#!/bin/bash

set -e

export CARGO_BUILD_JOBS=6

PKG_NAME="gmf" 
TARGET_TRIPLE="x86_64-pc-windows-gnu"
EXECUTABLE_NAME="${PKG_NAME}.exe"
SOURCE_PATH="target/${TARGET_TRIPLE}/release/${EXECUTABLE_NAME}"
REMOTE_PATH="target/x86_64-unknown-linux-musl/release/gmf-remote"

ZIP_NAME="${PKG_NAME}-${TARGET_TRIPLE}.zip"


echo "🚀 步骤 1/4: 构建依赖 'gmf-remote' (release)..."
cargo build -p gmf-remote --release --target x86_64-unknown-linux-musl

echo "🚀 步骤 2/4: 构建 Windows 目标 'gmf' (release)..."
cargo build -p gmf --release --target x86_64-pc-windows-gnu

echo "✅ 构建完成: ${SOURCE_PATH}"

echo "📦 步骤 3/4: 正在使用 zip 进行极限压缩..."

FILES_TO_PACKAGE=("${SOURCE_PATH}")

zip -9 -j "${ZIP_NAME}" "${FILES_TO_PACKAGE[@]}"

echo "🚀 步骤 4/4: 完成打包！"
echo "🎉 成功！压缩包已创建: ${ZIP_NAME}"

# 打印文件大小
echo ""
echo "--- 文件大小 ---"
ls -lh "${SOURCE_PATH}" "${ZIP_NAME}" "${REMOTE_PATH}"
echo "----------------"
