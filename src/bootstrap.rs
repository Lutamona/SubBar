//! Первый запуск SubBar: забрать аккаунты из LimitBar и завести карточку Claude,
//! чтобы ключи OpenCode Go и лимиты Claude были на месте без ручной настройки.

use crate::{model, providers, store};
use std::os::unix::fs::PermissionsExt;

/// Свой каталог данных (`LIMITBAR_DATA_DIR`: тесты, снимки) — изолированный: ни чужих аккаунтов,
/// ни ключей из Keychain туда не кладём.
fn isolated() -> bool {
    // Как store::data_dir: одни пробелы — «не задано», иначе данные в боевом каталоге, а перенос пропущен.
    std::env::var("LIMITBAR_DATA_DIR").is_ok_and(|d| !d.trim().is_empty())
}

/// Своего хранилища ещё нет, а у LimitBar есть — копируем файл (LimitBar не трогаем).
/// Синхронно и до загрузки состояния окном: это одна копия файла.
pub fn migrate_from_limitbar() {
    if isolated() {
        return;
    }
    let ours = store::state_path();
    // Перенос — один раз в жизни: иначе пропавший state.json (удалили, чистилка) откатывал всё
    // к старому снимку LimitBar, воскрешая удалённые карточки.
    let marker = ours.with_file_name(".migrated-from-limitbar");
    let mark = || {
        if let Err(e) = std::fs::write(&marker, b"") {
            eprintln!("[subbar] не записал метку переноса из LimitBar: {e}");
        }
    };
    if marker.exists() {
        return;
    }
    if ours.exists() {
        // Своё хранилище уже есть — дальше переносить нечего никогда.
        // Замок держим, пока ставим метку, и не дольше 10 с: CLI не должен висеть на окне молча.
        if let Ok(_lock) = store::acquire_state_lock_within(Some(std::time::Duration::from_secs(10))) {
            mark();
        }
        return;
    }
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else { return };
    let theirs = std::path::PathBuf::from(home).join("Library/Application Support/LimitBar/state.json");
    if !theirs.exists() {
        return;
    }
    // Каталог создаёт замок (ensure_private_data_dir: 0700 с первого inode, отказ по symlink);
    // свой chmod тут шёл по симлинку в чужую папку до этой проверки.
    // Под замком состояния: окно и CLI стартуют вместе и иначе копировали в один tmp наперегонки.
    let _lock = match store::acquire_state_lock_within(Some(std::time::Duration::from_secs(10))) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[subbar] не перенёс аккаунты из LimitBar: каталог данных недоступен ({e})");
            return;
        }
    };
    // Остаток оборванной прошлой попытки: полная копия ключей, больше никто её не уберёт.
    let _ = std::fs::remove_file(ours.with_file_name(".state-migrate.tmp"));
    if ours.exists() {
        mark();
        return;
    }
    // Отложенный битый файл значит, что своё хранилище уже было: старый снимок LimitBar
    // поверх него потерял бы всё, что добавлено в SubBar.
    if store::has_corrupt_backup() {
        mark();
        return;
    }
    // Через временный файл: оборванная копия не станет «уже перенесённым» обрезком навсегда.
    // Имя не под уборку store (state.json.*.tmp): её делает держатель замка, а мы копируем без него.
    let tmp = ours.with_file_name(".state-migrate.tmp");
    let copy = || -> std::io::Result<bool> {
        // 0600 с первого байта: fs::copy создал бы файл с ключами по umask.
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            // Как свой state.json: без ссылок и FIFO — иначе copy повис бы под замком состояния.
            let mut src = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&theirs)?;
            if !src.metadata()?.is_file() {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "state.json LimitBar — не обычный файл"));
            }
            std::io::copy(&mut src, &mut out)?;
        }
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        // Без sync обрыв питания оставлял бы state.json нулевой длины — и миграция не повторится.
        std::fs::File::open(&tmp)?.sync_all()?;
        // Обрезанный файл LimitBar (выключили посреди записи) не переносим: он ушёл бы в state.corrupt.*,
        // а метка переноса не дала бы повторить.
        let parsed = serde_json::from_slice::<model::State>(&std::fs::read(&tmp)?)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::rename(&tmp, &ours)?;
        // Файл уже на месте: сбой fsync каталога — не провал переноса (как в store).
        if let Some(Err(e)) = ours.parent().map(|dir| std::fs::File::open(dir).and_then(|d| d.sync_all())) {
            eprintln!("[subbar] state.json перенесён, но fsync каталога не удался: {e}");
        }
        Ok(!parsed.accounts.is_empty())
    };
    match copy() {
        Ok(has_accounts) => {
            mark();
            // Перенесённые карточки — тоже «карточки были»: иначе снесённый state.json до первого
            // сохранения окном выглядел бы первым запуском, а не потерей.
            if has_accounts {
                store::mark_had_accounts();
            }
            eprintln!("[subbar] {}", if has_accounts { "аккаунты перенесены из LimitBar" } else { "снимок LimitBar перенесён — карточек в нём нет" })
        }
        Err(e) => {
            // Копия с ключами не должна остаться лежать (уборка store её не видит).
            let _ = std::fs::remove_file(&tmp);
            eprintln!("[subbar] не перенёс аккаунты из LimitBar: {e}")
        }
    }
}

