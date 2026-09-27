#!/bin/bash
# Builds and installs SubBar into /Applications, then restarts it.
# Окно и служба прокси — один бинарь, но разные процессы: окно — без аргументов,
# служба — «SubBar proxy». Окно перезапускаем, службу — мягко (доделает начатые запросы).
set -euo pipefail
cd "$(dirname "$0")/.."

# Одна установка за раз: две параллельные отнимали друг у друга бандл в /Applications
# и принимали чужое окно за своё. Замок — каталог (mkdir атомарен), с pid владельца.
LOCK="${TMPDIR:-/tmp}/subbar-install.lock"
if ! mkdir "$LOCK" 2>/dev/null; then
  lpid=$(cat "$LOCK/pid" 2>/dev/null || true)
  # Пустой pid — хозяин мог только что взять замок и ещё не записаться: дать ему секунду.
  [ -n "$lpid" ] || { sleep 1; lpid=$(cat "$LOCK/pid" 2>/dev/null || true); }
  if [ -n "$lpid" ] && kill -0 "$lpid" 2>/dev/null; then
    echo "Уже идёт другая установка SubBar (pid $lpid) — дождись её" >&2
    exit 1
  fi
  # Хозяин умер, не сняв замок (kill -9) — забираем.
  rm -rf "$LOCK"
  mkdir "$LOCK" || { echo "Не смог взять замок установки $LOCK" >&2; exit 1; }
fi
echo $$ > "$LOCK/pid"
release_lock() { rm -rf "$LOCK"; }
trap release_lock EXIT
# EXIT при kill/Ctrl-C bash не зовёт — сигнал переводим в обычный выход, и откат отрабатывает.
trap 'exit 130' INT TERM HUP

./scripts/make-app.sh

# awk дочитывает до конца: head под pipefail ронял бы скрипт SIGPIPE-ом, будь строк version две.
VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | awk 'NR == 1')"
# Пустая версия превратила бы сверку «прокси новой версии» в «"" = ""» — ложный успех.
[ -n "$VERSION" ] || { echo "Не прочитал version из Cargo.toml" >&2; exit 1; }
WINDOW_RE='SubBar\.app/Contents/MacOS/SubBar$'   # только окно: у службы в конце « proxy»
LABEL="ai.subbar.proxy"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

echo "==> установка в /Applications"
INSTALLED="/Applications/SubBar.app"
STAGED="/Applications/.SubBar.new.$$.app"
BACKUP="/Applications/.SubBar.previous.$$.app"
installed_ok=0
# Только что убитое окно LaunchServices какое-то время считает живым: `open` тогда будит несуществующий
# процесс и падает с -600 — повторяем, пока система не отпустит старый экземпляр.
launch() {
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12; do
    open -a "$1" 2>/dev/null && return 0
    sleep 0.5
  done
  open -a "$1"
}
cleanup() {
  # Откат под set -e оборвался бы на первом сбое rm — и не дошёл бы до возврата прежнего бандла.
  set +e
  if [ "$installed_ok" -eq 0 ] && [ -e "$BACKUP" ]; then
    # Сначала убираем новое в сторону, а не стираем: если mv не пройдёт, прежний бандл не пропадёт.
    FAILED="$(dirname "$INSTALLED")/.SubBar.failed.$$.app"
    if mv "$INSTALLED" "$FAILED" 2>/dev/null || [ ! -e "$INSTALLED" ]; then
      if mv "$BACKUP" "$INSTALLED"; then
        rm -rf "$FAILED"
      else
        # Прежний не вернулся — хотя бы новый обратно на место, чтобы не остаться без приложения.
        [ -e "$FAILED" ] && mv "$FAILED" "$INSTALLED"
        echo "Откат не удался: прежний SubBar остался в $BACKUP" >&2
      fi
    fi
    # В откате без двенадцати повторов: после Ctrl-C человек иначе ждал бы ещё до 6 с молча.
    open -a "$INSTALLED" >/dev/null 2>&1 || true
  elif [ "$installed_ok" -eq 0 ] && [ "$window_killed" -eq 1 ] && [ -e "$INSTALLED" ]; then
    # Окно уже погасили, а до подмены не дошли (Ctrl-C, таймаут) — вернуть прежнее.
    open -a "$INSTALLED" >/dev/null 2>&1 || true
  fi
  rm -rf "$STAGED"
  release_lock
}
window_killed=0
# Хвосты установок, убитых без EXIT (kill -9, перезагрузка): скрытые полные бандлы копятся навсегда.
for old in /Applications/.SubBar.new.*.app /Applications/.SubBar.previous.*.app /Applications/.SubBar.failed.*.app /Applications/.SubBar.test.*.app; do
  [ -e "$old" ] || continue
  opid="${old%.app}"; opid="${opid##*.}"
  case "$opid" in ''|*[!0-9]*) continue ;; esac
  kill -0 "$opid" 2>/dev/null && continue
  # Установку убили между «убрал прежний» и «положил новый»: прежний бандл — единственная копия.
  case "$old" in
    */.SubBar.previous.*.app) if [ ! -e "$INSTALLED" ]; then if mv "$old" "$INSTALLED"; then echo "Вернул прежний SubBar из $old" >&2; else echo "Не смог вернуть прежний SubBar из $old — верни его вручную" >&2; fi; continue; fi ;;
  esac
  rm -rf "$old"
