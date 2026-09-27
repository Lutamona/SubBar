use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{LazyLock, Mutex};

use crate::model::{Account, AccountUsage, DetectedCredential, ProviderId, Settings, State};
use crate::providers;
use crate::store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    List,
    Form,
    Settings,
    Proxy,
}

/// Пауза после неудачной записи state.json: тик идёт каждые 0,75 с, а файл может быть
/// неписуемым часами. Данные ждут в памяти, повтор — не чаще этого срока.
const SAVE_RETRY_MS: i64 = 30_000;

#[derive(Debug, Clone)]
pub struct FormState {
    pub editing_id: Option<String>,
    pub provider: ProviderId,
    pub label: String,
    /// Значения выбранного сервиса; секреты показаны точками.
    pub values: BTreeMap<String, String>,
    /// Черновики только в памяти. Смена сервиса не должна терять введённые ключи
    /// или случайно переносить API-ключ в другой сервис.
    pub drafts: HashMap<ProviderId, BTreeMap<String, String>>,
}

impl Default for FormState {
    fn default() -> Self {
        FormState {
            editing_id: None,
            provider: ProviderId::Codex,
            label: String::new(),
            values: BTreeMap::new(),
            drafts: HashMap::new(),
        }
    }
}

impl FormState {
    pub fn capture(&mut self, label: String, values: BTreeMap<String, String>) {
        self.label = label;
        self.drafts.insert(self.provider, values.clone());
        self.values = values;
    }

    pub fn switch_provider(
        &mut self,
        provider: ProviderId,
        label: String,
        current_values: BTreeMap<String, String>,
    ) {
        self.drafts.insert(self.provider, current_values);
        self.provider = provider;
        self.label = label;
        self.values = self.drafts.get(&provider).cloned().unwrap_or_default();
    }
}

#[derive(Debug, Clone)]
pub enum WorkerEvent {
    RefreshStarted,
    RefreshDone {
        id: String,
        usage: AccountUsage,
        patch: Vec<(String, String)>,
        /// Поколение карточки на старте опроса — устаревшие итоги выбрасываются.
        generation: u64,
        /// Доступ, с которым опрос начинался: устаревший результат всё равно несёт обменянный
        /// одноразовый refresh — его сохраняем, если карточка всё ещё на тех же ключах.
        basis: std::collections::BTreeMap<String, String>,
    },
    DetectDone(Vec<DetectedCredential>),
    Notice(String),
}

pub struct AppState {
    pub data: State,
    /// Последний снимок с диска; локальные правки сливаются с ним под замком файла.
    persisted: State,
    pub screen: Screen,
    pub refreshing: bool,
    pub inflight: usize,
    /// Обновление (⌘R или автоволна) не запускает дублей для той же версии карточки.
    active_fetches: HashSet<(String, u64)>,
    pub form: FormState,
    pub detected: Vec<DetectedCredential>,
    pub status_line: Option<String>,
    status_until_ms: Option<i64>,
    /// Карточка, для которой открыто контекстное меню.
    pub menu_account: Option<String>,
    /// Только что удалённая карточка — её можно вернуть («Вернуть» в шапке, ⌘Z), пока не истёк срок:
    /// (карточка, её место в списке, до какого момента).
    /// Удалённые карточки, которые ещё можно вернуть (последняя — сверху): второе удаление
    /// подряд не должно молча стирать возможность вернуть первое.
    pub removed: Vec<(Account, usize, i64)>,
    /// Раскрытые подробности на время текущего показа окна (не сохраняются).
    pub expanded_accounts: HashSet<String>,
    /// Удалённые, что были раскрыты: ⌘Z возвращает карточку такой же, какой её убрали.
    pub removed_expanded: HashSet<String>,
    /// Монотонный счётчик для версий идентичности сервиса по карточкам.
    pub generation: u64,
    /// Версии идентичности опроса по карточкам. Посторонние правки не должны
    /// выбрасывать годные итоги других карточек, а правки ключей — должны.
    account_generations: HashMap<String, u64>,
    /// Флаг «грязно» — сохранение пачкой, а не на каждом тике.
    pub dirty: bool,
    /// Файл испортился на ходу — следующая запись создаёт его заново из памяти окна.
    rewrite_after_corruption: bool,
    /// До какого момента не пробовать запись снова после отказа (см. `SAVE_RETRY_MS`).
    save_retry_after_ms: Option<i64>,
}

impl AppState {
    fn new() -> Self {
        let mut app = Self::from_state(store::load_state());
        if let Some(path) = store::SET_ASIDE.lock().unwrap_or_else(|e| e.into_inner()).take() {
            app.set_status_for(format!("state.json был повреждён — битая копия в {}, восстанови его", path.display()), 120_000);
        } else if !store::state_path().exists() && store::has_corrupt_backup() {
            // Отложили в прошлый запуск: без подсказки пустое окно выглядит как потеря всего.
            app.set_status_for("state.json нет, но рядом лежит битая копия state.corrupt.* — восстанови её, если нужны карточки".to_string(), 120_000);
        } else if app.persisted.accounts.is_empty() && !store::state_path().exists() && store::had_accounts_marked() {
            // Файл удалили, а карточки были: пустое окно без подсказки выглядит первым запуском.
            app.set_status_for("state.json пропал, а карточки были — восстанови его из копии".to_string(), 120_000);
        }
        app
    }

    fn from_state(data: State) -> Self {
        let account_generations = data
            .accounts
            .iter()
            .map(|account| (account.id.clone(), 0))
            .collect();
        AppState {
            persisted: data.clone(),
            data,
            screen: Screen::List,
            refreshing: false,
            inflight: 0,
            active_fetches: HashSet::new(),
            form: FormState::default(),
            detected: Vec::new(),
            status_line: None,
            status_until_ms: None,
            menu_account: None,
            removed: Vec::new(),
            expanded_accounts: HashSet::new(),
            removed_expanded: HashSet::new(),
            generation: 0,
            account_generations,
            dirty: false,
            rewrite_after_corruption: false,
            save_retry_after_ms: None,
        }
    }

    fn bump_account_generation(&mut self, id: &str) {
        self.generation = self.generation.wrapping_add(1);
        self.account_generations
            .insert(id.to_string(), self.generation);
    }

    fn reconcile_account_generations(&mut self, before: &State, after: &State) {
        let before_by_id: HashMap<&str, &Account> = before
            .accounts
            .iter()
            .map(|account| (account.id.as_str(), account))
            .collect();
        let after_by_id: HashMap<&str, &Account> = after
            .accounts
            .iter()
            .map(|account| (account.id.as_str(), account))
            .collect();
        let ids: HashSet<&str> = before_by_id
            .keys()
            .chain(after_by_id.keys())
            .copied()
            .collect();
        for id in ids {
            let changed = match (before_by_id.get(id), after_by_id.get(id)) {
                (Some(before), Some(after)) => !same_refresh_identity(before, after),
                (None, None) => false,
                _ => true,
            };
            if changed {
                self.bump_account_generation(id);
            }
        }
    }

    /// Отсортированные карточки для показа — без ключей, чтобы не утекли секреты.
    /// Порядок списка без копий карточек (и их секретов) — для горячих путей вроде движения мыши.
    pub fn accounts_sorted_refs(&self) -> Vec<&Account> {
        let mut accounts: Vec<&Account> = self.data.accounts.iter().collect();
        accounts.sort_by(|a, b| list_order(a, b));
        accounts
    }

    pub fn accounts_sorted(&self) -> Vec<Account> {
        let mut accounts = self.data.accounts.clone();
        for account in &mut accounts {
            account.credentials.clear();
        }
        accounts.sort_by(list_order);
        accounts
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.set_status_for(message, 6_000);
    }

    /// Сообщение в шапке на заданное время.
    pub fn set_status_for(&mut self, message: impl Into<String>, ms: i64) {
        self.status_line = Some(message.into());
        let now = store::now_ms();
        let deadline = now.saturating_add(ms);
        // Чужое сообщение не укорачивает окно «Вернуть»: кнопка живёт до своего срока,
        // а не до конца этого сообщения.
        self.status_until_ms = Some(match self.removed.last() {
            Some((_, _, until)) => deadline.max(*until),
            None => deadline,
        });
    }

    pub fn expire_status(&mut self) -> bool {
        let now = store::now_ms();
        let mut changed = false;
        if self.status_line.is_some() && status_is_expired(self.status_until_ms, now) {
            self.status_line = None;
            self.status_until_ms = None;
            changed = true;
        }
        // «Вернуть» живёт своим сроком: чужие статусы его не продлевают и не укорачивают.
        let before = self.removed.len();
        self.removed.retain(|(_, _, until)| now < *until);
        changed |= self.removed.len() != before;
        let removed = &self.removed;
        self.removed_expanded.retain(|id| removed.iter().any(|(a, _, _)| &a.id == id));
        changed
    }

    /// Пометить состояние грязным — запись будет в `flush_save` (пачкой).
    pub fn save(&mut self) {
        self.dirty = true;
    }

    /// Выход из приложения: последняя попытка записи — мимо паузы после отказа.
    pub fn flush_save_now(&mut self) {
        self.save_retry_after_ms = None;
        self.flush_save();
    }

