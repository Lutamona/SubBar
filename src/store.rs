use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::model::State;

/// Каталог данных. `LIMITBAR_DATA_DIR` подменяет его — тесты, снимки, отдельные стенды.
pub fn data_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("LIMITBAR_DATA_DIR") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom.trim());
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Ключи никогда не кладём в общий предсказуемый /tmp/Library,
            // если GUI-запускатор не передал HOME. Конечный каталог делаем приватным
            // и перед работой проверяем его владельца.
            std::env::temp_dir().join(format!("subbar-{}", unsafe { libc::geteuid() }))
        });
    home.join("Library")
        .join("Application Support")
        .join("SubBar")
}

pub fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

fn lock_path() -> PathBuf {
    data_dir().join("state.lock")
}

fn ensure_private_data_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    // Итоговый каталог создаём приватным с первого inode: иначе только что
    // записанные ключи на миг были бы открыты при слишком щедрой
    // umask процесса.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "каталог данных SubBar должен быть обычным каталогом",
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "каталог данных SubBar принадлежит другому пользователю",
        ));
    }

    // Свой путь может указывать на домашний каталог или его родителя. Такой
    // широкий каталог молча не chmod-им — просим отдельный каталог данных.
    let has_custom_dir = std::env::var("LIMITBAR_DATA_DIR")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if has_custom_dir {
        let canonical_dir = fs::canonicalize(dir)?;
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            if let Ok(home) = fs::canonicalize(home) {
                if home == canonical_dir || home.starts_with(&canonical_dir) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "LIMITBAR_DATA_DIR должен указывать на отдельный каталог",
                    ));
                }
            }
        }

        // LIMITBAR_DATA_DIR задуман для отдельного каталога состояния.
        // Не chmod-им любой существующий каталог (например рабочее дерево)
        // только потому, что его передали по ошибке.
        let allowed = |name: &std::ffi::OsStr| {
            let name = name.to_string_lossy();
            name == "state.json"
                || name == "state.lock"
                // замок «одно окно» — файл самого приложения
                || name == "limitbar.lock"
                // настройки прокси лежат рядом со state.json
                || name == "proxy.json"
                // учёт сессий прокси пишется рядом с proxy.json
                || name == "proxy-sessions.json"
                || (name.starts_with("proxy-sessions.json.") && name.ends_with(".tmp"))
                || (name.starts_with("proxy.json.") && name.ends_with(".tmp"))
                // недельные паузы ключей прокси и замок обновления токена Claude
                || name == "proxy-paused.json"
                || (name.starts_with("proxy-paused.json.") && name.ends_with(".tmp"))
                || name == "claude-refresh.lock"
                // служебные файлы Finder и прочие скрытые — не повод отказать
                || name.starts_with('.')
                || name.starts_with("state.corrupt.")
                || (name.starts_with("state.json.") && name.ends_with(".tmp"))
        };
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if !allowed(&entry.file_name()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "LIMITBAR_DATA_DIR должен быть отдельным каталогом данных: лишний файл «{}»",
                        entry.file_name().to_string_lossy()
                    ),
                ));
            }
        }
    }

    if metadata.mode() & 0o7777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Замок файла, общий для окна и CLI: `load → изменить → save` остаётся атомарным.
/// Возвращает дескриптор замка; вызывающий держит его всю критическую секцию.
pub(crate) fn acquire_state_lock() -> io::Result<fs::File> {
    acquire_state_lock_within(None)
}

