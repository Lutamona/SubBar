mod app;
mod bootstrap;
mod model;
mod providers;
mod proxy;
mod state;
mod store;
mod ui;
mod util;

use model::ProviderId;

fn account_secrets(account: &model::Account) -> Vec<&str> {
    account
        .credentials
        .iter()
        .filter(|(key, value)| {
            // Пути и настройки — не секреты: «По пути «***» нет числа» прятало бы ровно то, что надо чинить.
            !value.is_empty()
                && !store::is_setting_key(key)
                // Как store: короткое «секретом» не считаем — замена «1» на *** испортила бы весь текст.
                && value.chars().count() >= 4
        })
        .map(|(_, value)| value.as_str())
        .collect()
}

fn redact_text(text: &str, secrets: &[&str]) -> String {
    // Длинные первыми, как store::sanitize_saved_text: иначе секрет-надстрока останется наполовину открытым.
    let mut secrets = secrets.to_vec();
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut redacted = text.to_string();
    for secret in secrets {
        redacted = redacted.replace(secret, "***");
    }
    redacted
}

fn redact_account_text(text: &str, account: &model::Account) -> String {
    redact_text(text, &account_secrets(account))
}

fn redact_usage(account: &model::Account, usage: &model::AccountUsage) -> model::AccountUsage {
    let mut usage = usage.clone();
    let secrets = account_secrets(account);
    if let Some(error) = usage.error.take() {
        let safe = redact_text(&error, &secrets);
        usage.error = Some(
            if safe.contains("://") || safe.to_ascii_lowercase().contains("bearer ") {
                store::HIDDEN_ERROR.to_string()
            } else {
                safe
            },
        );
    }
    if let Some(plan) = usage.plan_type.take() {
        usage.plan_type = Some(redact_text(&plan, &secrets));
    }
    // Служебный маркер store «Текст скрыт» пользователю — тем же текстом, что и скрытая ошибка.
    let unmask = |text: String| if text == "Текст скрыт" { store::HIDDEN_ERROR.to_string() } else { text };
    usage.plan_type = usage.plan_type.take().map(unmask);
    for note in &mut usage.notes {
        *note = unmask(redact_text(note, &secrets));
    }
    for window in &mut usage.windows {
        window.key = redact_text(&window.key, &secrets);
        window.label = redact_text(&window.label, &secrets);
        window.label = unmask(std::mem::take(&mut window.label));
        if let Some(note) = window.note.take() {
            window.note = Some(unmask(redact_text(&note, &secrets)));
        }
    }
    usage
}

fn safe_id(account: &model::Account) -> String {
    redact_account_text(&account.id, account)
}

/// Общий поиск подписки для всех команд: сначала точный id/label, иначе уникальный
/// префикс id. Точное совпадение важнее префиксного, одинаковые имена — всегда ошибка
/// («удалить» иначе снесло бы не ту подписку).
enum AccountMatch {
    Found(usize),
    NotFound,
    Ambiguous { ids: Vec<String>, by_label: bool },
}

fn find_account(accounts: &[model::Account], needle: &str) -> AccountMatch {
    // Пустая строка — префикс любого id: `remove "$UNSET"` снёс бы единственную подписку.
    if needle.trim().is_empty() {
        return AccountMatch::NotFound;
    }
    // Пробел от копипасты — не часть id или названия.
    let needle = needle.trim();
    let ids = |list: &[usize]| list.iter().map(|index| safe_id(&accounts[*index])).collect();
    let exact: Vec<usize> = accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| account.id == needle || account.label == needle)
        .map(|(index, _)| index)
        .collect();
    match exact.as_slice() {
        [index] => return AccountMatch::Found(*index),
        [] => {}
        many => return AccountMatch::Ambiguous { ids: ids(many), by_label: true },
    }
    let prefix: Vec<usize> = accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| account.id.starts_with(needle))
        .map(|(index, _)| index)
        .collect();
    match prefix.as_slice() {
        [index] => AccountMatch::Found(*index),
        [] => AccountMatch::NotFound,
        many => AccountMatch::Ambiguous { ids: ids(many), by_label: false },
    }
}

/// «2 подписки», «5 подписок» — число в сообщении о неоднозначном поиске.
fn plural_accounts(count: usize) -> String {
    util::plural(count as u64, "подписка", "подписки", "подписок")
}