    /// Записываем только свои изменения, а не старую копию всего файла.
    pub fn flush_save(&mut self) {
        if !self.dirty {
            return;
        }
        // После отказа не бьёмся в тот же несгибаемый файл каждый тик (0,75 с):
        // ждём паузу, изменения остаются в памяти и в `dirty` — ничего не теряется.
        if self.save_retry_after_ms.is_some_and(|retry_at| store::now_ms() < retry_at) {
            return;
        }
        let base = self.persisted.clone();
        let local = self.data.clone();
        let before = self.data.clone();
        // Первый запуск (файла нет, сохранённого пусто, метки «карточки были» нет) — писать смело.
        // С меткой файл пропал: одна новая карточка затёрла бы все прежние, пусть store откажет с подсказкой.
        let rewrite = std::mem::take(&mut self.rewrite_after_corruption)
            || (self.persisted.accounts.is_empty() && !store::state_path().exists() && !store::had_accounts_marked());
        match store::with_state_opts(rewrite, move |disk| {
            // Битый файл отложен в копию. Ранее заполненное состояние не заменяем
            // пустым файлом только потому, что он пропал.
            if !rewrite && !store::state_path().exists() && !base.accounts.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "state.json исчез; восстанови его из резервной копии",
                ));
            }
            let merged = merge_state(&base, &local, disk);
            let changed = merged != *disk;
            *disk = merged.clone();
            Ok((merged, changed))
        }) {
            Ok(merged) => {
                self.reconcile_account_generations(&before, &merged);
                self.data = merged.clone();
                self.persisted = merged;
                self.dirty = false;
                self.save_retry_after_ms = None;
            }
            Err(error) => {
                // Файл испортился на ходу и ушёл в сторону прямо в этой записи: карточки целы в памяти —
                // пишем их заново сразу, а не ждём sync_external, до которого при `dirty` не дойдёт.
                if !rewrite {
                    if let Some(path) = store::SET_ASIDE.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        self.restore_from_window(&path);
                        return;
                    }
                }
                eprintln!("[subbar] не сохранил state: {error}");
                // Подробность ошибки (что восстановить, что удалить) — в строку статуса, а не только в stderr.
                let why: String = error.to_string().chars().take(240).collect();
                // Держим до следующей попытки: через 6 с шапка выглядела бы нормально, а данные жили только в памяти.
                self.set_status_for(format!("Ошибка сохранения — {why}"), SAVE_RETRY_MS + 5_000);
                self.save_retry_after_ms = Some(store::now_ms().saturating_add(SAVE_RETRY_MS));
            }
        }
    }

    /// Битый state.json уведён в `path`: записать файл заново из карточек окна
    /// (база «пусто», так слияние ничего не выкинет) и сказать, где лежит битая копия.
    fn restore_from_window(&mut self, path: &std::path::Path) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.persisted = State::default();
        self.rewrite_after_corruption = true;
        self.save_retry_after_ms = None;
        self.save();
        self.flush_save_now();
        if self.dirty {
            return; // статус об ошибке записи уже поставлен
        }
        self.set_status_for(format!("state.json был повреждён — записал заново из окна, битая копия в {name}"), 120_000);
    }

    /// Подхватываем правки CLI, даже если окну нечего сохранять.
    /// Несохранённые правки не выбрасываем, когда файл не читается.
    pub fn sync_external(&mut self) -> bool {
        let before = self.data.clone();
        let before_status = self.status_line.clone();
        if self.dirty {
            self.flush_save();
            if self.dirty {
                // Сохранение не удалось — статус переписан, даже если текст совпал: заголовок перерисовать.
                return true;
            }
        }
        match store::try_load_state() {
            Ok(disk) if disk != self.persisted && store::state_path().exists() => {
                self.reconcile_account_generations(&self.data.clone(), &disk);
                self.data = disk.clone();
                self.persisted = disk;
            }
            Ok(_) => {
                // Файл испортился на ходу и уведён в сторону: в памяти карточки целы — пишем их заново
                // (база «пусто», так слияние ничего не выкинет) и говорим, где лежит битая копия.
                if let Some(path) = store::SET_ASIDE.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    self.restore_from_window(&path);
                }
            }
            Err(error) => {
                eprintln!("[subbar] не прочитал изменения state: {error}");
                self.set_status("Ошибка чтения данных — проверь state.json");
            }
        }
        self.expanded_accounts
            .retain(|id| self.data.accounts.iter().any(|a| &a.id == id));
        self.data != before || self.status_line != before_status
    }

    pub fn upsert_account(&mut self, account: Account) {
        let added = !self.data.accounts.iter().any(|a| a.id == account.id);
        let changed_identity = match self.data.accounts.iter_mut().find(|a| a.id == account.id) {
            Some(existing) => {
                let changed = !same_refresh_identity(existing, &account);
                *existing = account.clone();
                changed
            }
            None => {
                self.data.accounts.push(account.clone());
                true
            }
        };
        if changed_identity {
            self.bump_account_generation(&account.id);
        }
        // Новая карточка — сразу на диск: CLI и прокси должны увидеть её не через секунду-другую,
        // а закрытое тут же окно не должно её потерять.
        self.save();
        if added {
            self.flush_save_now();
        }
    }
}

fn status_is_expired(until: Option<i64>, now: i64) -> bool {
    until.is_some_and(|deadline| now >= deadline)
}

/// Опрашивают ли две записи расход одной и той же идентичности.
/// Закрепление и его порядок — только показ; любая другая опция может влиять на опрос.
pub fn same_refresh_identity(left: &Account, right: &Account) -> bool {
    let data = |key: &String| !crate::model::PRESENTATION_OPTIONS.contains(&key.as_str());
    left.provider == right.provider
        && left.credentials == right.credentials
        && left.options.iter().filter(|(key, _)| data(key)).eq(right.options.iter().filter(|(key, _)| data(key)))
}

/// Сопоставить найденный локальный ключ с сохранённой карточкой, не опираясь
/// на названия: их можно править, и карточек одного сервиса бывает несколько.
/// Секреты сравниваются только в памяти и никогда не попадают в вывод.
pub fn matches_detected_account(account: &Account, found: &DetectedCredential) -> bool {
    if account.provider != found.provider {
        return false;
    }
    if found.provider == ProviderId::Devin {
        // Расход Devin берётся из одного общего локального кэша CLI, а не из
        // сохранённого API-ключа. Импортировать можно только один найденный профиль.
        return true;
    }
    if found.provider == ProviderId::Claude
        && account.options.get("claudeCodeSource").map(String::as_str) == Some("true")
        && found
            .options
            .iter()
            .any(|(key, value)| key == "claudeCodeSource" && value == "true")
    {
        // Привязанная запись нарочно следует текущему токену из связки ключей, даже
        // если в её сохранённых полях лежит старый, уже обновлённый токен.
        return true;
    }
    let found_credentials: BTreeMap<&str, &str> = found
        .credentials
        .iter()
        .map(|(key, value)| (key.as_str(), value.trim()))
        .filter(|(_, value)| !value.is_empty())
        .collect();
    let identity_keys: &[&str] = match found.provider {
        ProviderId::Codex => &["accountId", "accessToken"],
        ProviderId::Claude => &["refreshToken", "accessToken"],
        ProviderId::OpenCodeGo | ProviderId::CommandCode | ProviderId::Devin => &["apiKey"],
        ProviderId::Custom => &["url", "headerValue"],
    };
    let existing: Vec<(&str, &str)> = identity_keys
        .iter()
        .filter_map(|key| {
            account
                .credentials
                .get(*key)
                .map(|value| (*key, value.trim()))
                .filter(|(_, value)| !value.is_empty())
        })
        .collect();
    let discovered: Vec<(&str, &str)> = identity_keys
        .iter()
        .filter_map(|key| found_credentials.get(key).map(|value| (*key, *value)))
        .collect();
    if existing
        .iter()
        .any(|(key, value)| discovered.contains(&(*key, *value)))
    {
        return true;
    }
    // Явно разные токены — разные аккаунты, даже если источник
    // случайно дал им одинаковое название.
    if !existing.is_empty() {
        return false;
    }

    // Некоторые сервисы берут ключи из своего CLI или связки ключей, когда
    // у карточки нет явного токена. Такая карточка — найденная
    // идентичность по умолчанию для этого источника.
    match found.provider {
        ProviderId::Codex => {
            let configured_home = account
                .credentials
                .get("codexHome")
                .or_else(|| account.options.get("codexHome"))
                .map(|value| value.trim())
                // Пустой codexHome codex::codex_home считает «не задан» — и тут так же, иначе дубль.
                .filter(|value| !value.is_empty());
            let discovered_home = found_credentials.get("codexHome").copied();
            match (configured_home, discovered_home) {
                (Some(left), Some(right)) => {
                    // Та же развёртка, что у codex::codex_home: и «~/…», и голый «~».
                    let expanded = crate::providers::codex::expand_tilde(left);
                    expanded == right
                }
                (None, Some(right)) => {
                    let home = std::env::var("CODEX_HOME")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                        .or_else(|| {
                            std::env::var("HOME")
                                .ok()
                                .filter(|value| !value.trim().is_empty())
                                .map(|home| format!("{home}/.codex"))
                        });
                    home.as_deref() == Some(right)
                }
                _ => false,
            }
        }
        // Совпадение по claudeCodeSource уже поймано ранним return выше.
        ProviderId::Claude => false,
        ProviderId::OpenCodeGo | ProviderId::CommandCode | ProviderId::Devin => {
            !discovered.is_empty()
        }
        ProviderId::Custom => false,
    }
}

