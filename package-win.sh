#!/bin/bash

# Prerequisites
# curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# rustup target add x86_64-unknown-linux-musl
# rustup target add x86_64-pc-windows-gnu
# sudo apt install mingw-w64 zip musl-tools build-essential cmake ninja-build perl pkg-config

set -e

export CARGO_BUILD_JOBS=6

PKG_NAME="gmf" 
TARGET_TRIPLE="x86_64-pc-windows-gnu"
EXECUTABLE_NAME="${PKG_NAME}.exe"
SOURCE_PATH="target/${TARGET_TRIPLE}/release/${EXECUTABLE_NAME}"
REMOTE_PATH="target/x86_64-unknown-linux-musl/release/gmf-remote"

echo "1/3: 构建依赖 'gmf-remote' (release)..."
cargo build -p gmf-remote --release --target x86_64-unknown-linux-musl

echo "2/3: 构建 Windows 目标 'gmf' (release)..."
cargo build -p gmf --release --target x86_64-pc-windows-gnu

echo "3/3: 构建完成: ${SOURCE_PATH}"

# 打印文件大小
echo ""
echo "--- 文件大小 ---"
ls -lh "${SOURCE_PATH}" "${REMOTE_PATH}"
echo "----------------"

cp ${SOURCE_PATH} ./gmf-x86_64-pc-windows-gnu.exe