#!/bin/bash
# ==============================================================================
# Aether Sovereign Node - 1-Line macOS Auto Installer
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/kjaylee/aether-node/main/install.sh | bash
# ==============================================================================
set -e

echo ""
echo "  █████╗ ███████╗████████╗██╗  ██╗███████╗██████╗ "
echo " ██╔══██╗██╔════╝╚══██╔══╝██║  ██║██╔════╝██╔══██╗"
echo " ███████║█████╗     ██║   ███████║█████╗  ██████╔╝"
echo " ██╔══██║██╔══╝     ██║   ██╔══██║██╔══╝  ██╔══██╗"
echo " ██║  ██║███████╗   ██║   ██║  ██║███████╗██║  ██║"
echo " ╚═╝  ╚═╝╚══════╝   ╚═╝   ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝"
echo "       SOVEREIGN BLOCKCHAIN NODE INSTALLER        "
echo "================================================================================"

# Check OS
OS="$(uname -s)"
if [ "$OS" != "Darwin" ]; then
    echo "❌ Error: This installer is intended for macOS (Darwin). Detected OS: $OS"
    exit 1
fi

INSTALL_DIR="/Applications"
APP_NAME="Aether Node.app"
APP_TARGET="$INSTALL_DIR/$APP_NAME"
REPO="kjaylee/aether-node"
RELEASE_TAG="v0.1.0"
DMG_URL="https://github.com/$REPO/releases/download/$RELEASE_TAG/Aether-Node-v0.1.0-macOS.dmg"
TMP_DIR="$(mktemp -d)"

cleanup() {
    if [ -d "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}
trap cleanup EXIT

echo "⏳ [1/4] 최신 Aether Node 배포판 다운로드 중..."
DMG_PATH="$TMP_DIR/Aether-Node.dmg"
curl -fSL "$DMG_URL" -o "$DMG_PATH" --progress-bar

echo "📦 [2/4] 디스크 이미지 마운트 및 /Applications 폴더에 설치 중..."
MOUNT_DIR="$TMP_DIR/mount"
mkdir -p "$MOUNT_DIR"
hdiutil attach "$DMG_PATH" -mountpoint "$MOUNT_DIR" -nobrowse -quiet

# Remove previous installation if exists
if [ -d "$APP_TARGET" ]; then
    echo " ℹ️ 기존 설치된 $APP_NAME 교체 중..."
    rm -rf "$APP_TARGET"
fi

cp -R "$MOUNT_DIR/$APP_NAME" "$INSTALL_DIR/"
hdiutil detach "$MOUNT_DIR" -quiet

echo "🛡️  [3/4] macOS Gatekeeper 보안 검역 속성 해제 (Quarantine Removal)..."
xattr -cr "$APP_TARGET"

echo "🚀 [4/4] Aether Node 실행..."
open "$APP_TARGET"

echo "================================================================================"
echo "  ✔ Aether Node가 /Applications에 성공적으로 설치되었습니다!"
echo "  ✔ Gatekeeper 보안 경고 없이 즉시 대시보드가 열립니다."
echo "  ✔ 다음 실행 시: Spotlight(Cmd+Space) 또는 Launchpad에서 'Aether Node' 검색"
echo "================================================================================"
echo ""