/// Слить правки окна с `base` в свежий снимок с диска. Параллельная смена
/// идентичности на диске побеждает устаревшие локальные ключи и расход, а
/// карточка, удалённая через CLI, обновлением не воскресает.
fn merge_state(base: &State, local: &State, disk: &State) -> State {
    let mut merged = disk.clone();
    let old_ids: HashSet<&str> = base.accounts.iter().map(|a| a.id.as_str()).collect();
    let local_ids: HashSet<&str> = local.accounts.iter().map(|a| a.id.as_str()).collect();
    merged
        .accounts
        .retain(|a| !old_ids.contains(a.id.as_str()) || local_ids.contains(a.id.as_str()));

    for current in &local.accounts {
        match base.accounts.iter().find(|a| a.id == current.id) {
            None => {
                if !merged.accounts.iter().any(|a| a.id == current.id) {
                    merged.accounts.push(current.clone());
                }
            }
            Some(previous) => {
                // На диске этого id может не быть, потому что CLI его явно удалил.
                if let Some(target) = merged.accounts.iter_mut().find(|a| a.id == current.id) {
                    let local_identity_changed = !same_refresh_identity(current, previous);
                    let disk_identity_changed = !same_refresh_identity(target, previous);
                    if !disk_identity_changed {
                        // Ключи — одна идентичность: по отдельным полям их не сливаем,
                        // иначе можно случайно спарить разные токены.
                        if local_identity_changed {
                            target.provider = current.provider;
                            target.credentials = current.credentials.clone();
                        }
                        // В опциях есть независимое состояние показа (например закрепление),
                        // поэтому после проверки идентичности сливаем их по ключам.
                        merge_map(&previous.options, &current.options, &mut target.options);
                        if local_identity_changed {
                            // Кэш, не менявшийся с `base`, принадлежит старым
                            // ключам. Изменённый кэш получен уже после
                            // локальной смены идентичности — его можно оставить.
                            target.last_usage = if current.last_usage == previous.last_usage {
                                None
                            } else {
                                current.last_usage.clone()
                            };
                            target.selected_model = current.selected_model.clone();
                            target.selected_window = current.selected_window.clone();
                        } else {
                            let local_usage_changed = current.last_usage != previous.last_usage;
                            let disk_usage_changed = target.last_usage != previous.last_usage;
                            let local_usage_wins = if !disk_usage_changed {
                                true
                            } else {
                                match (&current.last_usage, &target.last_usage) {
                                    // Отказ опроса несёт свежее время при протянутых старых окнах —
                                    // удачный опрос соседа (CLI) он перебивать не должен.
                                    (Some(local), Some(disk)) if local.status != crate::model::FetchStatus::Ok && disk.status == crate::model::FetchStatus::Ok => false,
                                    // Зеркально: свой удачный опрос не уступает более позднему отказу соседа с протухшими окнами.
                                    (Some(local), Some(disk)) if local.status == crate::model::FetchStatus::Ok && disk.status != crate::model::FetchStatus::Ok => true,
                                    (Some(local), Some(disk)) => local.updated_at > disk.updated_at,
                                    // Очистка на диске — явное изменение;
                                    // кэш расхода поверх неё не воскрешаем.
                                    _ => false,
                                }
                            };
                            if local_usage_changed && local_usage_wins {
                                target.last_usage = current.last_usage.clone();
                            }
                            if current.selected_model != previous.selected_model {
                                target.selected_model = current.selected_model.clone();
                            }
                            if current.selected_window != previous.selected_window {
                                target.selected_window = current.selected_window.clone();
                            }
                        }
                    } else {
                        // Диск сменил вход (link-claude, ротация токена из CLI): данные его,
                        // но закрепление — чистое оформление, его правка из окна не теряется.
                        let presentation = |m: &std::collections::BTreeMap<String, String>| {
                            m.iter()
                                .filter(|(k, _)| crate::model::PRESENTATION_OPTIONS.contains(&k.as_str()))
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<std::collections::BTreeMap<_, _>>()
                        };
                        merge_map(&presentation(&previous.options), &presentation(&current.options), &mut target.options);
                        // Выбор модели/окна — тоже оформление: правка из окна не должна молча откатываться.
                        if current.selected_model != previous.selected_model {
                            target.selected_model = current.selected_model.clone();
                        }
                        if current.selected_window != previous.selected_window {
                            target.selected_window = current.selected_window.clone();
                        }
                    }
                    if current.label != previous.label {
                        target.label = current.label.clone();
                    }
                    if current.enabled != previous.enabled {
                        target.enabled = current.enabled;
                    }
                    if current.created_at != previous.created_at {
                        target.created_at = current.created_at;
                    }
                }
            }
        }
    }
    if local.settings.refresh_seconds != base.settings.refresh_seconds {
        merged.settings.refresh_seconds = local.settings.refresh_seconds;
    }
    if local.settings.show_remaining != base.settings.show_remaining {
        merged.settings.show_remaining = local.settings.show_remaining;
    }
    if local.settings.notify_used_percent != base.settings.notify_used_percent {
        merged.settings.notify_used_percent = local.settings.notify_used_percent;
    }
    if local.settings.launch_at_login != base.settings.launch_at_login {
        merged.settings.launch_at_login = local.settings.launch_at_login;
    }
    if local.settings.show_tray_percent != base.settings.show_tray_percent {
        merged.settings.show_tray_percent = local.settings.show_tray_percent;
    }
    if local.version != base.version {
        merged.version = local.version;
    }
    merged
}

fn merge_map(
    base: &BTreeMap<String, String>,
    local: &BTreeMap<String, String>,
    disk: &mut BTreeMap<String, String>,
) {
    for key in base.keys() {
        if !local.contains_key(key) {
            disk.remove(key);
        }
    }
    for (key, value) in local {
        if base.get(key) != Some(value) {
            disk.insert(key.clone(), value.clone());
        }
    }
}

/// Экспорт в буфер — для диагностики, а не резервная копия ключей.
/// Ошибки сервисов из старых версий могли содержать эхо токена или URL.
pub fn masked_export(data: &State) -> State {
    let mut masked = data.clone();
    for account in &mut masked.accounts {
        // Пути в JSON, окно и каталог Codex — настройки, а не секреты: ради них экспорт и берут.
        // Прячем остальное; вычищать из id/подписей только прячемое и не короче 8 знаков —
        // иначе «300» или «used» превращали подпись карточки в «***».
        let secrets: Vec<_> = account
            .credentials
            .iter()
            .filter(|(key, value)| !value.is_empty() && !store::is_setting_key(key))
            .map(|(_, value)| value.clone())
            .collect();
        let replaceable: Vec<_> = secrets.iter().filter(|v| v.chars().count() >= 8).cloned().collect();
        account.credentials.retain(|key, _| {
            crate::model::credential_fields(account.provider)
                .iter()
                .any(|field| field.key == key)
        });
        for (key, value) in account.credentials.iter_mut() {
            if !value.is_empty() && !store::is_setting_key(key) {
                *value = "***".to_string();
            }
        }
        let linked = account.options.get("claudeCodeSource").map(String::as_str) == Some("true");
        // codexHome — путь, а не секрет: ради него экспорт и берут.
        account.options.retain(|key, _| crate::model::PRESENTATION_OPTIONS.contains(&key.as_str()) || key == "codexHome");
        if linked {
            account
                .options
                .insert("claudeCodeSource".into(), "true".into());
        }
        account.selected_model = None;
        account.selected_window = None;
        for secret in &replaceable {
            account.id = account.id.replace(secret, "***");
            account.label = account.label.replace(secret, "***");
        }
        if let Some(usage) = &mut account.last_usage {
            if usage.error.is_some() {
                usage.error = Some("Текст ошибки скрыт в экспорте".to_string());
            }
            usage.plan_type = None;
            usage.notes.clear();
            for window in &mut usage.windows {
                window.note = None;
                for secret in &replaceable {
                    window.key = window.key.replace(secret, "***");
                    window.label = window.label.replace(secret, "***");
                }
            }
        }
    }
    masked
}

