# SubBar — инструкция для ИИ-агента

Ты — ИИ-агент (Claude Code, Cursor, Codex и т. п.), и пользователь попросил установить SubBar.
Делай по шагам, после каждого шага проверяй результат. Общайся с пользователем на его языке.

## Что это, в двух строках

SubBar — приложение для строки меню macOS + локальный прокси `127.0.0.1:8479`. Claude Code, запущенный через
`claude-sub`, шлёт запросы в прокси: основная модель уходит в Anthropic по подписке пользователя, а субагенты
`haiku` (по желанию и `sonnet`) с инструментами — в OpenCode Go.

## Шаг 0. Проверки (не пропускай)

```bash
sw_vers -productVersion        # нужно 13 или выше
uname -m                       # arm64 или x86_64 — оба годятся
command -v claude && claude --version
```

- Нет `claude` или пользователь не залогинен → попроси пользователя поставить Claude Code и **самому** войти:
  запустить `claude` и выполнить `/login`. Вход интерактивный, через браузер — ты его сделать не можешь.
  SubBar не требует отдельного входа: `claude-sub` использует ту же авторизацию.
- Больше ничего не нужно: установка скачивает готовое приложение. Rust и Xcode — только для
  [сборки из исходников](#запасной-путь-сборка-из-исходников).

## Шаг 1. Установка

```bash
curl -fsSL https://raw.githubusercontent.com/Lutamona/SubBar/main/scripts/get.sh | bash
```

Ожидаемо (обычно меньше минуты): скачан `SubBar.zip` из последнего релиза, контрольная сумма сошлась,
`/Applications/SubBar.app` запущен, `~/.local/bin/claude-sub` — симлинк, в выводе «SubBar работает (иконка в менюбаре)».

**Ты сам работаешь внутри `claude-sub`** (SubBar уже стоит, это обновление)? Установщик мягко перезапускает прокси,
через который идут твои же запросы, и ждёт новую версию до 3 минут — дольше обычного таймаута инструмента.
Запусти его в фоне и жди по PID:

```bash
nohup bash -c 'curl -fsSL https://raw.githubusercontent.com/Lutamona/SubBar/main/scripts/get.sh | bash' \
  > "${TMPDIR:-/tmp}/subbar-install.log" 2>&1 &
echo "PID $!"
# потом: kill -0 <PID> 2>/dev/null && echo "ещё идёт" || tail -20 "${TMPDIR:-/tmp}/subbar-install.log"
```

`~/.local/bin` не было в PATH — установщик сам допишет его в `~/.zshrc` (у bash — в `~/.bash_profile`) и скажет об
этом. Тогда `claude-sub` заработает в **новом** окне Терминала — передай это пользователю.

Проверка: `ls -l ~/.local/bin/claude-sub` и `pgrep -fl "SubBar.app/Contents/MacOS/SubBar"`.

macOS может показать окно доступа к связке ключей «Claude Code-credentials» — скажи пользователю нажать
«Разрешить всегда».

## Шаг 2. Ключ OpenCode Go

Ключ нужен от пользователя (подписка на https://opencode.ai). **Не проси вставить ключ в чат, если можно иначе** —
лучше попроси пользователя добавить его в окне: значок SubBar в строке меню → **+** → OpenCode Go → вставить ключ →
«Добавить».

Больше ничего включать не надо: первая карточка OpenCode Go сама становится основным ключом для субагентов,
а заодно сама встаёт служба прокси (работает и стартует при входе в систему) — если её не выключали руками
переключателем «Прокси как служба» или командой `proxy-service off`. Уже входил в OpenCode CLI — вместо вставки
ключа хватит кнопки **«Найти на этом Mac»** в той же форме.

Если пользователь сам дал ключ и просит добавить за него (ключ тогда попадёт в историю shell — предупреди):

```bash
/Applications/SubBar.app/Contents/MacOS/SubBar add opencode-go "OpenCode #1" apiKey=...
# «OpenCode #1» — основной ключ для субагентов
# Служба прокси включена: работает и стартует при входе
```

Проверка связи:

```bash
/Applications/SubBar.app/Contents/MacOS/SubBar proxy-check
# ✓ deepseek-v4.1-flash отвечает · ключ OpenCode #1 · 1261 мс · «ок»
```

Если «нет ключа» — проверь `SubBar proxy-config` (ключ замаскирован) и что подмена включена:
`SubBar proxy-config enabled=true`. Прокси не отвечает — `SubBar proxy-service on`.

## Шаг 3. Панель внизу Claude Code (строка состояния)

Одна команда, она сама правит `~/.claude/settings.json` (или `$CLAUDE_CONFIG_DIR/settings.json`):

```bash
/Applications/SubBar.app/Contents/MacOS/SubBar statusline install
```

- Если у пользователя уже есть своя `statusLine` — **не переписывай её руками**: команда сама оставит её первой
  строкой и добавит строку SubBar второй. Копия настроек — `settings.json.subbar-backup`.
- Если правишь вручную (нет доступа к команде), итог должен выглядеть так:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/Applications/SubBar.app/Contents/MacOS/SubBar statusline"
  }
}
```

- Убрать: `… statusline remove`.

Строка видна и в уже открытых сессиях после следующего ответа.

## Шаг 4. Правильный запуск

- Запускать **`claude-sub`** вместо `claude`. Все аргументы пробрасываются (`claude-sub --resume`, `claude-sub -p "…"`).
- Обычный `claude` идёт мимо прокси — субагенты будут на настоящих моделях Claude (строка покажет
  `настоящий Claude · не через claude-sub`).
- **Не выставляй** `ANTHROPIC_BASE_URL` глобально в `~/.zshrc` или `settings.json`: если прокси упадёт, Claude Code
  перестанет стартовать. `claude-sub` сам ставит его только когда прокси жив, иначе запускает обычный Claude.
- Хочет всегда через SubBar — можно алиас `alias claude='claude-sub'` в `~/.zshrc` (безопасно: внутри скрипта
  `claude-sub` алиасы не раскрываются, и он зовёт настоящий `claude`).
- Чтобы работа реально уходила в OpenCode, субагентов надо звать с `model: "haiku"` (или `"sonnet"`, если в SubBar
  выбрано «haiku и sonnet»). `claude-sub` добавляет основной модели об этом подсказку.
- Подмена идёт по модели, а не по роли: основная сессия на подменяемой модели (`claude-sub --model haiku`, или
  Sonnet в режиме «haiku и sonnet») тоже уйдёт в OpenCode. Если пользователю это не нужно — предупреди.

## Шаг 5. Итоговая проверка

```bash
curl -s 127.0.0.1:8479/_subbar/status | head -c 400; echo
```

Должно быть `"ok":true`, `"enabled":true`, `"hasKey":true`. Затем попроси пользователя запустить `claude-sub`,
дать задачу «найди через субагента haiku все TODO в проекте» и посмотреть строку внизу — там должно появиться
`deepseek-v4.1-flash · 1 аг · N отв`.

## Если что-то не так

| Симптом | Что делать |
| --- | --- |
| `прокси SubBar не запущен` | `/Applications/SubBar.app/Contents/MacOS/SubBar proxy-service restart` |
| `прокси SubBar не отвечает — запросы сессии не проходят` | то же `proxy-service restart`; не помогло — перезапустить `claude-sub` |
| `подмена выключена` | `… proxy-config enabled=true` или переключатель «Подмена» в окне |
| `нет ключа OpenCode` | добавить карточку OpenCode Go (шаг 2) |
| `… · N в Claude` растёт | OpenCode не отвечает/лимит; смотреть `tail -50 ~/Library/Logs/SubBar/proxy.log` |
| `claude-sub: command not found` | новое окно Терминала; не помогло — `~/.local/bin` нет в PATH (шаг 1) |
| «не удаётся проверить разработчика» | `xattr -dr com.apple.quarantine /Applications/SubBar.app` (ставили вручную из архива) |
| `get.sh`: «Не нашёл последний релиз» | нет доступа к GitHub — проверить интернет или собрать из исходников (ниже) |
| Claude Code просит войти | это вход самого Claude Code — пользователь делает `/login` в `claude`, SubBar ни при чём |

## Запасной путь: сборка из исходников

Только если установка одной строкой не прошла:

```bash
xcode-select -p >/dev/null 2>&1 || xcode-select --install   # пользователь жмёт «Установить» в окне
command -v cargo || { curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source "$HOME/.cargo/env"; }
git clone https://github.com/Lutamona/SubBar.git ~/SubBar   # если репозитория ещё нет
cd ~/SubBar && ./scripts/install.sh
```

Сборка идёт 2–5 минут — дольше таймаута инструмента, так что запускай `install.sh` так же в фоне через `nohup`
и жди по PID. Дальше — с шага 2.

Полный справочник — [REFERENCE.md](REFERENCE.md).