/// Нет карточки Claude — найти вход Claude Code (Keychain или файл) и добавить. В фоне: сеть и Keychain.
pub fn ensure_claude_account() {
    if isolated() {
        return;
    }
    std::thread::spawn(|| {
        // Не load_state: при ошибке он завершает процесс, а это фоновый поток — окно просто исчезло бы.
        // Чтение с потолком: замок может держать окно или CLI — не висеть молча до конца жизни окна.
        let Ok(known) = store::try_load_state_within(std::time::Duration::from_secs(10)) else { return };
        // Битый state.json отложен, а пустой список — не «карточки нет»: сохранить новую store откажется,
        // а одноразовый refresh опрос уже сжёг бы.
        if known.accounts.is_empty() && store::has_corrupt_backup() {
            return;
        }
        if known.accounts.iter().any(|a| a.provider == model::ProviderId::Claude) {
            return;
        }
        let Some(item) = providers::claude::detect().into_iter().next() else { return };
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
        let Ok((usage, patch)) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| providers::fetch_account(&account))) else {
            eprintln!("[subbar] опрос новой карточки Claude упал — попробую при следующем запуске");
            return;
        };
        // Опрос не удался (сеть, протухший вход) — не заводить карточку-ошибку: следующий запуск попробует снова.
        // Новые токены (patch) терять нельзя — с ними карточку заводим в любом случае.
        if usage.status != model::FetchStatus::Ok && patch.is_empty() {
            return;
        }
        let rotated = !patch.is_empty();
        for (key, value) in patch {
            if !value.is_empty() && model::credential_fields(account.provider).iter().any(|f| f.key == key) {
                account.credentials.insert(key, value);
            }
        }
        account.last_usage = Some(usage);
        // Без срока намеренно: поток фоновый, а в руках свежая пара после сгоревшего refresh —
        // сдаться по таймауту значило бы потерять вход. Дождаться замка лучше.
        let saved = store::with_state(move |disk| {
            let fresh = !disk.accounts.iter().any(|a| a.provider == model::ProviderId::Claude);
            if fresh {
                disk.accounts.push(account);
                return Ok(((), true));
            }
            // Карточку завели, пока мы опрашивали (окно, CLI): старый refresh в ней уже сгорел —
            // переложить свежую пару туда, иначе она следующим опросом упрётся в «войди заново».
            let mut changed = false;
            // Только если там всё ещё наша, сгоревшая пара: более новую (окно успело своё) не откатывать.
            let old_refresh = item.credentials.iter().find(|(k, _)| k == "refreshToken").map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty());
            // Все карточки с этой сгоревшей парой (дубль от гонки с импортом), а не первая попавшаяся.
            for existing in disk.accounts.iter_mut().filter(|a| {
                rotated && a.provider == model::ProviderId::Claude && old_refresh.is_some() && a.credentials.get("refreshToken").map(|v| v.trim().to_string()) == old_refresh
            }) {
                for (key, value) in &account.credentials {
                    if existing.credentials.get(key) != Some(value) && !value.is_empty() {
                        existing.credentials.insert(key.clone(), value.clone());
                        changed = true;
                    }
                }
            }
            Ok(((), changed))
        });
        if let Err(error) = saved {
            eprintln!("[subbar] не смог сохранить карточку Claude: {error}");
        }
    });
}