pub static APP: LazyLock<Mutex<AppState>> = LazyLock::new(|| Mutex::new(AppState::new()));

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn account(id: &str) -> Account {
        Account {
            id: id.into(),
            provider: ProviderId::Codex,
            label: id.into(),
            enabled: true,
            credentials: BTreeMap::new(),
            options: BTreeMap::new(),
            created_at: 0,
            last_usage: None,
            selected_model: None,
            selected_window: None,
        }
    }

    #[test]
    fn transient_status_expires_but_not_immediately() {
        assert!(!status_is_expired(None, 10_000));
        assert!(!status_is_expired(Some(6_000), 5_999));
        assert!(status_is_expired(Some(6_000), 6_000));
    }

    #[test]
    fn gui_and_cli_additions_both_survive() {
        let base = State::default();
        let mut local = base.clone();
        let mut disk = base.clone();
        local.accounts.push(account("gui"));
        disk.accounts.push(account("cli"));
        let merged = merge_state(&base, &local, &disk);
        assert_eq!(merged.accounts.len(), 2);
        assert!(merged.accounts.iter().any(|a| a.id == "cli"));
        assert!(merged.accounts.iter().any(|a| a.id == "gui"));
    }

    #[test]
    fn deletions_are_not_resurrected_by_the_other_writer() {
        let mut base = State::default();
        base.accounts.push(account("old"));
        let mut local = base.clone();
        local.accounts.clear(); // GUI removed old
        let mut disk = base.clone();
        disk.accounts.push(account("cli"));
        assert_eq!(
            merge_state(&base, &local, &disk).accounts,
            vec![account("cli")]
        );

        let local = base.clone();
        let disk = State::default(); // CLI removed old while GUI was refreshing it
        assert!(merge_state(&base, &local, &disk).accounts.is_empty());
    }

    #[test]
    fn refresh_from_old_credentials_is_not_merged_into_external_identity() {
        let mut base = State::default();
        base.accounts.push(account("old"));
        let mut local = base.clone();
        local.accounts[0].last_usage = Some(AccountUsage {
            status: crate::model::FetchStatus::Ok,
            windows: Vec::new(),
            plan_type: None,
            notes: Vec::new(),
            error: None,
            updated_at: 42,
            last_ok_at: Some(42),
        });
        let mut disk = base.clone();
        disk.accounts[0].label = "External rename".into();
        disk.accounts[0]
            .credentials
            .insert("external".into(), "synthetic".into());
        let merged = merge_state(&base, &local, &disk);
        assert_eq!(merged.accounts[0].label, "External rename");
        assert_eq!(merged.accounts[0].credentials["external"], "synthetic");
        assert!(merged.accounts[0].last_usage.is_none());
    }

    #[test]
    fn clipboard_export_hides_legacy_errors_and_nested_secrets() {
        let mut data = State::default();
        let mut account = account("Account synthetic-secret");
        account
            .credentials
            .insert("accessToken".into(), "synthetic-secret".into());
        account
            .credentials
            .insert("synthetic-secret-key".into(), String::new());
        account
            .options
            .insert("debug".into(), "synthetic-secret".into());
        account.selected_model = Some("synthetic-secret".into());
        account.last_usage = Some(AccountUsage {
            status: crate::model::FetchStatus::Error,
            windows: Vec::new(),
            plan_type: Some("synthetic-secret".into()),
            notes: vec!["synthetic-secret".into()],
            error: Some("HTTP 500: synthetic-secret".into()),
            updated_at: 1,
            last_ok_at: None,
        });
        data.accounts.push(account);
        let json = serde_json::to_string(&masked_export(&data)).unwrap();
        assert!(!json.contains("synthetic-secret"));
        assert!(json.contains("***"));
        assert!(json.contains("Текст ошибки скрыт в экспорте"));
        assert!(!json.contains("debug"));
    }

    #[test]
    fn live_gui_snapshot_does_not_erase_cli_changes_on_disk() {
        struct DataDir {
            path: std::path::PathBuf,
            previous: Option<std::ffi::OsString>,
        }
        impl Drop for DataDir {
            fn drop(&mut self) {
                if let Some(previous) = &self.previous {
                    std::env::set_var("LIMITBAR_DATA_DIR", previous);
                } else {
                    std::env::remove_var("LIMITBAR_DATA_DIR");
                }
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
        let path = std::env::temp_dir().join(format!("subbar-gui-sync-{}", store::new_id()));
        std::fs::create_dir(&path).unwrap();
        let guard = DataDir {
            previous: std::env::var_os("LIMITBAR_DATA_DIR"),
            path,
        };
        std::env::set_var("LIMITBAR_DATA_DIR", &guard.path);
        let mut gui = AppState::new();
        gui.form.capture(
            "Unsaved draft".into(),
            BTreeMap::from([("accessToken".into(), "synthetic-unsaved-secret".into())]),
        );
        gui.upsert_account(account("initial"));
        gui.flush_save();
        assert!(!gui.dirty);
        assert!(!std::fs::read_to_string(guard.path.join("state.json"))
            .unwrap()
            .contains("synthetic-unsaved-secret"));
        gui.upsert_account(account("gui"));
        std::thread::spawn(|| {
            store::with_state(|disk| {
                disk.accounts.push(account("cli"));
                Ok(((), true))
            })
        })
        .join()
        .unwrap()
        .unwrap();
        gui.flush_save();
        assert!(!gui.dirty);
        let disk = store::load_state();
        assert_eq!(disk.accounts.len(), 3);
        assert!(disk.accounts.iter().any(|a| a.id == "gui"));
        assert!(disk.accounts.iter().any(|a| a.id == "cli"));
        store::with_state(|disk| {
            disk.accounts.retain(|a| a.id != "initial");
            Ok(((), true))
        })
        .unwrap();
        assert!(gui.sync_external());
        assert!(!gui.data.accounts.iter().any(|a| a.id == "initial"));

        let lock = guard.path.join("state.lock");
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        let previous = gui.data.clone();
        assert!(gui.sync_external());
        assert_eq!(
            gui.data, previous,
            "failed reads must preserve the GUI snapshot"
        );
        assert_eq!(
            gui.status_line.as_deref(),
            Some("Ошибка чтения данных — проверь state.json")
        );
        std::fs::remove_dir(&lock).unwrap();
    }

    #[test]
    fn settings_changes_merge_per_field() {
        let base = State::default();
        let mut local = base.clone();
        local.settings.show_remaining = false;
        let mut disk = base.clone();
        disk.settings.refresh_seconds = 900;
        let merged = merge_state(&base, &local, &disk);
        assert_eq!(merged.settings.refresh_seconds, 900);
        assert!(!merged.settings.show_remaining);
    }

    #[test]
    fn окно_вернуть_не_зависит_от_чужих_статусов() {
        let mut app = AppState::from_state(State::default());
        let until = store::now_ms() + UNDO_MS;
        app.removed = vec![(account("first"), 0, until)];
        app.set_status_for("Удалена «first»", UNDO_MS);
        assert!(app.status_until_ms.unwrap() >= until);
        // Короткое сообщение на 6 с не должно обрезать окно «Вернуть».
        app.set_status("Сохранено");
        assert_eq!(app.status_line.as_deref(), Some("Сохранено"));
        assert!(
            app.status_until_ms.unwrap() >= until,
            "чужой статус укоротил окно «Вернуть»"
        );
        // Пока срок не вышел, кнопка жива, даже если статус держится вечно.
        app.status_until_ms = Some(store::now_ms() + 600_000);
        assert!(!app.expire_status());
        assert!(!app.removed.is_empty());
        // Срок «Вернуть» вышел сам — кнопка уходит, свежий статус остаётся.
        app.removed = vec![(account("first"), 0, store::now_ms() - 1)];
        app.set_status("Прошёл час");
        assert!(app.expire_status());
        assert!(app.removed.is_empty(), "кнопка «Вернуть» пережила свой срок");
        assert!(app.status_line.is_some());
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;

    #[test]
    fn closing_popover_preserves_only_in_memory_form_draft() {
        let mut form = FormState::default();
        form.capture(
            "Черновик".into(),
            BTreeMap::from([("accessToken".into(), "synthetic-secret".into())]),
        );
        assert_eq!(form.label, "Черновик");
        assert_eq!(
            form.values.get("accessToken").map(String::as_str),
            Some("synthetic-secret")
        );
        form.switch_provider(
            ProviderId::OpenCodeGo,
            form.label.clone(),
            form.values.clone(),
        );
        assert!(form.values.is_empty());
        form.switch_provider(ProviderId::Codex, form.label.clone(), form.values.clone());
        assert_eq!(
            form.values.get("accessToken").map(String::as_str),
            Some("synthetic-secret")
        );
        form = FormState::default(); // Cancel or successful Save discards all drafts.
        assert!(form.drafts.is_empty());
        assert!(form.values.is_empty());
    }

    #[test]
    fn provider_drafts_are_kept_separately() {
        let mut form = FormState::default();
        let mut codex = BTreeMap::new();
        codex.insert("accessToken".into(), "codex-value".into());
        form.switch_provider(ProviderId::OpenCodeGo, "Мой аккаунт".into(), codex);
        assert!(form.values.is_empty());
        let mut opencode = BTreeMap::new();
        opencode.insert("apiKey".into(), "first-value".into());
        form.switch_provider(ProviderId::CommandCode, "Мой аккаунт".into(), opencode);
        assert!(
            form.values.is_empty(),
            "shared apiKey must not leak to another provider"
        );
        form.switch_provider(
            ProviderId::OpenCodeGo,
            "Мой аккаунт".into(),
            BTreeMap::new(),
        );
        assert_eq!(
            form.values.get("apiKey").map(String::as_str),
            Some("first-value")
        );
        form.switch_provider(ProviderId::Codex, "Мой аккаунт".into(), form.values.clone());
        assert_eq!(
            form.values.get("accessToken").map(String::as_str),
            Some("codex-value")
        );
    }

    #[test]
    fn changing_provider_drops_old_credentials_but_keeps_pin() {
        let mut account = Account {
            id: "account".into(),
            provider: ProviderId::Codex,
            label: "Старый".into(),
            enabled: true,
            credentials: BTreeMap::from([("accessToken".into(), "old".into())]),
            options: BTreeMap::from([
                ("pinned".into(), "true".into()),
                ("codexHome".into(), "other-home".into()),
            ]),
            created_at: 0,
            last_usage: Some(AccountUsage {
                status: crate::model::FetchStatus::Ok,
                windows: Vec::new(),
                plan_type: None,
                notes: Vec::new(),
                error: None,
                updated_at: 0,
                last_ok_at: None,
            }),
            selected_model: Some("old-model".into()),
            selected_window: Some("old-window".into()),
        };
        let form = FormState {
            provider: ProviderId::Custom,
            label: "Новый".into(),
            values: BTreeMap::from([
                ("url".into(), "https://example.com/usage".into()),
                ("accessToken".into(), "must-not-follow".into()),
            ]),
            ..FormState::default()
        };
        apply_form_to_account(&mut account, &form);
        assert_eq!(account.provider, ProviderId::Custom);
        assert_eq!(account.label, "Новый");
        assert_eq!(account.credentials.len(), 1);
        assert!(account.credentials.contains_key("url"));
        assert_eq!(account.options.len(), 1);
        assert_eq!(
            account.options.get("pinned").map(String::as_str),
            Some("true")
        );
        assert!(account.last_usage.is_none());
        assert!(account.selected_model.is_none() && account.selected_window.is_none());
    }

    #[test]
    fn changing_token_discards_usage_from_previous_identity() {
        let mut account = Account {
            id: "synthetic".into(),
            provider: ProviderId::Claude,
            label: "Старый".into(),
            enabled: true,
            credentials: BTreeMap::from([("accessToken".into(), "old".into())]),
            options: BTreeMap::from([("claudeCodeSource".into(), "true".into())]),
            created_at: 0,
            last_usage: Some(AccountUsage {
                status: crate::model::FetchStatus::Ok,
                windows: Vec::new(),
                plan_type: None,
                notes: Vec::new(),
                error: None,
                updated_at: 1,
                last_ok_at: Some(1),
            }),
            selected_model: None,
            selected_window: None,
        };
        let mut form = FormState {
            provider: ProviderId::Claude,
            label: "Новое имя".into(),
            values: account.credentials.clone(),
            ..FormState::default()
        };
        apply_form_to_account(&mut account, &form);
        assert!(account.last_usage.is_some(), "rename must preserve usage");
        assert_eq!(
            account.options.get("claudeCodeSource").map(String::as_str),
            Some("true")
        );
        form.values.insert("accessToken".into(), "new".into());
        apply_form_to_account(&mut account, &form);
        assert!(
            account.last_usage.is_none(),
            "old account's quota must disappear"
        );
        assert!(!account.options.contains_key("claudeCodeSource"));
    }

    #[test]
    fn новый_аккаунт_хранит_обрезанные_значения_как_и_правленый() {
        // Токен из буфера приходит с пробелами и переводом строки: в карточку — как
        // проверяла валидация, без обрезки сохранять нечего.
        let values = BTreeMap::from([
            ("accessToken".into(), "  sk-synthetic-token\n".into()),
            ("codexHome".into(), "   ".into()),
        ]);
        let credentials = trimmed_credentials(&values);
        assert_eq!(
            credentials.get("accessToken").map(String::as_str),
            Some("sk-synthetic-token")
        );
        assert!(!credentials.contains_key("codexHome"), "пустое поле не хранится");

        // Правка через apply_form_to_account даёт тот же результат.
        let mut account = account_for_apply();
        let form = FormState {
            provider: ProviderId::Codex,
            label: "Карточка".into(),
            values,
            ..FormState::default()
        };
        apply_form_to_account(&mut account, &form);
        assert_eq!(account.credentials, credentials);
    }

    fn account_for_apply() -> Account {
        Account {
            id: "synthetic".into(),
            provider: ProviderId::Codex,
            label: "Карточка".into(),
            enabled: true,
            credentials: BTreeMap::new(),
            options: BTreeMap::new(),
            created_at: 0,
            last_usage: None,
            selected_model: None,
            selected_window: None,
        }
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    use crate::model::{FetchStatus, RateLimitWindow};

    fn card(id: &str, pinned: Option<&str>, used: f64, created_at: i64) -> Account {
        let mut options = BTreeMap::new();
        if let Some(order) = pinned {
            options.insert("pinned".to_string(), "true".to_string());
            if !order.is_empty() {
                options.insert("pinOrder".to_string(), order.to_string());
            }
        }
        Account {
            id: id.into(),
            provider: ProviderId::OpenCodeGo,
            label: id.into(),
            enabled: true,
            credentials: BTreeMap::new(),
            options,
            created_at,
            last_usage: Some(AccountUsage {
                status: FetchStatus::Ok,
                windows: vec![RateLimitWindow { key: "5h".into(), label: "5ч".into(), used_percent: used, window_minutes: 300, resets_at: None, note: None }],
                plan_type: None,
                notes: vec![],
                error: None,
                updated_at: 0,
                last_ok_at: Some(0),
            }),
            selected_model: None,
            selected_window: None,
        }
    }

    fn order(accounts: &[Account]) -> Vec<String> {
        let mut sorted = accounts.to_vec();
        sorted.sort_by(list_order);
        sorted.into_iter().map(|a| a.id).collect()
    }

    #[test]
    fn закреплённые_сверху_в_своём_порядке_остальные_по_расходу() {
        // OpenCode закреплён раньше (старый, без порядка), Claude — позже; незакреплённые — по расходу.
        let mut accounts = vec![
            card("oc4", Some(""), 79.0, 100),
            card("claude", Some("5000"), 13.0, 200),
            card("oc2", None, 100.0, 50),
            card("devin", None, 0.0, 60),
        ];
        assert_eq!(order(&accounts), ["oc4", "claude", "oc2", "devin"]);
        assert_eq!(pin_position(&accounts, "claude"), Some((1, 2)));
        assert_eq!(pin_position(&accounts, "oc2"), None, "незакреплённая — не в порядке");

        assert!(move_pinned(&mut accounts, "claude", true), "поднять выше");
        assert_eq!(order(&accounts), ["claude", "oc4", "oc2", "devin"]);
        assert_eq!(pin_position(&accounts, "claude"), Some((0, 2)));
        assert!(!move_pinned(&mut accounts, "claude", true), "выше первой некуда");
        assert!(!move_pinned(&mut accounts, "oc4", false), "ниже последней закреплённой некуда");
        assert!(!move_pinned(&mut accounts, "oc2", true), "незакреплённую не двигаем");

        assert!(move_pinned(&mut accounts, "claude", false));
        assert_eq!(order(&accounts), ["oc4", "claude", "oc2", "devin"]);
    }

    #[test]
    fn новая_карточка_продолжает_нумерацию_и_частый_сервис() {
        let mut accounts = vec![card("oc2", None, 0.0, 0), card("oc6", None, 0.0, 0), card("x", None, 0.0, 0)];
        accounts[0].label = "OpenCode #2".into();
        accounts[1].label = "OpenCode #6".into();
        accounts[2].label = "Рабочий".into();
        assert_eq!(likely_provider(&accounts), ProviderId::OpenCodeGo, "чего больше — то и предлагаем");
        assert_eq!(likely_provider(&[]), ProviderId::Codex);
        assert_eq!(suggested_label(&accounts, ProviderId::OpenCodeGo), "OpenCode #7");
        assert_eq!(suggested_label(&accounts, ProviderId::Claude), "Claude (Pro/Max)", "нумерации нет — имя сервиса");
    }

    #[test]
    fn ручной_номер_из_state_json_не_переполняет_нумерацию() {
        let mut accounts = vec![card("oc", None, 0.0, 0)];
        accounts[0].label = "OpenCode #18446744073709551615".into();
        assert_eq!(
            suggested_label(&accounts, ProviderId::OpenCodeGo),
            "OpenCode #18446744073709551615",
            "потолок не переполняем — разведёт add_account"
        );
        let mut accounts = vec![card("oc", None, 0.0, 0)];
        accounts[0].label = "OpenCode #99999999999999999999".into();
        assert_eq!(
            suggested_label(&accounts, ProviderId::OpenCodeGo),
            "OpenCode Go",
            "нечитаемый номер — просто имя сервиса"
        );
    }

    #[test]
    fn порядок_закрепления_не_меняет_аккаунт_для_обновления() {
        let before = card("claude", Some(""), 13.0, 200);
        let mut after = before.clone();
        after.options.insert("pinOrder".into(), "1".into());
        assert!(same_refresh_identity(&before, &after), "перестановка не должна перезапрашивать лимиты");
        after.options.insert("codexHome".into(), "/tmp".into());
        assert!(!same_refresh_identity(&before, &after));
    }

    #[test]
    fn failed_refresh_keeps_last_good_windows_and_time() {
        let previous = AccountUsage {
            status: FetchStatus::Ok,
            windows: vec![RateLimitWindow {
                key: "weekly".into(),
                label: "7д".into(),
                used_percent: 95.0,
                window_minutes: 10080,
                resets_at: None,
                note: None,
            }],
            plan_type: Some("Plus".into()),
            notes: Vec::new(),
            error: None,
            updated_at: 10,
            last_ok_at: Some(10),
        };
        let failed = AccountUsage {
            status: FetchStatus::Error,
            windows: Vec::new(),
            plan_type: None,
            notes: Vec::new(),
            error: Some("synthetic timeout".into()),
            updated_at: 20,
            last_ok_at: None,
        };
        let merged = keep_last_good(Some(&previous), failed);
        assert_eq!(merged.status, FetchStatus::Error);
        assert_eq!(merged.windows, previous.windows);
        assert_eq!(merged.last_ok_at, Some(10));
        assert_eq!(merged.plan_type.as_deref(), Some("Plus"));
        assert_eq!(merged.error.as_deref(), Some("synthetic timeout"));
    }
}

/// Выполнить замыкание с доступом на чтение к общему состоянию окна.
pub fn with_app<T>(f: impl FnOnce(&AppState) -> T) -> T {
    let app = APP.lock().unwrap_or_else(|e| e.into_inner());
    f(&app)
}

static WORKER: LazyLock<(Sender<WorkerEvent>, Mutex<Receiver<WorkerEvent>>)> =
    LazyLock::new(|| {
        let (tx, rx) = channel::<WorkerEvent>();
        (tx, Mutex::new(rx))
    });

static SUBSCRIBERS: LazyLock<Mutex<Vec<Sender<WorkerEvent>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Подписаться на события рабочих потоков. Приёмник получает копию каждого события.
pub fn subscribe() -> Receiver<WorkerEvent> {
    let (tx, rx) = channel();
    SUBSCRIBERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(tx);
    rx
}

fn broadcast(event: &WorkerEvent) {
    let mut subscribers = SUBSCRIBERS.lock().unwrap_or_else(|e| e.into_inner());
    // Рассылаем всем, мёртвых получателей выбрасываем — список не растёт вечно.
    subscribers.retain(|tx| tx.send(event.clone()).is_ok());
}

pub fn sender() -> Sender<WorkerEvent> {
    WORKER.0.clone()
}

/// Забрать всё, что фоновые потоки закончили с прошлого тика.
pub fn flush_events() -> bool {
    let mut changed = false;
    let receiver = WORKER.1.lock().unwrap_or_else(|e| e.into_inner());
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    drop(receiver);

    if events.is_empty() {
        return false;
    }

    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    // Сначала сверяем правки CLI, потом смотрим завершённые опросы: токен,
    // сменённый вне окна, должен сперва обесценить итоги этой карточки.
    app.sync_external();
    let mut data_changed = false;
    for event in events {
        changed = true;
        // Копия для подписчиков (рассылка — после применения) и без сырых токенов:
        // RefreshDone и RefreshStarted не шлём вовсе (слушать их некому), у DetectDone вычищаем значения учёток.
        let copy = match &event {
            WorkerEvent::RefreshDone { .. } | WorkerEvent::RefreshStarted => None,
            WorkerEvent::DetectDone(found) => Some(WorkerEvent::DetectDone(
                found
                    .iter()
                    .map(|d| {
                        let mut d = d.clone();
                        d.credentials.iter_mut().for_each(|(_, v)| v.clear());
                        d
                    })
                    .collect(),
            )),
            other => Some(other.clone()),
        };
        match event {
            WorkerEvent::RefreshStarted => {
                // Учёт «в полёте» уже сделан атомарно при постановке.
            }
            WorkerEvent::RefreshDone {
                id,
                usage,
                patch,
                generation,
                basis,
            } => {
                if app.active_fetches.remove(&(id.clone(), generation)) {
                    app.inflight = app.inflight.saturating_sub(1);
                }
                app.refreshing = app.inflight > 0;
                // Устаревшие итоги выбрасываем только у карточки, чья идентичность опроса
                // сменилась. Правка другой карточки не должна терять этот итог.
                if generation != app.account_generations.get(&id).copied().unwrap_or(0) {
                    // Числа устарели, но обменянную пару токенов выкинуть нельзя: старый refresh
                    // провайдер уже сжёг. Берём её, только если карточка на тех же ключах, что и опрос.
                    if let Some(account) = app.data.accounts.iter_mut().find(|a| a.id == id && a.credentials == basis) {
                        let mut rotated = false;
                        for (key, value) in patch {
                            let is_credential = crate::model::credential_fields(account.provider).iter().any(|field| field.key == key);
                            if is_credential && !value.is_empty() && account.credentials.get(&key) != Some(&value) {
                                account.credentials.insert(key, value);
                                rotated = true;
                            }
                        }
                        if rotated {
                            app.bump_account_generation(&id);
                            app.save();
                            app.flush_save_now();
                        }
                    }
                    continue;
                }
                let mut identity_changed = false;
                if let Some(account) = app.data.accounts.iter_mut().find(|a| a.id == id) {
                    for (key, value) in patch {
                        let is_credential = crate::model::credential_fields(account.provider)
                            .iter()
                            .any(|field| field.key == key);
                        if is_credential
                            && !value.is_empty()
                            && account.credentials.get(&key) != Some(&value)
                        {
                            account.credentials.insert(key, value);
                            identity_changed = true;
                            data_changed = true;
                        }
                    }
                    // Новый токен от провайдера — почти всегда штатная ротация той же личности (Claude Code
                    // меняет пару раз в ~8 ч). Стирать прошлые окна ради редкого перелогина в чужой аккаунт
                    // значило бы на каждом сбое опроса после ротации показывать «—» и топить карточку.
                    // Прошлые окна подставляются только при сбое и с меткой времени последнего удачного
                    // опроса; первый удачный опрос нового входа их заменит.
                    let usage = keep_last_good(account.last_usage.as_ref(), usage);
                    if account.last_usage.as_ref() != Some(&usage) {
                        account.last_usage = Some(usage);
                        data_changed = true;
                    }
                }
                if identity_changed {
                    app.bump_account_generation(&id);
                    // Новый refresh живёт только у нас: крах до следующего тика оставил бы на диске сожжённый.
                    app.save();
                    app.flush_save_now();
                }
            }
            WorkerEvent::DetectDone(found) => {
                app.detected = found;
            }
            WorkerEvent::Notice(message) => {
                if !message.is_empty() {
                    app.set_status(message);
                }
            }
        }
        if let Some(event) = copy {
            broadcast(&event);
        }
    }
    if data_changed {
        app.save();
    }
    changed
}

pub(crate) fn keep_last_good(
    previous: Option<&AccountUsage>,
    mut usage: AccountUsage,
) -> AccountUsage {
    if !matches!(usage.status, crate::model::FetchStatus::Ok) && usage.windows.is_empty() {
        if let Some(previous) = previous {
            if matches!(previous.status, crate::model::FetchStatus::Ok)
                || previous.last_ok_at.is_some()
            {
                usage.windows = previous.windows.clone();
                usage.last_ok_at = previous.last_ok_at;
                if usage.plan_type.is_none() {
                    usage.plan_type = previous.plan_type.clone();
                }
            }
        }
    }
    usage
}

/// Запустить фоновое обновление одной карточки.
pub fn refresh_account(id: &str) {
    // Снимок и поколение читаем под одним замком: повторные клики
    // не наплодят бесконечно запросов для одной карточки и версии.
    let (account, generation) = {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        let Some(account) = app.data.accounts.iter().find(|a| a.id == id).cloned() else {
            return;
        };
        let generation = app.account_generations.get(id).copied().unwrap_or(0);
        if !app.active_fetches.insert((id.to_string(), generation)) {
            return;
        }
        app.inflight = app.inflight.saturating_add(1);
        app.refreshing = true;
        (account, generation)
    };
    let basis = account.credentials.clone();
    let tx = sender();
    let _ = tx.send(WorkerEvent::RefreshStarted);
    let result = std::thread::Builder::new()
        // id карточек могут прийти из правленого руками файла: в видимые ОС имена
        // потоков их не пускаем — вдруг битый state положил туда ключ.
        .name("limitbar-refresh".to_string())
        .spawn(move || {
            // HTTP-запросы и CLI Devin сами держат конечный потолок времени.
            // Отцепленный внутренний поток продолжал бы работать после старого сторожа на 45 с.
            let (usage, patch) = std::panic::catch_unwind(|| providers::fetch_account(&account))
                .unwrap_or_else(|_| {
                    (
                        AccountUsage {
                            status: crate::model::FetchStatus::Error,
                            windows: Vec::new(),
                            plan_type: None,
                            notes: Vec::new(),
                            error: Some("Внутренняя ошибка обновления".to_string()),
                            updated_at: store::now_ms(),
                            last_ok_at: account.last_usage.as_ref().and_then(|u| u.last_ok_at),
                        },
                        Vec::new(),
                    )
                });
            let _ = tx.send(WorkerEvent::RefreshDone {
                id: account.id,
                usage,
                patch,
                generation,
                basis,
            });
        });
    if let Err(error) = result {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        if app.active_fetches.remove(&(id.to_string(), generation)) {
            app.inflight = app.inflight.saturating_sub(1);
        }
        app.refreshing = app.inflight > 0;
        app.set_status_for("Не удалось запустить обновление", 12_000);
        eprintln!("[subbar] не удалось запустить обновление: {error}");
    }
}

pub fn refresh_all() {
    let ids: Vec<String> = {
        let app = APP.lock().unwrap_or_else(|e| e.into_inner());
        app.data
            .accounts
            .iter()
            .filter(|account| account.enabled)
            .map(|account| account.id.clone())
            .collect()
    };
    if ids.is_empty() {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        let message = if app.data.accounts.is_empty() {
            "Добавь первый аккаунт"
        } else {
            "Все аккаунты выключены"
        };
        app.set_status(message);
        return;
    }
    // И ручное обновление сдвигает автотаймер: иначе следующий тик сразу пошлёт вторую волну.
    // Только настоящая волна: пустой ⌘R отодвигал бы авто-обновление на целый период.
    crate::app::AUTO_REFRESH_LAST.store(crate::store::now_ms(), std::sync::atomic::Ordering::Relaxed);
    for id in ids {
        refresh_account(&id);
    }
}

pub fn detect_credentials() {
    let tx = sender();
    let spawned = std::thread::Builder::new().name("limitbar-detect".into()).spawn(move || {
        // Падение поиска не должно оставить окно ждать ответа до таймаута.
        let found = std::panic::catch_unwind(providers::detect_all).unwrap_or_default();
        let _ = tx.send(WorkerEvent::DetectDone(found));
    });
    if let Err(e) = spawned {
        eprintln!("[subbar] не запустил поиск ключей: {e}");
    }
}

/// Импортировать каждый найденный ключ, которого ещё нет в списке.
pub fn import_detected() -> usize {
    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    let detected = app.detected.clone();
    let mut imported = 0;
    let mut new_ids = Vec::new();
    // Прокси и паузы различают ключи по названию — второй «OpenCode Go» пронумеровать, как при ручном добавлении.
    let mut labels: Vec<String> = app.data.accounts.iter().map(|a| a.label.clone()).collect();
    for item in detected {
        if app
            .data
            .accounts
            .iter()
            .any(|account| matches_detected_account(account, &item))
        {
            continue;
        }
        let id = store::new_id();
        let account = Account {
            id: id.clone(),
            provider: item.provider,
            label: unique_label(&labels, item.label.clone()),
            enabled: true,
            // Как у ручного добавления: пустые поля иначе сотрёт первый же Save формы — и лимиты с ними.
            credentials: trimmed_credentials(&item.credentials.into_iter().collect()),
            options: item.options.into_iter().collect(),
            created_at: store::now_ms(),
            last_usage: None,
            selected_model: None,
            selected_window: None,
        };
        app.bump_account_generation(&id);
        labels.push(account.label.clone());
        app.data.accounts.push(account);
        new_ids.push(id);
        imported += 1;
    }
    if imported > 0 {
        app.save();
        // Как у возврата карточки: импорт на диск сразу, а не на следующем тике.
        app.flush_save_now();
    }
    drop(app);
    for id in new_ids {
        refresh_account(&id);
    }
    imported
}

/// Сервис для новой карточки: которого у человека больше всего карточек (5 ключей OpenCode — значит, и шестой
/// будет OpenCode), без карточек — ChatGPT / Codex.
pub fn likely_provider(accounts: &[Account]) -> ProviderId {
    ProviderId::ALL
        .iter()
        .copied()
        .map(|p| (accounts.iter().filter(|a| a.provider == p).count(), p))
        .filter(|(n, _)| *n > 0)
        // max_by_key при равенстве берёт последний (Custom) — идём с конца, чтобы ничья ушла первому в ALL.
        .rev()
        .max_by_key(|(n, _)| *n)
        .map_or(ProviderId::Codex, |(_, p)| p)
}

/// Название новой карточки, если его не ввели: продолжить нумерацию человека («OpenCode #6» → «OpenCode #7»),
/// а без неё — имя сервиса (повтор получит «#2»).
pub fn suggested_label(accounts: &[Account], provider: ProviderId) -> String {
    let numbered: Vec<(String, u64)> = accounts
        .iter()
        .filter(|a| a.provider == provider)
        .filter_map(|a| {
            let (prefix, number) = a.label.rsplit_once(" #")?;
            Some((prefix.to_string(), number.trim().parse::<u64>().ok()?))
        })
        .collect();
    // Самая частая приставка — это и есть нумерация человека.
    let prefix = numbered
        .iter()
        .map(|(p, _)| p)
        .max_by_key(|p| numbered.iter().filter(|(q, _)| q == *p).count());
    match prefix {
        Some(prefix) => {
            // Номер мог прийти из ручного state.json — потолок не переполняем.
            let next = numbered
                .iter()
                .filter(|(p, _)| p == prefix)
                .map(|(_, n)| *n)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
            format!("{prefix} #{next}")
        }
        None => provider.display_name().to_string(),
    }
}

/// Обрезанные значения формы: токен из буфера приходит с пробелами и переводом строки,
/// а валидация смотрит на обрезанный — хранить надо такой же, как в `apply_form_to_account`.
pub fn trimmed_credentials(values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(key, value)| (key.clone(), value.trim().to_string()))
        .collect()
}

pub fn add_account(form: &FormState) -> Result<String, String> {
    let base_label = if form.label.trim().is_empty() {
        let app = APP.lock().unwrap_or_else(|e| e.into_inner());
        suggested_label(&app.data.accounts, form.provider)
    } else {
        form.label.trim().to_string()
    };
    // Нумеруем дубли: «OpenCode Go» → «OpenCode Go #2».
    let label = {
        let app = APP.lock().unwrap_or_else(|e| e.into_inner());
        let existing: Vec<String> = app.data.accounts.iter().map(|a| a.label.clone()).collect();
        drop(app);
        unique_label(&existing, base_label)
    };
    validate_form(form)?;
    let account = Account {
        id: store::new_id(),
        provider: form.provider,
        label,
        enabled: true,
        credentials: trimmed_credentials(&form.values),
        options: BTreeMap::new(),
        created_at: store::now_ms(),
        last_usage: None,
        selected_model: None,
        selected_window: None,
    };
    let id = account.id.clone();
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        app.upsert_account(account);
        app.set_status("Карточка добавлена");
    }
    refresh_account(&id);
    Ok(id)
}

/// «OpenCode Go» занято → «OpenCode Go #2»; «#3» занято → «#4», а не «#3 #2».
pub(crate) fn unique_label(existing: &[String], base_label: String) -> String {
    if !existing.contains(&base_label) {
        return base_label;
    }
    let (stem, mut n) = match base_label.rsplit_once(" #").and_then(|(s, d)| Some((s.to_string(), d.parse::<u64>().ok()?.checked_add(1)?))) {
        Some(split) => split,
        None => (base_label.clone(), 2),
    };
    loop {
        let candidate = format!("{} #{}", stem, n);
        if !existing.contains(&candidate) {
            break candidate;
        }
        match n.checked_add(1) {
            Some(next) => n = next,
            None => break format!("{} #{}", stem, store::new_id()), // номер из ручного файла у потолка
        }
    }
}

fn apply_form_to_account(account: &mut Account, form: &FormState) {
    let previous_credentials = account.credentials.clone();
    if !form.label.trim().is_empty() {
        account.label = form.label.trim().to_string();
    }
    if account.provider != form.provider {
        account.provider = form.provider;
        account.credentials.clear();
        account.options.retain(|key, _| crate::model::PRESENTATION_OPTIONS.contains(&key.as_str()));
        account.last_usage = None;
        account.selected_model = None;
        account.selected_window = None;
    }
    for (key, value) in &form.values {
        if !crate::model::credential_fields(form.provider)
            .iter()
            .any(|field| field.key == key)
        {
            continue;
        }
        if value.trim().is_empty() {
            account.credentials.remove(key);
        } else {
            account
                .credentials
                .insert(key.clone(), value.trim().to_string());
        }
    }
    // Поле доступа, пролежавшее в options (правка state.json руками/CLI), дублировало бы
    // только что введённое: два разных codexHome, и любая правка читалась бы сменой личности.
    // Форма его не показывала — пустое поле значит «не трогали», переносим, а не теряем.
    for field in crate::model::credential_fields(form.provider) {
        if let Some(old) = account.options.remove(field.key) {
            if !old.trim().is_empty() && account.credentials.get(field.key).is_none_or(|v| v.trim().is_empty()) {
                account.credentials.insert(field.key.to_string(), old.trim().to_string());
            }
        }
    }
    // Если сменился токен или аккаунт, кэшированные числа принадлежат старой идентичности.
    if account.credentials != previous_credentials {
        // Введённый вручную токен Claude больше не связан со связкой ключей
        // Claude Code, даже если карточка изначально пришла из автопоиска.
        account.options.remove("claudeCodeSource");
        account.last_usage = None;
        account.selected_model = None;
        account.selected_window = None;
    }
}

/// Проверка формы до записи: обязательные поля, а для Custom — то, что иначе всплывёт
/// вечной ошибкой на каждом обновлении (адрес без схемы, заголовок без значения, «30m» в окне).
fn validate_form(form: &FormState) -> Result<(), String> {
    validate_credentials(form.provider, &form.values)
}

/// Те же проверки для `subbar add`: иначе CLI записал бы ключ в URL (несекретное поле) открытым.
pub(crate) fn validate_credentials(provider: crate::model::ProviderId, values: &BTreeMap<String, String>) -> Result<(), String> {
    let get = |key: &str| values.get(key).map(|v| v.trim()).unwrap_or("");
    for field in crate::model::credential_fields(provider) {
        if !field.optional && get(field.key).is_empty() {
            return Err(format!("Заполни поле «{}»", field.label));
        }
    }
    if provider == crate::model::ProviderId::Custom {
        let url = get("url").to_ascii_lowercase();
        if !url.starts_with("https://") {
            return Err("URL должен начинаться с https://".into());
        }
        // Провайдер отвергает такой адрес на каждом обновлении, а пароль в несекретном поле виден на экране.
        match reqwest::Url::parse(&get("url")) {
            Err(_) => return Err("URL не разбирается — проверь адрес".into()),
            Ok(u) if !u.username().is_empty() || u.password().is_some() => {
                return Err("Логин и пароль в URL не поддерживаются — передай их заголовком авторизации".into())
            }
            // Ключ в query лежал бы открытым на экране и в state.json: URL — не секретное поле.
            Ok(u) if u.query_pairs().any(|(k, _)| {
                let k = k.to_ascii_lowercase().replace(['-', '_'], "");
                matches!(k.as_str(), "key" | "apikey" | "token" | "accesstoken" | "secret" | "auth" | "password" | "authorization" | "bearer" | "jwt")
            }) => {
                return Err("Ключ в адресе будет виден открытым — перенеси его в «Значение заголовка»".into())
            }
            Ok(_) => {}
        }
        if get("headerName").is_empty() != get("headerValue").is_empty() {
            return Err("Заголовок авторизации: заполни и название, и значение (или оба оставь пустыми)".into());
        }
        if get("headerName").eq_ignore_ascii_case("host") {
            return Err("Заголовок Host задавать нельзя — укажи нужный хост в URL".into());
        }
        if ["content-length", "content-type", "content-encoding", "transfer-encoding", "connection", "te", "trailer", "expect", "upgrade"].iter().any(|h| get("headerName").eq_ignore_ascii_case(h)) {
            return Err("Этот заголовок управляет самим запросом — для авторизации он не годится".into());
        }
        let name = get("headerName");
        if !name.is_empty() && reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
            return Err("Название заголовка — одно слово без пробелов и двоеточий (например, Authorization)".into());
        }
        // Значение с переводом строки (вставка многострочного буфера) иначе давало вечную ошибку на каждом обновлении.
        let value = get("headerValue");
        if !value.is_empty() && reqwest::header::HeaderValue::from_str(&value).is_err() {
            return Err("Значение заголовка — одной строкой, без переводов строк и управляющих знаков".into());
        }
        if get("usedPath").is_empty() && get("remainingPath").is_empty() {
            return Err("Укажи путь к проценту: «использовано, %» или «осталось, %»".into());
        }
        for name in ["usedPath", "remainingPath", "resetPath"] {
            let path = get(name);
            if !path.is_empty() && path.split('.').any(|part| part.trim().is_empty()) {
                return Err(format!("Путь «{path}» с пустым звеном — убери лишнюю точку"));
            }
        }
        let minutes = get("windowMinutes");
        if !minutes.is_empty() && !minutes.parse::<i64>().is_ok_and(|m| m > 0) {
            return Err("Длина окна — целое число минут больше нуля".into());
        }
    }
    Ok(())
}