/// Как `acquire_state_lock`, но не дольше `limit`: `flock` на macOS не слушает O_NONBLOCK, и
/// поток без срока висел бы, пока окно держит замок (у прокси такие потоки копились).
pub(crate) fn acquire_state_lock_within(limit: Option<std::time::Duration>) -> io::Result<fs::File> {
    let deadline = limit.map(|d| std::time::Instant::now() + d);
    let dir = data_dir();
    ensure_private_data_dir(&dir)?;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::io::AsRawFd;
    if let Ok(metadata) = fs::symlink_metadata(lock_path()) {
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "файл блокировки SubBar имеет небезопасный тип или владельца",
            ));
        }
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path())?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "файл блокировки SubBar имеет небезопасный тип или владельца",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mode = if deadline.is_some() { libc::LOCK_EX | libc::LOCK_NB } else { libc::LOCK_EX };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), mode) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match (error.kind(), deadline) {
            (io::ErrorKind::Interrupted, _) => {}
            (io::ErrorKind::WouldBlock, Some(deadline)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            (io::ErrorKind::WouldBlock, Some(_)) => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "state.json занят другим процессом"));
            }
            _ => return Err(error),
        }
    }
    // Мусор после падения чистит только держатель замка: до замка можно было
    // удалить временный файл активного писателя прямо во время записи.
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("state.json.") && name.ends_with(".tmp") {
                let _ = fs::remove_file(entry.path());
            }
            // Файлы прокси пишутся без этого замка: живой tmp пишущего не трогаем, только хвосты старше суток.
            let proxy_tmp = ["proxy.json.", "proxy-sessions.json.", "proxy-paused.json."].iter().any(|p| name.starts_with(p));
            if proxy_tmp && name.ends_with(".tmp") {
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > 86_400);
                if old {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
    Ok(file)
}

fn parse_error_place(error: &serde_json::Error) -> String {
    use serde_json::error::Category;
    let kind = match error.classify() {
        Category::Io => "ошибка чтения",
        Category::Syntax => "синтаксис",
        Category::Data => "неверные данные",
        Category::Eof => "файл оборван",
    };
    if error.line() == 0 {
        return kind.to_string();
    }
    format!("{kind}, строка {}, столбец {}", error.line(), error.column())
}

pub fn load_state() -> State {
    try_load_state().unwrap_or_else(|error| {
        eprintln!("[subbar] не удалось безопасно прочитать state.json: {error}");
        std::process::exit(1);
    })
}

pub fn try_load_state() -> io::Result<State> {
    let _lock = acquire_state_lock()?;
    load_state_unlocked()
}

/// Чтение с потолком ожидания замка — для прокси, которому нельзя висеть на окне.
pub fn try_load_state_within(limit: std::time::Duration) -> io::Result<State> {
    let _lock = acquire_state_lock_within(Some(limit))?;
    // Битый файл прокси не уводит в сторону: SET_ASIDE в его процессе никто не покажет —
    // окно само отложит файл и скажет об этом. (Права и миграцию Devin он всё же записывает.)
    load_state_unlocked_with(false)
}

fn load_state_unlocked() -> io::Result<State> {
    load_state_unlocked_with(true)
}

