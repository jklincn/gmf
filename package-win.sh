#!/bin/bash

# 当任何命令失败时立即退出脚本
set -e

# --- 配置 ---
# 这是你要打包的那个包 (crate) 的名字
PKG_NAME="gmf" 
# 你可以手动指定版本，或者留空
PKG_VERSION="1.0.0" # <-- 修改为你当前的版本

TARGET_TRIPLE="x86_64-pc-windows-gnu"
EXECUTABLE_NAME="${PKG_NAME}.exe"
SOURCE_PATH="target/${TARGET_TRIPLE}/release/${EXECUTABLE_NAME}"
REMOTE_PATH="target/x86_64-unknown-linux-musl/release/gmf-remote"

# 如果版本号不为空，则加入到压缩包名称中
if [ -n "$PKG_VERSION" ]; then
    ZIP_NAME="${PKG_NAME}-v${PKG_VERSION}-${TARGET_TRIPLE}.zip"
else
    ZIP_NAME="${PKG_NAME}-${TARGET_TRIPLE}.zip"
fi


# --- 脚本执行 ---

# 步骤 0: 检查依赖工具是否存在
echo "🔍 步骤 0/4: 检查依赖工具..."
if ! command -v zip &> /dev/null; then
    echo "❌ 错误: 'zip' 命令未找到。"
    echo "请先安装 zip 工具。例如在 Debian/Ubuntu 上运行: sudo apt install zip"
    exit 1
fi
if ! command -v unzip &> /dev/null; then
    echo "⚠️ 警告: 'unzip' 命令未找到。将无法预览压缩包内容。"
fi
echo "✅ 依赖工具检查通过。"

echo "🚀 步骤 1/4: 构建依赖 'gmf-remote' (release)..."
cargo build -p gmf-remote --release --target x86_64-unknown-linux-musl

echo "🚀 步骤 2/4: 构建 Windows 目标 'gmf' (release)..."
cargo build -p gmf --release --target x86_64-pc-windows-gnu

echo "✅ 构建完成: ${SOURCE_PATH}"

# 检查产物是否存在
if [ ! -f "$SOURCE_PATH" ]; then
    echo "❌ 错误: 构建产物未找到! 请检查包名 (${PKG_NAME}) 和路径是否正确。"
    exit 1
fi

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