pub fn update_account(editing_id: &str, form: &FormState) -> Result<(), String> {
    // Проверяем до изменения — то же правило, что в add_account.
    validate_form(form)?;
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        let Some(account) = app.data.accounts.iter_mut().find(|a| a.id == editing_id) else {
            return Err("Карточка не найдена".to_string());
        };
        // Поля доступа живут и в options (codexHome, ручная правка файла) — при смене сервиса пропали бы молча.
        let has_data = account.credentials.values().any(|v| !v.trim().is_empty())
            || crate::model::credential_fields(account.provider)
                .iter()
                .any(|f| account.options.get(f.key).is_some_and(|v| !v.trim().is_empty()));
        if account.provider != form.provider && has_data {
            return Err("Сервис у карточки не меняется — ключ бы пропал. Создай новую карточку".to_string());
        }
        let previous = account.clone();
        apply_form_to_account(account, form);
        // Прокси различает ключи по названию: чужое название при правке тоже нумеруем, как при добавлении.
        if account.label != previous.label {
            let label = account.label.clone();
            let existing: Vec<String> = app.data.accounts.iter().filter(|a| a.id != editing_id).map(|a| a.label.clone()).collect();
            let unique = unique_label(&existing, label);
            if let Some(account) = app.data.accounts.iter_mut().find(|a| a.id == editing_id) {
                account.label = unique;
            }
        }
        let Some(account) = app.data.accounts.iter_mut().find(|a| a.id == editing_id) else {
            return Err("Карточка не найдена".to_string());
        };
        let identity_changed = !same_refresh_identity(&previous, account);
        // Переименование — не повод дёргать сервис; новый ключ или настройки — повод (включение форма не трогает).
        // same_refresh_identity сравнивает и настройки (кроме оформления), так что отдельной проверки не нужно.
        let needs_fetch = identity_changed;
        if identity_changed {
            app.bump_account_generation(editing_id);
        }
        app.save();
        app.set_status("Сохранено");
        if !needs_fetch {
            return Ok(());
        }
    }
    refresh_account(editing_id);
    Ok(())
}