fn load_state_unlocked_with(set_aside: bool) -> io::Result<State> {
    let path = state_path();
    let raw = {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "state.json должен быть обычным файлом текущего пользователя",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(error) => return Err(error),
        }
        let mut file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state.json должен быть обычным файлом текущего пользователя",
            ));
        }
        // OpenOptionsExt::mode действует только при создании. Старые файлы правим тоже.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        // Битый байт (обрезанная запись посреди кириллицы) — тот же «повреждённый файл»: копия и чистый старт,
        // а не отказ запускаться. Заменять байты нельзя: подпорченный ключ сохранился бы как настоящий.
        String::from_utf8(bytes).unwrap_or_else(|_| "не UTF-8".to_string())
    };
    match serde_json::from_str::<State>(&raw) {
        Ok(mut state) => {
            // Терпим правленные руками файлы: id уникальны, названия не пустые.
            // Повтор id (склеенные руками файлы) — новый id, а не удаление карточки вместе с ключом.
            let mut seen = std::collections::HashSet::new();
            for account in &mut state.accounts {
                while account.id.is_empty() || !seen.insert(account.id.clone()) {
                    account.id = new_id();
                }
            }
            let mut migrated_devin_secrets = false;
            for account in &mut state.accounts {
                if account.label.trim().is_empty() {
                    account.label = account.provider.display_name().to_string();
                }
                // Чистим название: без переводов строк и управляющих символов.
                account.label = account
                    .label
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect::<String>()
                    .trim()
                    .to_string();
                if account.label.is_empty() {
                    account.label = account.provider.display_name().to_string();
                }
                let secrets: Vec<String> = account
                    .credentials
                    .iter()
                    // Тот же фильтр, что при записи: иначе путь из ошибки превращался в «***» на чтении и так уходил на диск.
                    .filter(|(key, _)| !is_setting_key(key))
                    .map(|(_, value)| value)
                    .filter(|value| value.chars().count() >= 4) // короткое «секретом» не считаем: replace «1» испортил бы весь текст
                    .cloned()
                    .collect();
                let mut legacy_devin_secrets = Vec::new();
                if account.provider == crate::model::ProviderId::Devin {
                    // Старые сборки хранили их, хотя расход Devin берётся из
                    // локального кэша CLI. С диска убираем, а в памяти используем
                    // только чтобы вычистить случайные копии из текстов
                    // окна и кэша до сохранения миграции.
                    for key in ["apiKey", "apiBase"] {
                        if let Some(secret) = account.credentials.remove(key) {
                            if !secret.is_empty() {
                                legacy_devin_secrets.push(secret);
                            }
                            migrated_devin_secrets = true;
                        }
                    }
                    for secret in &legacy_devin_secrets {
                        scrub_legacy_secret(&mut account.id, secret);
                        scrub_legacy_secret(&mut account.label, secret);
                        if let Some(value) = &mut account.selected_model {
                            scrub_legacy_secret(value, secret);
                        }
                        if let Some(value) = &mut account.selected_window {
                            scrub_legacy_secret(value, secret);
                        }
                        let options = std::mem::take(&mut account.options);
                        for (mut key, mut value) in options {
                            scrub_legacy_secret(&mut key, secret);
                            scrub_legacy_secret(&mut value, secret);
                            account.options.insert(key, value);
                        }
                    }
                }
                if let Some(usage) = &mut account.last_usage {
                    if let Some(message) = usage.error.take() {
                        usage.error = Some(sanitize_saved_error(&message, secrets.iter()));
                    }
                    if let Some(plan) = usage.plan_type.take() {
                        usage.plan_type = Some(sanitize_saved_text(&plan, secrets.iter()));
                    }
                    for note in &mut usage.notes {
                        *note = sanitize_saved_text(note, secrets.iter());
                    }
                    for window in &mut usage.windows {
                        // Как при записи: ручная правка 1e999 не должна гулять по окну бесконечностью.
                        if !window.used_percent.is_finite() {
                            window.used_percent = 0.0;
                        }
                        window.key = sanitize_saved_text(&window.key, secrets.iter());
                        window.label = sanitize_saved_text(&window.label, secrets.iter());
                        if let Some(note) = window.note.take() {
                            window.note = Some(sanitize_saved_text(&note, secrets.iter()));
                        }
                    }
                }
            }
            // Зажимаем настройки в разумные пределы — ручная правка может всё сломать.
            state.settings.refresh_seconds = match state.settings.refresh_seconds {
                0 => 0,
                // Меньше минуты — шторм запросов раз в тик: 0 значит «выключено», минимум в окне — 60.
                s => s.clamp(60, 86400),
            };
            state.settings.notify_used_percent = if state.settings.notify_used_percent.is_finite() {
                state.settings.notify_used_percent.clamp(0.0, 100.0)
            } else {
                crate::model::Settings::default().notify_used_percent // 0 значит «уведомления выключены» — мусор в файле не должен их гасить
            };
            // Удаление ключей сохраняем, пока вызывающий ещё держит межпроцессный
            // замок. При сбое секреты остаются на диске до следующего сохранения.
            if migrated_devin_secrets {
                // Отказ записи не должен ронять запуск: уберём при следующем сохранении.
                if let Err(e) = save_state_unlocked(&state) {
                    eprintln!("[subbar] не записал чистку старых ключей Devin: {e}");
                }
            }
            Ok(state)
        }
        // Текст serde цитирует значение поля — а там бывает ключ. Наружу только место и вид ошибки.
        Err(error) if !set_aside => Err(io::Error::new(io::ErrorKind::InvalidData, format!("state.json повреждён: {}", parse_error_place(&error)))),
        Err(error) => {
            let error = parse_error_place(&error);
            // Сохраняем *каждую* повреждённую версию: общий state.corrupt
            // затёр бы прежнюю копию при следующей попытке чтения.
            let corrupt = data_dir().join(format!("state.corrupt.{}.json", new_id()));
            fs::rename(&path, &corrupt)?;
            eprintln!(
                "[subbar] повреждённый state.json ({error}); копия: {}",
                corrupt.display()
            );
            // Файл уже уведён — отметку ставим сразу, fsync не должен её отменить.
            *SET_ASIDE.lock().unwrap_or_else(|e| e.into_inner()) = Some(corrupt);
            if let Err(e) = fs::File::open(data_dir()).and_then(|d| d.sync_all()) {
                eprintln!("[subbar] fsync каталога данных не удался: {e}");
            }
            Ok(State::default())
        }
    }
}

