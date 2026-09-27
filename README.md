# SubBar

**Claude Code дешевле: основная сессия работает на твоей подписке Claude, а субагенты — на недорогих моделях OpenCode Go.**
Плюс все лимиты подписок (Claude, ChatGPT/Codex, OpenCode Go и др.) — кольцами в строке меню macOS.

> Только macOS 13+. Приложение собирается из исходников одной командой.

---

## Зачем это

Когда Claude Code раздаёт работу субагентам (поиск по коду, чтение файлов, однотипные правки), каждый из них
съедает лимит твоей подписки. SubBar ставит на твоём Mac маленький локальный прокси:

```
Claude Code ──► прокси SubBar (127.0.0.1:8479)
                  ├─ основная модель (Opus/Sonnet) ──► Anthropic, как обычно, по твоей подписке
                  └─ субагенты haiku (и по желанию sonnet) ──► OpenCode Go (deepseek и др.)
```

- Основная сессия уходит в Anthropic **байт в байт** — ничего не ломается.
- Твой вход в Claude (OAuth) **никогда** не уходит в OpenCode — туда идёт только ключ OpenCode.
- OpenCode не ответил — запрос сам откатывается на настоящий Claude, работа не встаёт.
- Кончился лимит на одном ключе OpenCode — берётся следующий (если их несколько).
- Прокси слушает только `127.0.0.1`, запросы из браузера отбиваются.

Внизу Claude Code появляется строка, по которой видно, что подмена работает:

```
deepseek-v4.1-flash · 3 аг · 12 отв
```

---

## Установка (5 минут)

### 1. Что нужно заранее

- **Claude Code**, и ты в нём уже залогинен своей подпиской. Проверь: `claude` запускается и отвечает.
  Если нет — поставь с [claude.com/claude-code](https://claude.com/claude-code) и войди (`claude` → `/login`).
- **Ключ OpenCode Go** — оформи подписку на [opencode.ai](https://opencode.ai) и скопируй API-ключ (начинается на `sk-`).
- **Инструменты разработчика** (если ещё нет) — вставь в Терминал по одной строке:

```bash
xcode-select --install                                           # компилятор Apple
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust (жми Enter на вопросы)
brew install node jq                                             # Node.js и jq (если нет Homebrew — brew.sh)
```

После установки Rust закрой и открой Терминал заново.

### 2. Скачать и установить

```bash
git clone https://github.com/Lutamona/SubBar.git
cd SubBar
./scripts/install.sh
```

Скрипт соберёт приложение, положит его в `/Applications/SubBar.app`, запустит и добавит команду `claude-sub`
в `~/.local/bin`. Если в конце он попросит добавить `~/.local/bin` в PATH — выполни:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
```

> macOS может спросить разрешение на доступ к связке ключей «Claude Code-credentials» — нажми **«Разрешить всегда»**.
> Так SubBar читает лимиты твоей подписки Claude (токен никуда, кроме Anthropic, не уходит).

### 3. Добавить ключ OpenCode Go

1. Нажми на значок SubBar в строке меню (кольца вверху экрана).
2. Кнопка **+** → сервис **OpenCode Go** → вставь ключ → **Добавить**.
   Несколько ключей — несколько карточек («OpenCode #1», «OpenCode #2»…), SubBar будет переключаться между ними сам.
3. Открой экран **«Субагенты»** (кнопка **⇄**), включи **«Подмена»** и нажми **«Проверить»**.
   Должно появиться: `✓ Отвечает за 1,2 с`.

Карточка Claude добавится сама. ChatGPT/Codex, Command Code, Devin можно подтянуть кнопкой **«Найти на этом Mac»**
в той же форме — это только для показа лимитов, для подмены не нужно.

### 4. Включить строку внизу Claude Code

На экране «Субагенты» → **«Строка в Claude Code»**, или в Терминале:

```bash
/Applications/SubBar.app/Contents/MacOS/SubBar statusline install
```

Если у тебя уже была своя строка состояния — она останется первой, строка SubBar добавится второй.
Копия настроек перед правкой: `~/.claude/settings.json.subbar-backup`. Убрать: то же с `remove`.

### 5. Запускать

```bash
claude-sub
```

Вместо `claude` — всё остальное как обычно (любые флаги тоже работают: `claude-sub --resume`, `claude-sub -p "..."`).
Входить заново **не нужно**: `claude-sub` использует тот же вход, что и `claude`.

Попроси Claude «раздай это субагентам» — он возьмёт субагентов `haiku`, и они уйдут в OpenCode.
`claude-sub` сам подсказывает основной модели, что субагенты теперь дешёвые (отключить: `SUBBAR_HINT=0 claude-sub`).

---

## Как понять, что всё работает

Строка внизу Claude Code:

| Что видно | Что значит |
| --- | --- |
| `deepseek-v4.1-flash · 3 аг · 12 отв` (зелёная) | всё ок: 3 субагента, 12 ответов через OpenCode |
| `… · пока не запускались` | прокси работает, субагентов ещё не было |
| `… · 2 в Claude` (янтарная) | часть запросов откатилась на настоящий Claude (тратит подписку) |
| `… · 1 ош` (красная) | были ошибки |
| `настоящий Claude · не через claude-sub` | ты запустил обычный `claude` |
| `настоящий Claude · прокси SubBar не запущен` | служба прокси лежит — открой SubBar, экран «Субагенты» → ↻ |

Проверка из Терминала:

```bash
curl -s 127.0.0.1:8479/_subbar/status        # статус прокси
tail -f ~/Library/Logs/SubBar/proxy.log      # журнал
```

---

## Частые вопросы

**Это безопасно для моей подписки Claude?**
Основная сессия идёт в Anthropic ровно так же, как без SubBar. Токен подписки в OpenCode не уходит — на это есть тест.

**Какие модели можно выбрать для субагентов?**
На экране «Субагенты»: `deepseek-v4.1-flash`, `space-bunny-free`, `muse-spark-1.3-contributor`, плюс уровень размышления.

**А sonnet тоже можно подменять?**
Да: «Поведение» → «Кого подменять» → «haiku и sonnet».

**Прокси не запущен — Claude сломается?**
Нет. `claude-sub` предупредит и запустит обычный Claude.

**Где мои данные?**
`~/Library/Application Support/SubBar/` (`state.json`, `proxy.json`, права только у тебя). В репозитории и в сети — ничего.

**Как удалить?**
```bash
/Applications/SubBar.app/Contents/MacOS/SubBar statusline remove
/Applications/SubBar.app/Contents/MacOS/SubBar proxy-service off
rm -rf /Applications/SubBar.app ~/.local/bin/claude-sub "$HOME/Library/Application Support/SubBar"
rm -f ~/Library/LaunchAgents/ai.subbar.plist ~/Library/LaunchAgents/ai.subbar.proxy.plist
```

---

## Ставишь через нейросеть?

Дай своему Claude Code / Cursor / Codex ссылку на этот репозиторий и скажи «установи по [docs/FOR-AI.md](docs/FOR-AI.md)».
Там пошаговая инструкция для агента: установка, строка внизу, правильный запуск, проверка.

## Подробности

- [docs/REFERENCE.md](docs/REFERENCE.md) — полный справочник: все экраны и настройки, CLI, как устроен прокси, живые тесты.
- Разработка: `cargo test` (юнит-тесты + сквозные тесты прокси на фейковых серверах), `./scripts/make-app.sh` — собрать `.app`.

## Лицензия

[MIT](LICENSE). Проект не связан с Anthropic и OpenCode.
