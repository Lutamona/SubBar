//! Первый запуск SubBar: завести карточку Claude,
//! чтобы лимиты Claude были на месте без ручной настройки.

use crate::{model, providers, store};

/// Свой каталог данных (`SUBBAR_DATA_DIR`: тесты, снимки) — изолированный: ни чужих аккаунтов,
/// ни ключей из Keychain туда не кладём.
pub(crate) fn isolated() -> bool {
    // Как store::data_dir: одни пробелы — «не задано», иначе данные в боевом каталоге, а перенос пропущен.
    std::env::var("SUBBAR_DATA_DIR").is_ok_and(|d| !d.trim().is_empty())
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
