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
command -v cargo node jq git
```

- Нет `claude` или пользователь не залогинен → попроси пользователя поставить Claude Code и **самому** войти:
  запустить `claude` и выполнить `/login`. Вход интерактивный, через браузер — ты его сделать не можешь.
  SubBar не требует отдельного входа: `claude-sub` использует ту же авторизацию.
- Нет `cargo` → `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source "$HOME/.cargo/env"`.
- Нет `node` / `jq` → `brew install node jq` (нет brew — попроси пользователя поставить с brew.sh).
- Нет компилятора (`xcode-select -p` падает) → `xcode-select --install`, пользователь жмёт «Установить» в окне.

## Шаг 1. Установка

```bash
git clone https://github.com/Lutamona/SubBar.git ~/SubBar   # если репозитория ещё нет
cd ~/SubBar && ./scripts/install.sh
```

Ожидаемо: сборка 2–5 минут, затем `/Applications/SubBar.app` запущен, `~/.local/bin/claude-sub` — симлинк.
Если скрипт предупредил, что `~/.local/bin` нет в PATH:

```bash
grep -q '.local/bin' ~/.zshrc || echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
```

Проверка: `ls -l ~/.local/bin/claude-sub` и `pgrep -fl "SubBar.app/Contents/MacOS/SubBar"`.

macOS может показать окно доступа к связке ключей «Claude Code-credentials» — скажи пользователю нажать
«Разрешить всегда».

## Шаг 2. Ключ OpenCode Go

Ключ нужен от пользователя (подписка на https://opencode.ai). **Не проси вставить ключ в чат, если можно иначе** —
лучше попроси пользователя добавить его в окне: значок SubBar в строке меню → **+** → OpenCode Go → вставить ключ →
«Добавить». Затем экран «Субагенты» (кнопка ⇄) → включить «Подмена».

Если пользователь сам дал ключ и просит добавить за него (ключ тогда попадёт в историю shell — предупреди):

```bash
/Applications/SubBar.app/Contents/MacOS/SubBar add opencode-go "OpenCode #1" apiKey=sk-...
```

Проверка связи:

```bash
/Applications/SubBar.app/Contents/MacOS/SubBar proxy-check
# ✓ deepseek-v4.1-flash отвечает · ключ OpenCode #1 · 1261 мс · «ок»
```

Если «нет ключа» — проверь `SubBar proxy-config` (ключ замаскирован) и что подмена включена:
`SubBar proxy-config enabled=true`.

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
| `подмена выключена` | `… proxy-config enabled=true` или переключатель «Подмена» в окне |
| `нет ключа OpenCode` | добавить карточку OpenCode Go (шаг 2) |
| `… · N в Claude` растёт | OpenCode не отвечает/лимит; смотреть `tail -50 ~/Library/Logs/SubBar/proxy.log` |
| `claude-sub: command not found` | `~/.local/bin` не в PATH (шаг 1) |
| Claude Code просит войти | это вход самого Claude Code — пользователь делает `/login` в `claude`, SubBar ни при чём |

Полный справочник — [REFERENCE.md](REFERENCE.md).