/// Статус опроса по-русски: `{:?}` печатал бы английские Ok/Error.
fn status_ru(status: model::FetchStatus) -> &'static str {
    match status {
        model::FetchStatus::Ok => "обновлено",
        model::FetchStatus::Error => "ошибка",
        model::FetchStatus::Unavailable => "недоступно",
    }
}

fn report_ambiguous(needle: &str, ids: &[String], by_label: bool) {
    let what = if by_label { "Под именем" } else { "По префиксу" };
    let needle = needle.trim();
    eprintln!(
        "{what} «{needle}» подходят {} — укажи полный id:",
        plural_accounts(ids.len())
    );
    for id in ids {
        eprintln!("  {id}");
    }
}

fn redact_detected_text(text: &str, found: &model::DetectedCredential) -> String {
    let secrets: Vec<&str> = found
        .credentials
        .iter()
        .filter(|(key, value)| !value.is_empty() && !store::is_setting_key(key) && value.chars().count() >= 4)
        .map(|(_, value)| value.as_str())
        .collect();
    redact_text(text, &secrets)
}

/// Команды терминала без окна — полный список в `subbar help`.
/// Команды, у которых свой разбор аргументов: их «--help» общий help не перехватывает.
const PROXY_COMMANDS: [&str; 5] = ["proxy", "proxy-config", "proxy-service", "proxy-check", "statusline"];