/// Куда отложен битый state.json в этом запуске — окно скажет об этом, а не покажет молча пустой список.
pub static SET_ASIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn sanitize_saved_error<'a>(message: &str, secrets: impl Iterator<Item = &'a String>) -> String {
    if let Some(rest) = message.strip_prefix("HTTP ") {
        // Голое «HTTP 500» без двоеточия (старые записи Command Code) тоже приводим к общему виду.
        let code = rest.split_once(':').map_or(rest, |(code, _)| code).trim();
        if code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit()) && !code.starts_with('2') {
            return format!("HTTP {code}: сервер вернул ошибку");
        }
    }
    let safe = sanitize_saved_text(message, secrets);
    // Ссылки и «Bearer» sanitize_saved_text уже превратил в «Текст скрыт».
    if safe == "Текст скрыт" {
        return HIDDEN_ERROR.to_string();
    }
    safe
}

fn sanitize_saved_text<'a>(message: &str, secrets: impl Iterator<Item = &'a String>) -> String {
    let mut safe = message.to_string();
    // Длинные первыми: секрет, содержащий другой, иначе остался бы наполовину открытым.
    let mut secrets: Vec<&String> = secrets.filter(|secret| !secret.is_empty()).collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        safe = safe.replace(secret, "***");
    }
    if safe.contains("://") || safe.to_ascii_lowercase().contains("bearer ") {
        return "Текст скрыт".to_string();
    }
    safe.chars()
        .filter(|ch| !ch.is_control())
        .take(240)
        .collect()
}

fn scrub_legacy_secret(text: &mut String, secret: &str) {
    if !secret.is_empty() {
        *text = text.replace(secret, "***");
    }
}

