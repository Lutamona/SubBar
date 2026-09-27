#!/bin/bash
# Builds SubBar.app (release) and drops it into dist/.
# --universal — один бинарь для Apple Silicon и Intel (для релиза на GitHub).
set -euo pipefail
cd "$(dirname "$0")/.."

UNIVERSAL=0
for arg in "$@"; do
  case "$arg" in
    --universal) UNIVERSAL=1 ;;
    *) echo "Неизвестный аргумент: $arg (есть только --universal)" >&2; exit 2 ;;
  esac
done

APP_NAME="SubBar"
BUNDLE_ID="ai.subbar.app"
# awk дочитывает до конца: head под pipefail ронял бы скрипт SIGPIPE-ом, будь строк version две.
VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | awk 'NR == 1')"
if [ -z "$VERSION" ]; then
  echo "Не удалось прочитать версию из Cargo.toml" >&2
  exit 1
fi
OUT="dist"
APP="$OUT/$APP_NAME.app"

# Всё нужное — до долгой компиляции, а не после неё.
for tool in cargo sips iconutil codesign; do
  command -v "$tool" >/dev/null 2>&1 || { echo "Не найден $tool — сборка не пройдёт" >&2; exit 127; }
done
[ -f assets/icon.png ] || { echo "Нет assets/icon.png — запусти из полного клона репозитория" >&2; exit 1; }
# Каталог сборки cargo можно переназначить (CARGO_TARGET_DIR) — иначе cp падал бы с «No such file».
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
TRIPLES="aarch64-apple-darwin x86_64-apple-darwin"
if [ "$UNIVERSAL" -eq 1 ]; then
  command -v lipo >/dev/null 2>&1 || { echo "Не найден lipo — нужны Command Line Tools (xcode-select --install)" >&2; exit 127; }
  for triple in $TRIPLES; do
    rustup target list --installed 2>/dev/null | grep -qx "$triple" || { echo "Нет цели $triple — поставь: rustup target add $triple" >&2; exit 127; }
  done
fi

if [ "$UNIVERSAL" -eq 1 ]; then
  mkdir -p "$OUT"
  for triple in $TRIPLES; do
    echo "==> cargo build --release --target $triple"
    # Нижняя версия macOS — как в Info.plist, а не умолчание целевой платформы.
    MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release --target "$triple"
  done
  BIN="$OUT/.subbar.universal.$$"
  SLICES=()
  for triple in $TRIPLES; do SLICES+=("$TARGET_DIR/$triple/release/subbar"); done
  lipo -create -output "$BIN" "${SLICES[@]}"
else
  echo "==> cargo build --release"
  cargo build --release
  BIN="$TARGET_DIR/release/subbar"
fi

echo "==> icon"
# Исходник — assets/icon.png (1024×1024, нарисован по assets/icon.svg).
# Промежуточные файлы — свои на каждый прогон: параллельная сборка стирала их из-под sips.
ICON_SRC="assets/icon.png"
ICONSET="$OUT/.icon.$$.iconset"
trap 'rm -rf "$ICONSET" "$OUT/.icon.$$.icns" "$OUT/.subbar.universal.$$"' EXIT
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  retina_size=$((size * 2))
  sips -z "$retina_size" "$retina_size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$OUT/.icon.$$.icns"
mv -f "$OUT/.icon.$$.icns" "$OUT/icon.icns"
rm -rf "$ICONSET"

echo "==> bundle"
# Хвосты сборок, убитых без EXIT: их pid мёртв — убираем, живые (параллельная сборка) не трогаем.
for old in "$OUT"/."$APP_NAME".tmp.*.app "$OUT"/."$APP_NAME".previous.*.app "$OUT"/.icon.*.iconset "$OUT"/.icon.*.icns "$OUT"/.subbar.universal.*; do
  [ -e "$old" ] || continue
  opid="${old%.app}"; opid="${opid%.iconset}"; opid="${opid%.icns}"; opid="${opid##*.}"
  case "$opid" in ''|*[!0-9]*) continue ;; esac
  kill -0 "$opid" 2>/dev/null || rm -rf "$old"
done
APP_TMP="$OUT/.$APP_NAME.tmp.$$.app"
APP_BACKUP="$OUT/.$APP_NAME.previous.$$.app"
cleanup_bundle() {
  set +e
  rm -rf "$APP_TMP" "$OUT/.subbar.universal.$$"
  if [ -e "$APP_BACKUP" ] && [ ! -e "$APP" ]; then
    mv "$APP_BACKUP" "$APP"
  fi
}
trap cleanup_bundle EXIT
mkdir -p "$APP_TMP/Contents/MacOS" "$APP_TMP/Contents/Resources"
cp "$BIN" "$APP_TMP/Contents/MacOS/$APP_NAME"
cp "$OUT/icon.icns" "$APP_TMP/Contents/Resources/icon.icns"
# claude-sub внутрь приложения: install.sh ставит на него симлинк, репозиторий может исчезнуть.
# Копия не зависит от своего места — все пути в скрипте абсолютные ($HOME/... или из PATH).
cp scripts/claude-sub "$APP_TMP/Contents/Resources/claude-sub"
chmod +x "$APP_TMP/Contents/Resources/claude-sub"

cat > "$APP_TMP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>$APP_NAME</string>
  <key>CFBundleIconFile</key><string>icon.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>SubBar</string>
</dict>
</plist>
PLIST

echo "==> ad-hoc codesign"
codesign --force --sign - "$APP_TMP"
codesign --verify --strict "$APP_TMP"

if [ -e "$APP" ]; then
  mv "$APP" "$APP_BACKUP"
fi
# Параллельная сборка успела положить свой бандл: mv в существующий каталог не падает, а вкладывает внутрь.
if [ -e "$APP" ]; then
  echo "dist/SubBar.app занят параллельной сборкой — повтори позже" >&2
  rm -rf "$APP_BACKUP"
  exit 1
fi
mv "$APP_TMP" "$APP"
if [ -e "$APP/$(basename "$APP_TMP")" ]; then
  rm -rf "${APP:?}/$(basename "$APP_TMP")"
  echo "dist/SubBar.app занят параллельной сборкой — повтори позже" >&2
  exit 1
fi
rm -rf "$APP_BACKUP" "$OUT/.subbar.universal.$$"
trap - EXIT

echo "==> размер"
du -sh "$APP"
echo "готово: $APP"