fn run_cli(args: &[String]) -> i32 {
    // «subbar remove --help» не должен искать подписку по имени «--help». Только сразу после команды:
    // «add … apiKey=… --help» иначе молча не добавлял карточку и выходил с кодом 0.
    let wants_help = matches!(args.get(1).map(String::as_str), Some("--help" | "-h")) && !PROXY_COMMANDS.contains(&args[0].as_str());
    let first = if wants_help { Some("help") } else { args.first().map(|s| s.as_str()) };
    match first {
        Some("proxy") => proxy::run(&args[1..]),
        Some("proxy-config") => proxy::run_config(&args[1..]),
        Some("proxy-check") => proxy::run_check(),
        Some("statusline") => proxy::run_statusline(&args[1..]),
        Some("proxy-service") => {
            let result = match args.get(1).map(String::as_str) {
                Some("on") => proxy::control::install_agent().map(|()| "включена"),
                Some("off") => proxy::control::remove_agent().map(|()| "выключена"),
                Some("restart") => proxy::control::restart_agent().map(|()| "перезапускается (начатые запросы доделает)"),
                _ => Err("subbar proxy-service on|off|restart".to_string()),
            };
            match result {
                Ok(what) => {
                    println!("служба прокси: {what}");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        Some("accounts") => {
            let state = store::load_state();
            // Битый state.json отложен в сторону, и список пуст — это не «пусто», а сбой: скрипт должен это увидеть.
            if let Some(p) = store::SET_ASIDE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                // Отложенный файл заменён пустым состоянием: списка нет вовсе.
                eprintln!("state.json был битым и отложен в {} — карточки там; восстанови его", p.display());
                return 1;
            }
            if state.accounts.is_empty() {
                println!("Список пуст. Добавь подписку через UI или: subbar add opencode-go \"OpenCode 1\" apiKey=...");
                return 0;
            }
            for account in &state.accounts {
                let label = redact_account_text(&account.label, account);
                // Окна печатаем при любом статусе, как `usage`: keep_last_good хранит прошлые при упавшем опросе.
                let windows = |usage: &model::AccountUsage| {
                    usage
                        .windows
                        .iter()
                        .map(|window| {
                            let left = util::display_percent(util::remaining_percent(window.used_percent));
                            let reset =
                                util::format_reset_countdown(window.resets_at, store::now_ms())
                                    .map(|value| format!(" (сброс {value})"))
                                    .unwrap_or_default();
                            format!(
                                "{}: осталось {left}%{reset}",
                                redact_account_text(&window.label, account)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                };
                let line = match account.last_usage.as_ref() {
                    Some(usage) if matches!(usage.status, model::FetchStatus::Ok) => {
                        let line = windows(usage);
                        if line.is_empty() { "окон нет".to_string() } else { line }
                    }
                    // Та же защита, что в `usage`: адрес с токеном в тексте ошибки подстановкой не поймать.
                    Some(usage) => {
                        let error = redact_usage(account, usage).error.filter(|e| !e.is_empty());
                        let head = format!("{}: {}", status_ru(usage.status), error.unwrap_or_else(|| "причина не указана".to_string()));
                        let last = windows(usage);
                        if last.is_empty() { head } else { format!("{head} · прошлые: {last}") }
                    }
                    None => "нет данных".to_string(),
                };
                println!(
                    "{} {} [{}]  {}",
                    if account.enabled { "•" } else { "○" },
                    label,
                    account.provider.display_name(),
                    line
                );
            }
            0
        }
        Some("usage") => {
            let filter = args.get(1);
            let snapshot = store::load_state();
            // Битый файл — не «подписок нет»: карточки лежат в отложенной копии.
            if let Some(p) = store::SET_ASIDE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                eprintln!("state.json был битым и отложен в {} — восстанови его", p.display());
                return 1;
            }
            let accounts: Vec<_> = match filter {
                // Как окно: выключенные карточки без явного имени не опрашиваем.
                None => snapshot.accounts.iter().filter(|a| a.enabled).cloned().collect(),
                Some(needle) => match find_account(&snapshot.accounts, needle) {
                    AccountMatch::Found(index) => vec![snapshot.accounts[index].clone()],
                    AccountMatch::NotFound => {
                        eprintln!("Подписка не найдена");
                        return 1;
                    }
                    AccountMatch::Ambiguous { ids, by_label } => {
                        report_ambiguous(needle, &ids, by_label);
                        return 1;
                    }
                },
            };
            if accounts.is_empty() {
                // Ни одна не опрошена — для `subbar usage && …` это не успех.
                if snapshot.accounts.is_empty() {
                    eprintln!("Список пуст. Добавь подписку через UI или: subbar add …");
                } else {
                    eprintln!("Включённых подписок нет");
                }
                return 1;
            }

            // Опрос — вне замка, а запись — только если у карточки всё ещё те же
            // ключи и настройки сервиса. Медленный ответ не должен приклеить
            // старый расход или обновлённые токены к уже заменённому ключу.
            let mut results = Vec::with_capacity(accounts.len());
            // На диск — сырой итог: прошлые окна подставляются уже против диска (там они свежее снимка).
            let mut updates = Vec::with_capacity(accounts.len());
            for account in &accounts {
                // Как окно и bootstrap: паника одного опроса не роняет вывод по остальным карточкам.
                let (usage, patch) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| providers::fetch_account(account)))
                    .unwrap_or_else(|_| {
                        let usage = crate::model::AccountUsage {
                            status: crate::model::FetchStatus::Error,
                            windows: Vec::new(),
                            plan_type: None,
                            notes: Vec::new(),
                            error: Some("Внутренняя ошибка обновления".to_string()),
                            updated_at: store::now_ms(),
                            last_ok_at: account.last_usage.as_ref().and_then(|u| u.last_ok_at),
                        };
                        (usage, Vec::new())
                    });
                updates.push((account.clone(), usage.clone(), patch.clone()));
                let usage = state::keep_last_good(account.last_usage.as_ref(), usage);
                results.push((account.clone(), usage, patch));
            }
            let saved = store::with_state(move |disk| {
                let mut changed = false;
                let mut vanished = Vec::new();
                let mut replaced = Vec::new();
                for (snapshot, usage, patch) in updates {
                    let Some(account) = disk.accounts.iter_mut().find(|a| a.id == snapshot.id)
                    else {
                        vanished.push(snapshot.id.clone());
                        continue;
                    };
                    if !state::same_refresh_identity(account, &snapshot) {
                        // Ключ сменили, пока опрашивали: цифры старого ключа не записаны — и печатать их как итог нельзя.
                        replaced.push(snapshot.id.clone());
                        continue;
                    }
                    // Свежесть сравниваем только для лимитов: новый refreshToken нужен в любом случае
                    // (часы скакнули назад — иначе ротированный токен потерялся бы, а старый уже мёртв).
                    let newer_on_disk = account
                        .last_usage
                        .as_ref()
                        .is_some_and(|current| {
                            // Свой удачный опрос не уступает более позднему отказу на диске (окна там протухшие).
                            let ours_ok_theirs_failed = usage.status == model::FetchStatus::Ok && current.status != model::FetchStatus::Ok;
                            !ours_ok_theirs_failed
                                && (current.updated_at > usage.updated_at
                                    // Как в merge_state: отказ не перебивает удачный опрос окна.
                                    || (usage.status != model::FetchStatus::Ok && current.status == model::FetchStatus::Ok))
                        });
                    for (key, value) in patch {
                        let allowed = model::credential_fields(account.provider)
                            .iter()
                            .any(|field| field.key == key);
                        if allowed
                            && !value.is_empty()
                            && account.credentials.get(&key) != Some(&value)
                        {
                            account.credentials.insert(key, value);
                            changed = true;
                        }
                    }
                    if newer_on_disk {
                        continue;
                    }
                    let usage = state::keep_last_good(account.last_usage.as_ref(), usage);
                    if account.last_usage.as_ref() != Some(&usage) {
                        account.last_usage = Some(usage);
                        changed = true;
                    }
                }
                Ok(((vanished, replaced), changed))
            });
            let (vanished, replaced) = saved.as_ref().cloned().unwrap_or_default();
            let saved = saved.map(|_| ());
            // Сказать сразу, а не после итога: напечатанное ниже на диск не легло.
            if let Err(error) = &saved {
                eprintln!("Не удалось сохранить результаты обновления: {error} — ниже только то, что пришло сейчас");
            }

            let mut any_failed = false;
            for (account, raw_usage, patch) in &results {
                if vanished.contains(&account.id) {
                    // Удалили, пока опрашивали: печатать устаревшее и рапортовать успех нельзя.
                    eprintln!("{}: аккаунт удалён во время опроса", safe_id(account));
                    any_failed = true;
                    continue;
                }
                if replaced.contains(&account.id) {
                    eprintln!("{}: ключ подписки сменился во время опроса — повтори `subbar usage`", safe_id(account));
                    any_failed = true;
                    continue;
                }
                let mut display_account = account.clone();
                for (key, value) in patch {
                    if !value.is_empty() {
                        display_account
                            .credentials
                            .insert(key.clone(), value.clone());
                    }
                }
                let usage = redact_usage(&display_account, raw_usage);
                if !matches!(usage.status, crate::model::FetchStatus::Ok) {
                    any_failed = true;
                }
                println!(
                    "\n{} [{}] {}",
                    redact_account_text(&account.label, &display_account),
                    account.provider.display_name(),
                    status_ru(usage.status)
                );
                if let Some(plan) = usage.plan_type.as_ref().filter(|p| !p.is_empty()) {
                    println!("  тариф: {plan}");
                }
                // Окна прошлого удачного опроса под «ошибкой» — не свежие цифры: так и пишем.
                if usage.status != crate::model::FetchStatus::Ok && !usage.windows.is_empty() {
                    println!("  прошлые данные:");
                }
                for window in &usage.windows {
                    let left = util::display_percent(util::remaining_percent(window.used_percent));
                    let reset = util::format_reset_countdown(window.resets_at, store::now_ms())
                        .map(|value| format!("   сброс {value}"))
                        .unwrap_or_default();
                    println!("  {:10} осталось {:>3}%{reset}", window.label, left);
                }
                for note in usage.notes.iter().filter(|n| !n.is_empty()) {
                    println!("  · {note}");
                }
                if let Some(error) = usage.error.as_ref().filter(|e| !e.is_empty()) {
                    println!("  ошибка: {error}");
                }
            }
            match saved {
                // Скрипт `subbar usage && …` должен видеть, что опрос упал.
                Ok(()) => i32::from(any_failed),
                Err(_) => 1,
            }
        }
        Some("detect") => {
            let import = match &args[1..] {
                [] => false,
                [flag] if flag == "--import" => true,
                other => {
                    // Сами аргументы не печатаем: вставленный не туда ключ ушёл бы в терминал.
                    eprintln!("detect: неизвестные аргументы ({} шт.) — есть только --import", other.len());
                    return 1;
                }
            };
            let known = store::load_state();
            if let Some(p) = store::SET_ASIDE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                eprintln!("state.json был битым и отложен в {} — восстанови его", p.display());
                return 1;
            }
            let found = providers::detect_all();
            if found.is_empty() {
                println!("Ничего не найдено");
                return 0;
            }
            // Поиск и сетевые опросы идут вне замка файла. Запись — по свежему
            // снимку с диска, чтобы не потерять чужую запись.
            let mut prepared = Vec::new();
            for item in found {
                let exists = known
                    .accounts
                    .iter()
                    .any(|account| state::matches_detected_account(account, &item));
                let account = if import {
                    let mut account = model::Account {
                        id: store::new_id(),
                        provider: item.provider,
                        label: item.label.clone(),
                        enabled: true,
                        credentials: crate::state::trimmed_credentials(&item.credentials.clone().into_iter().collect()),
                        options: item.options.clone().into_iter().collect(),
                        created_at: store::now_ms(),
                        last_usage: None,
                        selected_model: None,
                        selected_window: None,
                    };
                    if !exists {
                        // Паника одного опроса не должна терять уже найденные карточки.
                        let (usage, patch) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| providers::fetch_account(&account)))
                            .unwrap_or_else(|_| {
                                let usage = crate::model::AccountUsage {
                                    status: crate::model::FetchStatus::Error,
                                    windows: Vec::new(),
                                    plan_type: None,
                                    notes: Vec::new(),
                                    error: Some("Внутренняя ошибка обновления".to_string()),
                                    updated_at: store::now_ms(),
                                    last_ok_at: None,
                                };
                                (usage, Vec::new())
                            });
                        for (key, value) in patch {
                            if model::credential_fields(account.provider)
                                .iter()
                                .any(|field| field.key == key)
                                && !value.is_empty()
                            {
                                account.credentials.insert(key, value);
                            }
                        }
                        account.last_usage = Some(usage);
                    }
                    Some(account)
                } else {
                    None
                };
                prepared.push((item, account));
            }
            if !import {
                for (item, _) in prepared {
                    let exists = known
                        .accounts
                        .iter()
                        .any(|account| state::matches_detected_account(account, &item));
                    println!(
                        "• {} [{}] ← {}{}",
                        redact_detected_text(&item.label, &item),
                        item.provider.display_name(),
                        item.source,
                        if exists {
                            " (уже добавлено)"
                        } else {
                            ""
                        }
                    );
                }
                return 0;
            }
            match store::with_state(move |disk| {
                let mut lines = Vec::new();
                let mut changed = false;
                for (item, account) in prepared {
                    let exists = disk
                        .accounts
                        .iter()
                        .any(|account| state::matches_detected_account(account, &item));
                    lines.push(format!(
                        "• {} [{}] ← {}{}",
                        redact_detected_text(&item.label, &item),
                        item.provider.display_name(),
                        item.source,
                        if exists {
                            " (уже добавлено)"
                        } else {
                            ""
                        }
                    ));
                    if !exists {
                        let Some(mut account) = account else { continue };
                        // Как окно: тёзка уже в списке — «#2», иначе поиск по имени перестал бы различать их.
                        let labels: Vec<String> = disk.accounts.iter().map(|a| a.label.clone()).collect();
                        account.label = state::unique_label(&labels, account.label);
                        if let Some(usage) = &account.last_usage {
                            let safe_usage = redact_usage(&account, usage);
                            lines.push(format!(
                                "  → добавлено, статус: {}{}",
                                status_ru(usage.status),
                                safe_usage
                                    .error
                                    .as_ref()
                                    .map(|e| format!(" ({e})"))
                                    .unwrap_or_default()
                            ));
                        } else {
                            lines.push(
                                "  → добавлено, данные появятся после обновления".to_string(),
                            );
                        }
                        disk.accounts.push(account);
                        changed = true;
                    }
                }
                Ok((lines, changed))
            }) {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    0
                }
                Err(error) => {
                    eprintln!("Не удалось импортировать аккаунты: {error}");
                    1
                }
            }
        }
        Some("add") => {
            let (Some(provider), Some(label)) = (args.get(1), args.get(2)) else {
                eprintln!("Как вызывать: subbar add <provider> <label> [key=value ...]");
                return 1;
            };
            // «add codex --help» иначе заводил карточку с названием «--help».
            if matches!(label.as_str(), "--help" | "-h") {
                eprintln!("Как вызывать: subbar add <provider> <label> [key=value ...]");
                return 1;
            }
            let provider = match provider.as_str() {
                "codex" => ProviderId::Codex,
                // Каноническая запись в state.json — «open-code-go»/«command-code»: её тоже принимаем.
                "opencode-go" | "open-code-go" => ProviderId::OpenCodeGo,
                "commandcode" | "command-code" => ProviderId::CommandCode,
                "devin" => ProviderId::Devin,
                "claude" => ProviderId::Claude,
                "custom" => ProviderId::Custom,
                _ => {
                    // Незнакомый аргумент может оказаться токеном, вставленным по ошибке.
                    eprintln!("Неизвестный провайдер. Доступны: codex, opencode-go, commandcode, devin, claude, custom");
                    return 1;
                }
            };
            let mut credentials = std::collections::BTreeMap::new();
            for pair in &args[3..] {
                match pair.split_once('=') {
                    Some((key, value)) => {
                        // Как форма: пустое не храним, пробелы режем — иначе сменится «личность» при первом сохранении.
                        let value = value.trim();
                        if key.trim().is_empty() {
                            // «=значение» — имени поля нет; само значение не печатаем: это может быть ключ.
                            eprintln!("Пропускаю аргумент без имени поля (нужно key=value)");
                        } else if value.is_empty() {
                            // Имя показываем, только если оно похоже на имя, а не на вставленный секрет.
                            let key = key.trim();
                            if key.len() <= 20 && key.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
                                eprintln!("Поле «{key}» пустое — пропускаю");
                            } else {
                                eprintln!("Пустое поле — пропускаю");
                            }
                        } else {
                            credentials.insert(key.trim().to_string(), value.to_string());
                        }
                    }
                    None => {
                        // Кривой аргумент сам может быть вставленным секретом.
                        // Опечатка («key value») не должна давать «Добавлено» карточку без ключа.
                        eprintln!("Аргумент без формата key=value — карточка не добавлена");
                        return 1;
                    }
                }
            }
            // Случайные секреты под незнакомым именем поля не храним.
            let unknown: Vec<&String> = credentials
                .keys()
                .filter(|key| !model::credential_fields(provider).iter().any(|field| field.key == key.as_str()))
                .collect();
            if !unknown.is_empty() {
                // Все сразу, а не по одному за проход. Имя показываем, только если оно похоже на имя, а не на секрет.
                let known = model::credential_fields(provider).iter().map(|f| f.key).collect::<Vec<_>>().join(", ");
                let known = if known.is_empty() { "полей нет — команда только с названием".to_string() } else { known };
                let named: Vec<String> = unknown
                    .iter()
                    .filter(|key| key.len() <= 20 && key.chars().all(|c| c.is_ascii_alphabetic() || c == '_'))
                    .map(|key| format!("«{key}»"))
                    .collect();
                if named.len() == unknown.len() {
                    eprintln!("Неизвестное поле {} — карточка не добавлена. Для этого сервиса: {known}", named.join(", "));
                } else {
                    eprintln!("Есть неизвестные поля — карточка не добавлена. Для этого сервиса: {known}");
                }
                return 1;
            }
            let label: String = label
                .chars()
                .filter(|ch| !ch.is_control())
                .collect::<String>()
                .trim()
                .to_string();
            if label.is_empty() {
                eprintln!("Укажи название аккаунта");
                return 1;
            }
            // Проверяем обязательные поля сервиса.
            let required: Vec<&str> = model::credential_fields(provider)
                .iter()
                .filter(|f| !f.optional)
                .map(|f| f.key)
                .collect();
            let missing: Vec<&str> = required
                .iter()
                .filter(|k| {
                    credentials
                        .get(**k)
                        .map(|value| value.trim().is_empty())
                        .unwrap_or(true)
                })
                .copied()
                .collect();
            if !missing.is_empty() {
                eprintln!("Не хватает полей: {}", missing.join(", "));
                return 1;
            }
            if let Err(why) = state::validate_credentials(provider, &credentials) {
                eprintln!("{why}");
                return 1;
            }
            let account = model::Account {
                id: store::new_id(),
                provider,
                label: label.clone(),
                enabled: true,
                // Как из окна: ключ с переводом строки из $'…' иначе считался бы другим.
                credentials: state::trimmed_credentials(&credentials),
                options: std::collections::BTreeMap::new(),
                created_at: store::now_ms(),
                last_usage: None,
                selected_model: None,
                selected_window: None,
            };
            let id = account.id.clone();
            let added = match store::with_state(move |disk| {
                if account.provider == ProviderId::Devin
                    && disk
                        .accounts
                        .iter()
                        .any(|existing| existing.provider == ProviderId::Devin)
                {
                    return Ok((None, false));
                }
                // Как окно: повтор названия нумеруем, иначе карточку не найти по имени (две подходят).
                // «Go #2» занято → «Go #3», а не «Go #2 #2» — тем же правилом, что окно.
                let mut account = account;
                let taken: Vec<String> = disk.accounts.iter().map(|a| a.label.clone()).collect();
                account.label = state::unique_label(&taken, account.label);
                let final_label = account.label.clone();
                disk.accounts.push(account);
                Ok((Some(final_label), true))
            }) {
                Ok(added) => added,
                Err(error) => {
                    eprintln!("Не удалось сохранить аккаунт: {error}");
                    return 1;
                }
            };
            let Some(final_label) = added else {
                eprintln!("Аккаунт Devin уже добавлен для локального CLI-профиля");
                return 1;
            };
            if final_label != label {
                println!("Название «{label}» уже занято — сохранил как «{final_label}»");
            }
            println!("Добавлено, id: {id}");
            0
        }
        Some("link-claude") => {
            let Some(needle) = args.get(1) else {
                eprintln!("Как вызывать: subbar link-claude <id|label>");
                return 1;
            };
            enum LinkResult {
                Linked,
                Already,
                NotFound,
                WrongProvider,
                Ambiguous { ids: Vec<String>, by_label: bool },
            }
            let result = store::with_state(|state| {
                let index = match find_account(&state.accounts, needle) {
                    AccountMatch::Found(index) => index,
                    AccountMatch::NotFound => return Ok((LinkResult::NotFound, false)),
                    AccountMatch::Ambiguous { ids, by_label } => {
                        return Ok((LinkResult::Ambiguous { ids, by_label }, false))
                    }
                };
                let account = &mut state.accounts[index];
                if account.provider != ProviderId::Claude {
                    return Ok((LinkResult::WrongProvider, false));
                }
                if account.options.get("claudeCodeSource").map(String::as_str) == Some("true") {
                    return Ok((LinkResult::Already, false));
                }
                account
                    .options
                    .insert("claudeCodeSource".into(), "true".into());
                account.last_usage = None; // прежние цифры могли относиться к другому токену
                account.selected_model = None;
                account.selected_window = None;
                Ok((LinkResult::Linked, true))
            });
            match result {
                Ok(LinkResult::Linked) => {
                    println!("Привязано к Claude Code");
                    0
                }
                Ok(LinkResult::Already) => {
                    println!("Уже привязано к Claude Code");
                    0
                }
                Ok(LinkResult::NotFound) => {
                    eprintln!("Подписка не найдена");
                    1
                }
                Ok(LinkResult::WrongProvider) => {
                    eprintln!("Это не аккаунт Claude");
                    1
                }
                Ok(LinkResult::Ambiguous { ids, by_label }) => {
                    report_ambiguous(needle, &ids, by_label);
                    1
                }
                Err(error) => {
                    eprintln!("Не удалось привязать аккаунт: {error}");
                    1
                }
            }
        }
        Some("remove") => {
            let Some(needle) = args.get(1) else {
                eprintln!("Как вызывать: subbar remove <id|label>");
                return 1;
            };
            enum RemoveResult {
                Removed,
                NotFound,
                Ambiguous { ids: Vec<String>, by_label: bool },
            }
            let result = store::with_state(|state| {
                let idx = match find_account(&state.accounts, needle) {
                    AccountMatch::Found(index) => index,
                    AccountMatch::NotFound => return Ok((RemoveResult::NotFound, false)),
                    AccountMatch::Ambiguous { ids, by_label } => {
                        return Ok((RemoveResult::Ambiguous { ids, by_label }, false))
                    }
                };
                state.accounts.remove(idx);
                Ok((RemoveResult::Removed, true))
            });
            match result {
                Ok(RemoveResult::Removed) => {
                    println!("Удалено");
                    0
                }
                Ok(RemoveResult::NotFound) => {
                    eprintln!("Подписка не найдена");
                    1
                }
                Ok(RemoveResult::Ambiguous { ids, by_label }) => {
                    report_ambiguous(needle, &ids, by_label);
                    1
                }
                Err(error) => {
                    eprintln!("Не удалось удалить аккаунт: {error}");
                    1
                }
            }
        }
        Some("help" | "--help" | "-h") | None => {
            println!(
                "SubBar\n\n  subbar accounts               — список подписок\n  subbar usage [id|label]       — обновить и показать лимиты\n  subbar detect [--import]      — найти входы на этом Mac\n  subbar add <provider> <label> [key=value ...]\n                                — добавить подписку (key=value — ключи и поля)\n  subbar remove <id|label>      — удалить подписку\n  subbar link-claude <id|label> — следить за токеном Claude Code\n\nСубагенты Claude на OpenCode Go:\n  subbar proxy                  — прокси (обычно служба)\n  subbar proxy-config [k=v …]   — показать/поменять настройки прокси\n  subbar proxy-check            — проверить связь: ключ, модель, время ответа\n  subbar proxy-service on|off|restart\n                                — служба прокси (автозапуск, мягкий перезапуск)\n  subbar statusline install|remove\n                                — строка «deepseek-v4.1-flash · 12 отв» внизу Claude Code\n  claude-sub                    — Claude Code через прокси\n"
            );
            0
        }
        Some(_) => {
            eprintln!("Неизвестная команда");
            1
        }
    }
}

