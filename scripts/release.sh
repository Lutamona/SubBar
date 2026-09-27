#!/bin/bash
# Выпустить релиз на GitHub: универсальная сборка (Apple Silicon + Intel), SubBar.zip и его контрольная сумма.
# Версия — из Cargo.toml, тег v<версия>. Нужен gh, вошедший в аккаунт с правом записи в репозиторий.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | awk 'NR == 1')"
[ -n "$VERSION" ] || { echo "Не прочитал version из Cargo.toml" >&2; exit 1; }
TAG="v$VERSION"

command -v gh >/dev/null 2>&1 || { echo "Нужен gh: brew install gh && gh auth login" >&2; exit 127; }
[ -z "$(git status --porcelain)" ] || { echo "В рабочем каталоге незакоммиченные правки — релиз собирается только из коммита" >&2; exit 1; }
if gh release view "$TAG" >/dev/null 2>&1; then
  echo "Релиз $TAG уже есть — подними version в Cargo.toml" >&2
  exit 1
fi

./scripts/make-app.sh --universal

ZIP="dist/SubBar.zip"
rm -f "$ZIP" "$ZIP.sha256"
ditto -c -k --keepParent dist/SubBar.app "$ZIP"
(cd dist && shasum -a 256 SubBar.zip > SubBar.zip.sha256)
echo "==> $(cat "$ZIP.sha256")"

git push origin HEAD
gh release create "$TAG" "$ZIP" "$ZIP.sha256" \
  --target "$(git rev-parse HEAD)" \
  --title "SubBar $VERSION" \
  --notes "Установка одной строкой (macOS 13+, Apple Silicon и Intel):

\`\`\`bash
curl -fsSL https://raw.githubusercontent.com/Lutamona/SubBar/main/scripts/get.sh | bash
\`\`\`

Или вручную: скачай SubBar.zip, распакуй и запусти \`install.sh --app SubBar.app\` из репозитория. Подробности — в README."
echo "готово: $(gh release view "$TAG" --json url -q .url)"