/// Сколько можно вернуть удалённую карточку.
pub const UNDO_MS: i64 = 15_000;

pub fn remove_account(id: &str) {
    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    let Some(index) = app.data.accounts.iter().position(|account| account.id == id) else {
        return;
    };
    let account = app.data.accounts.remove(index);
    let label = account.label.clone();
    if app.expanded_accounts.remove(id) {
        app.removed_expanded.insert(id.to_string());
    }
    app.bump_account_generation(id);
    // Запрос удалённой карточки не должен держать «обновляется» и глушить ⌘R/автоволну до таймаута.
    let before = app.active_fetches.len();
    app.active_fetches.retain(|(fid, _)| fid != id);
    app.inflight = app.inflight.saturating_sub(before - app.active_fetches.len());
    app.refreshing = app.inflight > 0;
    app.save();
    // Удаление сразу, но не насовсем: 15 секунд карточку (с ключом) можно вернуть — «Вернуть» в шапке или ⌘Z.
    // Сначала сам срок, потом сообщение — чтобы оно держалось до того же момента.
    app.removed.push((account, index, store::now_ms() + UNDO_MS));
    app.set_status_for(format!("Удалена «{label}»"), UNDO_MS);
    // Отметку «сообщили» не снимаем: после ⌘Z то же окно иначе прислало бы уведомление повторно.
}