fn main() {
    // args() паникует на аргументе не в UTF-8 — отвечаем понятной ошибкой.
    let Some(args) = std::env::args_os().skip(1).map(|a| a.into_string().ok()).collect::<Option<Vec<String>>>() else {
        eprintln!("Аргумент не в UTF-8 — не разбираю");
        std::process::exit(2);
    };
    if !args.is_empty() {
        std::process::exit(run_cli(&args));
    }
    if !app::acquire_single_instance() {
        eprintln!("[subbar] уже запущен — выходим");
        std::process::exit(1);
    }
    app::run();
}

#[cfg(test)]
mod find_tests {
    use super::*;

    fn account(id: &str, label: &str) -> model::Account {
        model::Account {
            id: id.to_string(),
            provider: ProviderId::Claude,
            label: label.to_string(),
            enabled: true,
            credentials: std::collections::BTreeMap::new(),
            options: std::collections::BTreeMap::new(),
            created_at: 0,
            last_usage: None,
            selected_model: None,
            selected_window: None,
        }
    }

    fn ids(accounts: &[model::Account], needle: &str) -> Option<Vec<String>> {
        match find_account(accounts, needle) {
            AccountMatch::Found(index) => Some(vec![safe_id(&accounts[index])]),
            AccountMatch::NotFound => None,
            AccountMatch::Ambiguous { ids, .. } => Some(ids),
        }
    }