done
trap cleanup EXIT
ditto dist/SubBar.app "$STAGED"
codesign --verify --strict "$STAGED"
window_killed=1
pkill -f "$WINDOW_RE" 2>/dev/null || true
for _ in $(seq 1 20); do
  if ! pgrep -f "$WINDOW_RE" >/dev/null; then
    break
  fi
  sleep 0.25
done
if pgrep -f "$WINDOW_RE" >/dev/null; then
  echo "Старая версия SubBar не завершилась" >&2
  exit 1
fi
if [ -e "$INSTALLED" ]; then
  mv "$INSTALLED" "$BACKUP"
fi
# Путь снова занят (параллельная установка) — mv положил бы бандл ВНУТРЬ и отчитался успехом.
if [ -e "$INSTALLED" ]; then
  echo "Параллельная установка заняла $INSTALLED — прерываюсь, её бандл не трогаю" >&2
  # Откат снёс бы чужой свежий бандл ради нашего старого — наш бэкап больше не нужен.
  rm -rf "$BACKUP"
  exit 1
fi
mv "$STAGED" "$INSTALLED"
# Путь заняли между проверкой и mv — наш бандл лёг внутрь чужого: вынуть и прерваться.
if [ -e "$INSTALLED/$(basename "$STAGED")" ]; then
  rm -rf "$INSTALLED/$(basename "$STAGED")"
  echo "Параллельная установка заняла $INSTALLED — прерываюсь" >&2
  rm -rf "$BACKUP"
  exit 1
fi

echo "==> запуск"
launch "$INSTALLED" || true # диагностика ниже, а не молчаливый выход по set -e
started=0
# Холодный старт свежеподписанного бинаря бывает дольше 10 с (проверка подписи) — ждём обычно до 30 с (потолок цикла — около 150 с).
# И окно должно прожить секунду: упавшее на старте не считается «работает».
for _ in $(seq 1 120); do
  if pgrep -f "$WINDOW_RE" >/dev/null; then
    sleep 1
    if pgrep -f "$WINDOW_RE" >/dev/null; then
      started=1
      break
    fi
  fi
  sleep 0.25