/// Сохранить под общим замком. Мусорные значения сервисов чистим, чтобы
/// state оставался читаемым даже после кривого ответа.
fn save_state_unlocked(state: &State) -> io::Result<()> {
    let dir = data_dir();
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    }
    let path = state_path();
    // Уникально даже для потоков одного процесса; create_new отказывает при совпадении.
    let tmp = dir.join(format!("state.json.{}.tmp", new_id()));

    // Чистим дроби до сериализации — serde_json не принимает NaN/Inf.
    let mut clean = state.clone();
    if !clean.settings.notify_used_percent.is_finite() {
        clean.settings.notify_used_percent = crate::model::Settings::default().notify_used_percent;
    }
    for account in &mut clean.accounts {
        let secrets: Vec<String> = account
            .credentials
            .iter()
            // Пути в JSON и длина окна — настройки, не секреты: иначе «По пути «***» нет числа».
            .filter(|(key, _)| !is_setting_key(key))
            .map(|(_, value)| value)
            .filter(|value| value.chars().count() >= 4) // короткое «секретом» не считаем: replace «1» испортил бы весь текст
            .cloned()
            .collect();
        if let Some(usage) = &mut account.last_usage {
            if let Some(message) = usage.error.take() {
                usage.error = Some(sanitize_saved_error(&message, secrets.iter()));
            }
            if let Some(plan) = usage.plan_type.take() {
                usage.plan_type = Some(sanitize_saved_text(&plan, secrets.iter()));
            }
            for note in &mut usage.notes {
                *note = sanitize_saved_text(note, secrets.iter());
            }
            for window in &mut usage.windows {
                if !window.used_percent.is_finite() {
                    window.used_percent = 0.0;
                }
                window.key = sanitize_saved_text(&window.key, secrets.iter());
                window.label = sanitize_saved_text(&window.label, secrets.iter());
                if let Some(note) = window.note.take() {
                    window.note = Some(sanitize_saved_text(&note, secrets.iter()));
                }
            }
        }
    }

    let json = serde_json::to_string_pretty(&clean)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let result = (|| -> io::Result<()> {
        // Секреты — 0600 даже пока пишется временный файл.
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)?;
        // Метка «карточки здесь были»: по ней CLI отличает первый запуск от снесённого state.json.
        // Все карточки удалены намеренно — метка снимается: иначе пропажа пустого state.json
        // навсегда запирала бы CLI с требованием «восстанови из копии».
        if !state.accounts.is_empty() {
            mark_had_accounts();
        } else {
            let _ = fs::remove_file(dir.join(HAD_ACCOUNTS));
        }
        // Файл уже на месте: сбой fsync каталога — не «не сохранил», иначе окно долбило бы запись по кругу.
        if let Err(e) = fs::File::open(&dir).and_then(|d| d.sync_all()) {
            eprintln!("[subbar] state.json записан, но fsync каталога не удался: {e}");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Загрузить, изменить, сохранить — всё под одним замком. Колбэк сообщает,
/// изменил ли он состояние; неудачные и только читающие операции файл не переписывают
/// (кроме разовой чистки старых секретов Devin внутри загрузчика).
pub fn with_state<F, R>(f: F) -> io::Result<R>
where
    F: FnOnce(&mut State) -> io::Result<(R, bool)>,
{
    with_state_opts(false, f)
}

/// `rewrite`: разрешить записать новый файл при отложенной битой копии — окно, у которого
/// карточки целы в памяти, восстанавливает state.json из них.
pub fn with_state_opts<F, R>(rewrite: bool, f: F) -> io::Result<R>
where
    F: FnOnce(&mut State) -> io::Result<(R, bool)>,
{
    let _lock = acquire_state_lock()?;
    // Не смогли узнать — считаем, что файл есть: иначе пустое состояние затрёт живой файл.
    let existed = state_path().try_exists().unwrap_or(true);
    let mut state = load_state_unlocked()?;
    // Битый файл только что ушёл в state.corrupt.*: запись поверх пустого состояния стёрла бы
    // все карточки из живого файла, а команда отрапортовала бы успех.
    // И позже: файла нет, а рядом лежит отложенная битая копия — «пустое» состояние не новое,
    // а потерянное; `subbar add` по совету «список пуст» затёр бы надежду его восстановить.
    if !rewrite && ((existed && !state_path().exists()) || (!existed && has_corrupt_backup())) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "state.json повреждён — восстанови его из state.corrupt.*.json (или удали state.corrupt.* и .had-accounts, если карточки не нужны) в {}",
                data_dir().display()
            ),
        ));
    }
    // Файл снесли руками или чистилкой: без этой проверки следующий `subbar add` записал бы
    // state.json из одной карточки, и все прежние пропали бы без следа.
    if !rewrite && !existed && data_dir().join(HAD_ACCOUNTS).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "state.json пропал, а карточки были — восстанови его из копии (если удалил намеренно, удали и {HAD_ACCOUNTS}) в {}",
                data_dir().display()
            ),
        ));
    }
    let (result, changed) = f(&mut state)?;
    if changed {
        save_state_unlocked(&state)?;
    }
    Ok(result)
}