    #[test]
    fn exact_id_and_label_win_over_prefix() {
        let accounts = vec![account("abcd-1111", "Рабочая"), account("abzz-2222", "Другая")];
        assert_eq!(ids(&accounts, "abcd-1111"), Some(vec!["abcd-1111".to_string()]));
        assert_eq!(ids(&accounts, "Рабочая"), Some(vec!["abcd-1111".to_string()]));
        // Префикс «abcd» — единственное совпадение по id.
        assert_eq!(ids(&accounts, "abcd"), Some(vec!["abcd-1111".to_string()]));
    }

    #[test]
    fn shared_prefix_is_an_error_for_every_command() {
        let accounts = vec![account("abcd-1111", "Первая"), account("abzz-2222", "Вторая")];
        let found = ids(&accounts, "ab");
        assert_eq!(found, Some(vec!["abcd-1111".to_string(), "abzz-2222".to_string()]));
    }

    #[test]
    fn duplicate_labels_stay_an_error() {
        let accounts = vec![account("id-a", "OpenCode"), account("id-b", "OpenCode")];
        let found = ids(&accounts, "OpenCode");
        assert_eq!(found, Some(vec!["id-a".to_string(), "id-b".to_string()]));
    }

    #[test]
    fn unknown_needle_is_not_found() {
        let accounts = vec![account("abcd-1111", "Рабочая")];
        assert_eq!(ids(&accounts, "zzz"), None);
        assert_eq!(ids(&accounts, ""), None, "пустое имя (незаданная переменная) не должно находить подписку");
        assert_eq!(ids(&accounts, "  "), None);
    }

    #[test]
    fn plural_forms_are_russian() {
        assert_eq!(status_ru(model::FetchStatus::Unavailable), "недоступно");
        assert_eq!(plural_accounts(1), "1 подписка");
        assert_eq!(plural_accounts(2), "2 подписки");
        assert_eq!(plural_accounts(5), "5 подписок");
        assert_eq!(plural_accounts(11), "11 подписок");
        assert_eq!(plural_accounts(21), "21 подписка");
    }

    #[test]
    fn secrets_are_hidden_but_short_values_do_not_eat_text() {
        let mut account = account("a", "x");
        account.credentials.insert("apiKey".into(), "sk-secret-123".into());
        account.credentials.insert("x".into(), "1".into());
        assert_eq!(redact_account_text("ключ sk-secret-123 отвергнут, 100%", &account), "ключ *** отвергнут, 100%");
        let usage = model::AccountUsage {
            status: model::FetchStatus::Error,
            windows: Vec::new(),
            plan_type: None,
            notes: Vec::new(),
            error: Some("GET https://x/?t=abc".into()),
            updated_at: 0,
            last_ok_at: None,
        };
        assert_eq!(redact_usage(&account, &usage).error.as_deref(), Some(store::HIDDEN_ERROR));
    }
}