done
if [ "$started" -eq 1 ]; then
  installed_ok=1
  rm -rf "$BACKUP"
  trap release_lock EXIT
  echo "SubBar работает (иконка в менюбаре)"
  # Дальше откатывать нечего: приложение уже стоит и запущено. Проблемы со службой прокси —
  # это отдельная поломка, откат вернул бы старое приложение, но не починил бы прокси.
  # claude-sub — в PATH (иначе кнопка «Скопировать claude-sub» копирует несуществующую команду).
  # Ссылаемся на копию внутри приложения, а не на файл в репозитории: репозиторий могут убрать.
  CLAUDE_SUB="$INSTALLED/Contents/Resources/claude-sub"
  [ -x "$CLAUDE_SUB" ] || { echo "⚠ нет $CLAUDE_SUB — ссылаюсь на scripts/claude-sub в репозитории" >&2; CLAUDE_SUB="$PWD/scripts/claude-sub"; }
  [ -x "$CLAUDE_SUB" ] || echo "⚠ claude-sub не найден и в репозитории ($CLAUDE_SUB) — ссылка в PATH будет битой" >&2
  # Не фатально: под set -e падение mkdir оборвало бы скрипт до перезапуска службы.
  # На месте ссылки каталог — ln положил бы ссылку внутрь него, а claude-sub в PATH остался бы сломан.
  if [ -d "$HOME/.local/bin/claude-sub" ] && [ ! -L "$HOME/.local/bin/claude-sub" ]; then
    echo "⚠ ~/.local/bin/claude-sub — каталог, не трогаю его; убери и повтори установку" >&2
  elif ! { mkdir -p "$HOME/.local/bin" && ln -sfn "$CLAUDE_SUB" "$HOME/.local/bin/claude-sub"; }; then
    echo "⚠ не смог положить claude-sub в ~/.local/bin" >&2
  fi
  case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "⚠ ~/.local/bin нет в PATH — добавь в ~/.zshrc: export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2 ;;
  esac
  if [ -f "$PLIST" ]; then
    echo "==> служба прокси: мягкий перезапуск на $VERSION (начатые запросы доделает — обычно до минуты, при занятой службе до нескольких минут — это не зависание)"
    if ! "$INSTALLED/Contents/MacOS/SubBar" proxy-service on; then
      echo "Приложение установлено, но служба прокси не встала — смотри ~/Library/Logs/SubBar/proxy.log" >&2
      exit 2
    fi
    # Путь к конфигу — как у самой службы: из её plist, а не из окружения этого терминала.
    plist_env() { /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$1" "$PLIST" 2>/dev/null || true; }
    P_CONF=$(plist_env SUBBAR_PROXY_CONFIG); P_DATA=$(plist_env LIMITBAR_DATA_DIR)
    if [ -n "$P_CONF" ]; then CONF="$P_CONF"
    elif [ -n "$P_DATA" ]; then CONF="$P_DATA/proxy.json"
    else CONF="${SUBBAR_PROXY_CONFIG:-${LIMITBAR_DATA_DIR:-$HOME/Library/Application Support/SubBar}/proxy.json}"
    fi
    # Порт: jq — основной источник, grep-запас только если jq недоступен или ключа нет.
    PORT=$( (command -v jq >/dev/null 2>&1 && jq -r '.port // empty' "$CONF" 2>/dev/null) || true )
    case "$PORT" in ''|*[!0-9]*)
      # Запас без jq: первое вхождение ключа "port" (сегодня конфиг плоский; порт бывает и строкой — "8479").
      # Без pipefail: head закрывает трубу раньше grep, и SIGPIPE стирал бы найденный порт.
      PORT=$(set +o pipefail; grep -oE '"port"[[:space:]]*:[[:space:]]*"?[0-9]+' "$CONF" 2>/dev/null | head -n 1 | grep -oE '[0-9]+' | head -n 1 || true)
      ;;
    esac
    case "$PORT" in ''|*[!0-9]*) PORT=8479 ;; esac
    # Старый прокси доделывает начатое до минуты — ждём новую версию до 3 минут.
    # Считаем по часам, а не по итерациям: curl -m 1 + sleep растягивали «3 минуты» до девяти.
    command -v curl >/dev/null || { echo "Приложение установлено, но ответ прокси не проверил: нет curl" >&2; exit 2; }
    deadline=$((SECONDS + 180))
    while [ "$SECONDS" -lt "$deadline" ]; do
      status=$(curl -s -m 1 --noproxy "*" "http://127.0.0.1:$PORT/_subbar/status" 2>/dev/null || true)
      # Первое вхождение: версия стоит на верхнем уровне ответа, раньше любых вложенных полей.
      running=$(printf '%s' "$status" | grep -o '"version":"[^"]*"' | head -n 1 | cut -d'"' -f4 || true)
      # Отвечать может и ручной `subbar proxy` той же версии, а служба в это время в краш-цикле:
      # верим только ответу процесса самой службы.
      rpid=$(printf '%s' "$status" | grep -o '"pid":[0-9]*' | head -n 1 | cut -d: -f2 || true)
      apid=$(launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null | sed -n 's/^[[:space:]]*pid = \([0-9]*\).*/\1/p' | head -n 1 || true)
      # Служба ещё не поднялась (pid пуст) — тоже не верим: ответить мог ручной прокси.
      if [ -z "$status" ]; then running="не отвечает"
      elif [ -z "$apid" ] || [ "$rpid" != "$apid" ]; then running="чужой процесс pid ${rpid:-?}"
      # Старый процесс той же версии ещё доделывает начатое — это не новый.
      elif printf '%s' "$status" | grep -q '"draining":true'; then running="старый процесс доделывает начатое"
      fi
      [ "$running" = "$VERSION" ] && break
      sleep 0.5
    done
    if [ "$running" = "$VERSION" ]; then
      echo "Прокси $VERSION отвечает на :$PORT"
    else
      # Приложение на месте и работает — откатывать нечего, прокси чинится отдельно (лог/перезапуск).
      echo "Приложение установлено и запущено, но прокси не ответил на $VERSION за 3 минуты (сейчас: ${running:-не отвечает}) — смотри ~/Library/Logs/SubBar/proxy.log" >&2
      exit 2
    fi
  else
    echo "Служба прокси не установлена — включить: subbar proxy-service on"
  fi
else
  echo "SubBar не запустился; если была предыдущая версия, она будет восстановлена" >&2
  exit 1
fi
