#!/bin/bash
# Установка SubBar одной строкой — готовое приложение из последнего релиза на GitHub, без Rust и Xcode:
#   curl -fsSL https://raw.githubusercontent.com/Lutamona/SubBar/main/scripts/get.sh | bash
# Скачивает SubBar.zip и install.sh той же версии, сверяет контрольную сумму и ставит в /Applications.
# Всё внутри main: оборвись загрузка на середине, bash не выполнит ни строчки недокачанного скрипта.
set -euo pipefail

REPO="Lutamona/SubBar"

main() {
  [ "$(uname -s)" = "Darwin" ] || { echo "SubBar работает только на macOS" >&2; exit 1; }
  local major
  major="$(sw_vers -productVersion | cut -d. -f1)"
  [ "$major" -ge 13 ] || { echo "Нужна macOS 13 или новее (сейчас $(sw_vers -productVersion))" >&2; exit 1; }

  # Номер последнего релиза — из переадресации GitHub (…/releases/tag/v0.3.1): без API и его лимитов.
  local tag
  tag="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's#.*/tag/##')"
  case "$tag" in v[0-9]*) ;; *) echo "Не нашёл последний релиз SubBar на GitHub — проверь интернет и повтори" >&2; exit 1 ;; esac

  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  echo "==> скачиваю SubBar $tag"
  curl -fSL --progress-bar "https://github.com/$REPO/releases/download/$tag/SubBar.zip" -o "$TMP/SubBar.zip"
  curl -fsSL "https://github.com/$REPO/releases/download/$tag/SubBar.zip.sha256" -o "$TMP/SubBar.zip.sha256"
  curl -fsSL "https://raw.githubusercontent.com/$REPO/$tag/scripts/install.sh" -o "$TMP/install.sh"

  local expected actual
  expected="$(awk '{print $1; exit}' "$TMP/SubBar.zip.sha256")"
  actual="$(shasum -a 256 "$TMP/SubBar.zip" | awk '{print $1}')"
  [ -n "$expected" ] && [ "$expected" = "$actual" ] || { echo "Контрольная сумма SubBar.zip не сошлась — файл битый, повтори установку" >&2; exit 1; }

  ditto -x -k "$TMP/SubBar.zip" "$TMP"
  bash "$TMP/install.sh" --app "$TMP/SubBar.app" </dev/null
}

main "$@"