/// Текст вместо ошибки, в которой могла остаться ссылка с токеном — один на окно и CLI.
pub const HIDDEN_ERROR: &str = "Ошибка обновления (детали скрыты)";

/// Метка в каталоге данных: state.json хоть раз сохранялся с карточками.
const HAD_ACCOUNTS: &str = ".had-accounts";

/// Поставить метку «карточки здесь были». Сбой — не провал сохранения, но защита от снесённого state.json слепа.
pub(crate) fn mark_had_accounts() {
    use std::os::unix::fs::OpenOptionsExt;
    if let Err(e) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(data_dir().join(HAD_ACCOUNTS))
    {
        eprintln!("[subbar] не записал метку {HAD_ACCOUNTS}: {e}");
    }
}

/// Метка «карточки были»: пропавший state.json при ней — потеря, а не первый запуск.
pub fn had_accounts_marked() -> bool {
    data_dir().join(HAD_ACCOUNTS).exists()
}

pub fn has_corrupt_backup() -> bool {
    fs::read_dir(data_dir())
        .map(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().starts_with("state.corrupt.")))
        .unwrap_or(false)
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub fn new_id() -> String {
    // /dev/urandom недоступен (песочница, исчерпаны дескрипторы) — не паника на старте окна,
    // а id из времени, pid и счётчика: в одном процессе он всё равно уникален.
    try_new_id().unwrap_or_else(|_| {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{:016x}-{:08x}{:08x}", now_ms().max(0), std::process::id(), n)
    })
}

fn try_new_id() -> io::Result<String> {
    // Энтропия ОС не даёт одинаковых id, когда часы стоят или pid переиспользуются.
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes))?;
    Ok(format!(
        "{:016x}-{:016x}",
        u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        u64::from_le_bytes(bytes[8..].try_into().unwrap())
    ))
}

#[cfg(test)]
mod tests {
    use super::new_id;
    use std::collections::HashSet;

    #[test]
    fn legacy_errors_never_echo_http_body_or_credentials() {
        assert_eq!(
            super::sanitize_saved_error("HTTP 500: synthetic-secret", std::iter::empty()),
            "HTTP 500: сервер вернул ошибку"
        );
        let secret = "synthetic-secret".to_string();
        assert_eq!(
            super::sanitize_saved_error("Не удалось: synthetic-secret", std::iter::once(&secret)),
            "Не удалось: ***"
        );
        assert_eq!(
            super::sanitize_saved_error(
                "request to https://example.test/?token=synthetic-secret failed",
                std::iter::empty()
            ),
            super::HIDDEN_ERROR
        );
    }

    #[test]
    fn ids_remain_unique_during_a_burst() {
        let ids: HashSet<_> = (0..10_000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 10_000);
    }
}

/// Настройки карточки, а не секреты: пути в JSON, длина окна, каталог Codex. Иначе диагностика
/// «По пути «***» нет числа» / «искал в ***/auth.json» теряет ровно то, что нужно для починки.
pub fn is_setting_key(key: &str) -> bool {
    // Совпадает с несекретными полями model::credential_fields: id аккаунта, адрес и имя заголовка — не секреты.
    matches!(key, "usedPath" | "remainingPath" | "resetPath" | "windowMinutes" | "codexHome" | "accountId" | "url" | "headerName")
}