/// Вернуть только что удалённую карточку на её место. true — вернули.
pub fn restore_removed() -> bool {
    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    // Снимаем запись только когда вернуть точно можно: неудачная попытка не должна её съедать.
    let now = store::now_ms();
    app.removed.retain(|(_, _, until)| now < *until);
    let Some(pos) = app.removed.iter().rposition(|(a, _, _)| !app.data.accounts.iter().any(|x| x.id == a.id)) else {
        return false;
    };
    let (account, index, _) = app.removed.remove(pos);
    let label = account.label.clone();
    let id = account.id.clone();
    let at = index.min(app.data.accounts.len());
    app.data.accounts.insert(at, account.clone());
    let was_expanded = app.removed_expanded.remove(&id);
    if was_expanded {
        app.expanded_accounts.insert(id.clone());
    }
    app.bump_account_generation(&id);
    app.save();
    // Запись отмены уже снята — карточку кладём на диск сразу, а не на следующем тике.
    app.flush_save_now();
    if !app.data.accounts.iter().any(|a| a.id == id) {
        // Слияние с диском её выкинуло (другой писатель удалил): отмену не съедаем молча.
        // Окно отмены — обычное: минутный срок растягивал и это сообщение на минуту.
        app.expanded_accounts.remove(&id);
        if was_expanded {
            app.removed_expanded.insert(id.clone());
        }
        app.removed.push((account, index, now + UNDO_MS));
        app.set_status(format!("«{label}» не вернули: её удалили снаружи"));
        return false;
    }
    if app.dirty {
        // Причину отказа записи из flush_save_now не затираем — по ней и чинить.
        let why = app.status_line.clone().unwrap_or_default();
        app.set_status(format!("Возвращена «{label}», но пока не записана на диск — повторю. {why}").trim_end().to_string());
    } else {
        app.set_status(format!("Возвращена «{label}»"));
    }
    let enabled = app.data.accounts.iter().any(|a| a.id == id && a.enabled);
    drop(app);
    // Выключенную карточку и возвращённую не опрашиваем — как и в автоволне.
    if enabled {
        refresh_account(&id);
    }
    true
}

pub fn toggle_account(id: &str) {
    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(account) = app.data.accounts.iter_mut().find(|a| a.id == id) {
        account.enabled = !account.enabled;
        let enabled = account.enabled;
        app.save();
        // Отметку «сообщили» не трогаем: гистерезис снимет её на первом удачном опросе после включения,
        // а снятие здесь давало дубль уведомления на каждый щелчок тумблером.
        drop(app);
        // Включили — опросить сразу, как при возврате из отмены: при выключенном автообновлении числа иначе старые.
        if enabled {
            refresh_account(id);
        }
    }
}

pub fn toggle_pinned(id: &str) {
    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(account) = app.data.accounts.iter_mut().find(|a| a.id == id) {
        if account.pinned() {
            account.options.remove("pinned");
            account.options.remove("pinOrder");
        } else {
            account.options.insert("pinned".to_string(), "true".to_string());
            // Новая закреплённая — последней среди закреплённых: выше её поднимают «Поднять выше».
            account.options.insert("pinOrder".to_string(), crate::store::now_ms().to_string());
        }
        app.save();
    }
}

/// Порядок карточек в списке: закреплённые сверху — в своём порядке (его задаёт человек);
/// остальные — по расходу (самые израсходованные выше), потом по названию.
pub fn list_order(a: &Account, b: &Account) -> std::cmp::Ordering {
    match (a.pinned(), b.pinned()) {
        (true, false) => return std::cmp::Ordering::Less,
        (false, true) => return std::cmp::Ordering::Greater,
        (true, true) => return a.pin_cmp(b),
        (false, false) => {}
    }
    // После неудачного обновления кэшированная срочность остаётся в ранжировании;
    // карточка помечает эти числа устаревшими, а не прячет их.
    let worst = |account: &Account| -> f64 {
        account
            .last_usage
            .as_ref()
            // Пустые окна (опрос упал до первых данных) — в хвост, как «нет данных»: карточка рисует «—», а не 0%.
            // Битый процент (NaN/∞) — тоже «нет данных», а не «0%, здоровая».
            .filter(|usage| !usage.windows.is_empty() && usage.windows.iter().all(|w| w.used_percent.is_finite()))
            .map(|usage| usage.windows.iter().map(|window| crate::util::clamp_percent(window.used_percent)).fold(0.0, f64::max))
            .unwrap_or(-1.0)
    };
    worst(b).total_cmp(&worst(a)).then_with(|| crate::proxy::control::natural_cmp(&a.label, &b.label))
}

/// Где карточка среди закреплённых: (место с 0, сколько всего закреплено). Не закреплена — None.
pub fn pin_position(accounts: &[Account], id: &str) -> Option<(usize, usize)> {
    let mut pinned: Vec<&Account> = accounts.iter().filter(|a| a.pinned()).collect();
    pinned.sort_by(|a, b| a.pin_cmp(b));
    pinned.iter().position(|a| a.id == id).map(|index| (index, pinned.len()))
}

/// Поднять (up) или опустить закреплённую карточку на одно место. Порядок всех закреплённых
/// перенумеровывается 1…n — равные и старые значения не спорят между собой.
pub fn move_pinned(accounts: &mut [Account], id: &str, up: bool) -> bool {
    let mut order: Vec<(String, i64, String)> =
        accounts.iter().filter(|a| a.pinned()).map(|a| (a.id.clone(), a.pin_rank(), a.label.clone())).collect();
    // Тот же порядок, что рисует список (pin_cmp): иначе «#2»/«#10» при равном ранге менялись бы не с тем соседом.
    order.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| crate::proxy::control::natural_cmp(&a.2, &b.2)));
    let Some(index) = order.iter().position(|(pinned_id, ..)| pinned_id == id) else { return false };
    let target = if up { index.checked_sub(1) } else { Some(index + 1).filter(|t| *t < order.len()) };
    let Some(target) = target else { return false };
    order.swap(index, target);
    for (place, (pinned_id, ..)) in order.iter().enumerate() {
        if let Some(account) = accounts.iter_mut().find(|a| &a.id == pinned_id) {
            account.options.insert("pinOrder".to_string(), (place + 1).to_string());
        }
    }
    true
}

/// Меню карточки: «Поднять выше» / «Опустить ниже».
pub fn move_pinned_account(id: &str, up: bool) {
    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    if move_pinned(&mut app.data.accounts, id, up) {
        app.save();
    }
}

pub fn update_settings(patch: impl FnOnce(&mut Settings)) {
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        patch(&mut app.data.settings);
        app.save();
    }
    crate::app::apply_settings();
}

