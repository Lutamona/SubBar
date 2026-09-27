use objc2_app_kit::NSBezierPath;
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Индекс наведённой карточки. UI работает на главном потоке.
pub static HOVER_ROW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
/// Индекс наведённой кнопки шапки (-1 — ни одной).
pub static HOVER_BUTTON: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

use crate::model::{Account, FetchStatus};
use crate::state::{AppState, Screen};
use crate::ui::draw::{self, FontWeight};
use crate::ui::{Palette, Rect};

pub const PANEL_WIDTH: f64 = 400.0;
/// Высота экранов с формами и не меньше этого — список (он может быть выше: см. `list_max_height`).
pub const MAX_PANEL_HEIGHT: f64 = 580.0;
/// Потолок высоты списка на большом экране: карточек видно больше, прокрутки меньше.
const LIST_CEILING: f64 = 760.0;
static LIST_MAX_BITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Сколько по высоте может занять список: сколько позволяет экран, но не меньше 580 и не больше 760.
pub fn list_max_height() -> f64 {
    let bits = LIST_MAX_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if bits == 0 {
        MAX_PANEL_HEIGHT
    } else {
        f64::from_bits(bits)
    }
}

/// Запомнить высоту экрана (видимую, без строки меню и Dock) — на каждое открытие окна: монитор мог смениться.
pub fn set_screen_height(visible: f64) {
    // NaN прошёл бы clamp насквозь и сломал бы всю раскладку — оставляем прежнюю высоту.
    if !visible.is_finite() {
        return;
    }
    // visibleFrame уже без строки меню и Dock; 90 — стрелка поповера и поля окна, а не их повтор.
    let height = (visible - 90.0).clamp(MAX_PANEL_HEIGHT, LIST_CEILING);
    LIST_MAX_BITS.store(height.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

pub(crate) const HEADER_HEIGHT: f64 = 77.0;
/// Шапка раскрытой карточки до первой строки окон (свёрнутая целиком — COMPACT_CARD_HEIGHT).
const CARD_HEADER: f64 = 60.0;
const COMPACT_CARD_HEIGHT: f64 = 78.0;
/// Строка окна в раскрытой карточке: подпись, сброс и значение — строкой, полоса во всю ширину.
const WINDOW_ROW: f64 = 30.0;
const NOTE_ROW: f64 = 20.0;
const ERROR_ROW: f64 = 23.0;
const CARD_BOTTOM: f64 = 8.0;
/// Кнопки шапки: квадрат и зазор.
const HEADER_BUTTON: f64 = 30.0;
const HEADER_BUTTON_GAP: f64 = 6.0;
const CARD_GAP: f64 = 7.0;
/// Кнопка «Добавить аккаунт» пустого списка: рисуется и ловит клик по одному прямоугольнику.
const EMPTY_BUTTON: Rect = Rect { x: 119.0, y: 227.0, w: 162.0, h: 33.0 };
/// Место под колонку процента справа, которое мета-строка не занимает.
const VALUE_COLUMN_W: f64 = 71.0;
const CARD_X: f64 = 10.0;
const CARD_WIDTH: f64 = PANEL_WIDTH - 20.0;
const INNER_X: f64 = CARD_X + 13.0;
const INNER_RIGHT: f64 = CARD_X + CARD_WIDTH - 13.0;
const PADDING: f64 = 15.0;

#[derive(Debug, Clone)]
pub enum Action {
    RefreshAll,
    AddAccount,
    OpenSettings,
    OpenProxy,
    /// Вернуть только что удалённую карточку.
    UndoRemove,
    AccountMenu(String),
    ToggleAccount(String),
}

/// Что VoiceOver скажет про зону клика: список рисуется сам, родных подписей у него нет.
pub fn a11y_label(state: &AppState, action: &Action) -> String {
    let label_of = |id: &str| state.data.accounts.iter().find(|a| a.id == id);
    match action {
        Action::RefreshAll => "Обновить лимиты".to_string(),
        Action::AddAccount => "Добавить аккаунт".to_string(),
        Action::OpenSettings => "Настройки".to_string(),
        Action::OpenProxy => "Субагенты Claude → OpenCode Go".to_string(),
        Action::UndoRemove => "Вернуть удалённую карточку".to_string(),
        Action::AccountMenu(id) => format!("Меню карточки {}", label_of(id).map_or("", |a| a.label.as_str())),
        Action::ToggleAccount(id) => {
            let Some(account) = label_of(id) else { return String::new() };
            let expanded = if state.expanded_accounts.contains(id) { "свернуть" } else { "раскрыть" };
            if !account.enabled {
                return format!("{}, аккаунт выключен, {expanded}", account.label);
            }
            let percent = |w: &crate::model::RateLimitWindow| {
                let value = if state.data.settings.show_remaining {
                    crate::util::remaining_percent(w.used_percent)
                } else {
                    crate::util::clamp_percent(w.used_percent)
                };
                crate::util::display_percent(value)
            };
            let mode = if state.data.settings.show_remaining { "осталось" } else { "потрачено" };
            match worst_window(account) {
                Some(w) => format!("{}, самое тесное окно {}: {mode} {}%, {expanded}", account.label, w.label, percent(w)),
                None => format!("{}, данных пока нет, {expanded}", account.label),
            }
        }
    }
}

pub struct Layout {
    pub hitboxes: Vec<(Rect, Action)>,
    /// Номера карточек (по порядку показа) — тем же порядком, что и хитбоксы раскрытия.
    /// Прокрунутый список пропускает невидимые карточки: по одному индексу хитбокса номер не восстановить.
    pub toggle_rows: Vec<usize>,
    /// Подсказки нарисованных меток (булавка, «субагенты»): где и что сказать.
    pub tips: Vec<(Rect, &'static str)>,
}

fn row_height(account: &Account, expanded: bool) -> f64 {
    if !expanded {
        return COMPACT_CARD_HEIGHT;
    }
    let Some(usage) = &account.last_usage else {
        return CARD_HEADER + WINDOW_ROW + CARD_BOTTOM;
    };
    // Пока обновление падает, последние удачные значения остаются видны.
    let windows = usage.windows.len().max(1) as f64 * WINDOW_ROW;
    let notes = if !usage.notes.is_empty() {
        NOTE_ROW
    } else {
        0.0
    };
    let error = if shows_error(account, usage) {
        ERROR_ROW
    } else {
        0.0
    };
    CARD_HEADER + windows + notes + error + CARD_BOTTOM
}

fn visible_accounts(state: &AppState) -> Vec<Account> {
    state.accounts_sorted()
}

pub fn visible_account_index(state: &AppState, id: &str) -> Option<usize> {
    state.accounts_sorted_refs().iter().position(|account| account.id == id)
}

fn worst_window(account: &Account) -> Option<&crate::model::RateLimitWindow> {
    account
        .last_usage
        .as_ref()?
        .windows
        .iter()
        .filter(|w| w.used_percent.is_finite())
        .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
}

fn urgent_count(state: &AppState) -> usize {
    state
        .data
        .accounts
        .iter()
        .filter(|account| {
            is_critical(account)
        })
        .count()
}

/// Карточка на исходе: включена, данные свежие, худшее окно в красной зоне. Одно правило для счётчика в шапке и рамки.
fn is_critical(account: &Account) -> bool {
    account.enabled
        && account
            .last_usage
            .as_ref()
            .is_some_and(|u| matches!(u.status, FetchStatus::Ok))
        && worst_window(account).is_some_and(|w| matches!(crate::util::band(w.used_percent), crate::util::Band::Danger))
}

pub fn max_scroll(content_height: f64, viewport_height: f64) -> f64 {
    (content_height - viewport_height).max(0.0)
}

/// По Enter: карточка под курсором, иначе первая, чей верх не под шапкой (шапка сверху закрывает начало списка).
/// `accounts` — номера карточек по порядку показа, те же, что у хитбоксов раскрытия: прокрунутый список
/// пропускает невидимые карточки, и по одному индексу хитбокса номер не восстановить.
pub fn enter_target(hitboxes: &[(Rect, Action)], accounts: &[usize], hover_row: i64, scroll: f64) -> Option<Action> {
    // Номера есть только у хитбоксов раскрытия: сначала отобрать их, потом сопоставлять —
    // кнопки шапки, «Вернуть», чипы и «···» иначе сдвинули бы номера.
    debug_assert_eq!(hitboxes.iter().filter(|(_, a)| matches!(a, Action::ToggleAccount(_))).count(), accounts.len());
    let rows: Vec<(usize, &Rect, &Action)> = hitboxes
        .iter()
        .filter(|(_, action)| matches!(action, Action::ToggleAccount(_)))
        .zip(accounts)
        .map(|((rect, action), account)| (*account, rect, action))
        .collect();
    let target = usize::try_from(hover_row)
        .ok()
        .and_then(|index| rows.iter().find(|(account, _, _)| *account == index))
        // Хитбоксы идут сверху вниз и обрезаны по шапке (y не меньше HEADER_HEIGHT). Прокрутили —
        // карточка ровно у шапки почти наверняка заехала под неё: берём первую ниже.
        .or_else(|| {
            let top = if scroll > 0.5 { HEADER_HEIGHT + 0.5 } else { HEADER_HEIGHT - 0.5 };
            rows.iter().find(|(_, rect, _)| rect.y >= top)
        })
        .or_else(|| rows.first())?;
    Some(target.2.clone())
}

pub fn content_height(state: &AppState) -> f64 {
    if state.screen != Screen::List {
        return MAX_PANEL_HEIGHT;
    }
    let accounts = visible_accounts(state);
    if accounts.is_empty() {
        return EMPTY_BUTTON.y + EMPTY_BUTTON.h + 26.0;
    }
    HEADER_HEIGHT
        + accounts
            .iter()
            .map(|a| row_height(a, state.expanded_accounts.contains(&a.id)) + CARD_GAP)
            .sum::<f64>()
        - CARD_GAP // после последней карточки зазора нет
        + 10.0
}

/// Нарисовать окно строки меню и вернуть кликабельные области.
pub fn draw(state: &AppState, palette: &Palette, scroll: f64, viewport_height: f64) -> Layout {
    let accounts = visible_accounts(state);
    let now = crate::store::now_ms();
    let mut hitboxes = Vec::new();
    let mut toggle_rows = Vec::new();

    // Рисуем кадр целиком — без застрявших пикселей при прокрутке и смене размера.
    palette.background.setFill();
    NSBezierPath::fillRect(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(PANEL_WIDTH, viewport_height),
    ));

    draw_header(state, palette, &mut hitboxes, now);
    if accounts.is_empty() {
        draw_empty(palette);
        hitboxes.push((EMPTY_BUTTON, Action::AddAccount));
        return Layout {
            hitboxes,
            toggle_rows,
            tips: Vec::new(),
        };
    }

    let clip = NSRect::new(
        NSPoint::new(0.0, HEADER_HEIGHT),
        NSSize::new(PANEL_WIDTH, (viewport_height - HEADER_HEIGHT).max(0.0)),
    );
    let context = objc2_app_kit::NSGraphicsContext::currentContext();
    let saved = context.as_ref().map(|ctx| ctx.saveGraphicsState());
    // Без контекста и сохранения нет: обрезка осталась бы висеть на чужом контексте.
    if saved.is_some() {
        objc2_app_kit::NSRectClip(clip);
    }

    let ctx = CardCtx {
        now,
        serving: crate::proxy::control::with_cached(crate::proxy::control::serving_label),
        offline_after: offline_after(state),
    };
    let mut y = HEADER_HEIGHT - scroll;
    let mut tips = Vec::new();
    let hover = HOVER_ROW.load(std::sync::atomic::Ordering::Relaxed);
    for (index, account) in accounts.iter().enumerate() {
        let expanded = state.expanded_accounts.contains(&account.id);
        let height = row_height(account, expanded);
        let visible = y + height > HEADER_HEIGHT && y < viewport_height;
        if visible {
            let hovered = index as i64 == hover;
            let marks = if expanded {
                draw_card(account, y, height, hovered, palette, state, &ctx)
            } else {
                draw_compact_card(account, y, height, hovered, palette, state, &ctx)
            };
            let fully_visible = |r: &Rect| r.y >= HEADER_HEIGHT && r.y + r.h <= viewport_height;
            // Метка «субагенты» ведёт на экран субагентов (если видна не обрезанной шапкой).
            if let Some(chip) = marks.chip.filter(fully_visible) {
                // Сама метка ниже 20 пт — промахнуться легко; зона клика не меньше 28 пт, в пределах видимого.
                let pad = ((28.0 - chip.h) / 2.0).max(0.0);
                let top = (chip.y - pad).max(HEADER_HEIGHT);
                let hit = Rect { x: chip.x - 2.0, y: top, w: chip.w + 4.0, h: (chip.y + chip.h + pad).min(viewport_height) - top };
                hitboxes.push((hit, Action::OpenProxy));
                tips.push((chip, "На этом ключе работают субагенты Claude — клик открывает экран «Субагенты»"));
            }
            if let Some(pin) = marks.pin.filter(fully_visible) {
                tips.push((pin, "Закреплена: всегда сверху, порядок — «Поднять выше» в меню ···; верхняя закреплённая — в строке меню"));
            }
            let menu_top = (y + 2.0).max(HEADER_HEIGHT);
            let menu_bottom = (y + 32.0).min(viewport_height);
            // Частично обрезанная шапка оставила бы невидимую зону клика по меню.
            // Меню кликабельно, пока видна заметная часть кнопки (иначе клик уходит в раскрытие карточки).
            if menu_bottom - menu_top >= 12.0 {
                hitboxes.push((
                    Rect {
                        x: INNER_RIGHT - 25.0,
                        y: menu_top,
                        w: 30.0,
                        h: menu_bottom - menu_top,
                    },
                    Action::AccountMenu(account.id.clone()),
                ));
            }
            hitboxes.push((
                Rect {
                    x: CARD_X,
                    y: y.max(HEADER_HEIGHT),
                    w: CARD_WIDTH,
                    h: (y + height).min(viewport_height) - y.max(HEADER_HEIGHT),
                },
                Action::ToggleAccount(account.id.clone()),
            ));
            toggle_rows.push(index);
        }
        y += height + CARD_GAP;
    }
    draw_scroll_indicator(state, scroll, palette, viewport_height);

    if saved.is_some() {
        if let Some(ctx) = context {
            ctx.restoreGraphicsState();
        }
    }
    Layout {
        hitboxes,
        toggle_rows,
        tips,
    }
}

/// Кнопки шапки слева направо: (где, что делают, иконка SF Symbols, подсказка).
pub fn header_buttons() -> [(Rect, Action, &'static str, &'static str); 4] {
    let items = [
        (Action::AddAccount, "plus", "Добавить аккаунт  +"),
        (Action::RefreshAll, "arrow.clockwise", "Обновить лимиты  R"),
        (Action::OpenProxy, "arrow.left.arrow.right", "Экран «Субагенты» — Claude → OpenCode Go"),
        (Action::OpenSettings, "gearshape", "Настройки  ,"),
    ];
    let count = items.len() as f64;
    let mut x = PANEL_WIDTH - PADDING - count * HEADER_BUTTON - (count - 1.0) * HEADER_BUTTON_GAP;
    items.map(|(action, symbol, tip)| {
            let rect = Rect { x, y: 12.0, w: HEADER_BUTTON, h: HEADER_BUTTON }; // клик — на 2 пт шире сверху и снизу, см. draw_header
            x += HEADER_BUTTON + HEADER_BUTTON_GAP;
            (rect, action, symbol, tip)
        })
}

fn draw_header(state: &AppState, palette: &Palette, hitboxes: &mut Vec<(Rect, Action)>, now: i64) {
    let title_font = draw::font(16.0, FontWeight::Semibold);
    let meta_font = draw::font(10.5, FontWeight::Regular);
    draw::draw_text("SubBar", PADDING, 11.0, &title_font, &palette.text);

    let enabled = state.data.accounts.iter().filter(|a| a.enabled).count();
    let total = state.data.accounts.len();
    let count_label = crate::util::plural(total as u64, "аккаунт", "аккаунта", "аккаунтов");
    let count_x = PADDING + draw::measure("SubBar", &title_font).width + 9.0;
    draw::draw_text(
        &count_label,
        count_x,
        17.0,
        &draw::font(10.0, FontWeight::Regular),
        &palette.muted,
    );
    let last_ok = state
        .data
        .accounts
        .iter()
        .filter(|a| a.enabled)
        .filter_map(|a| {
            a.last_usage
                .as_ref()
                .map(|u| u.last_ok_at.unwrap_or(u.updated_at))
        })
        .max();
    let offline_after = offline_after(state);
    let stale_count = state
        .data
        .accounts
        .iter()
        .filter(|account| account.enabled)
        .filter(|account| {
            // Ещё ни разу не обновлялась (только что добавили) — это ожидание, а не «без свежих данных».
            // «Недоступно» (лимиты не активны, нет входа) — штатное состояние, не сбой: считаем только ошибки.
            account.last_usage.as_ref().is_some_and(|usage| {
                matches!(usage.status, FetchStatus::Error)
                    || (!matches!(usage.status, FetchStatus::Unavailable)
                        && now.saturating_sub(usage.last_ok_at.unwrap_or(usage.updated_at)) > offline_after)
            })
        })
        .count();
    let urgent = urgent_count(state);
    let subtitle = if let Some(status) = &state.status_line {
        // Временный статус («Скопировано») не должен прятать тревогу: исходящие лимиты важнее.
        if urgent > 0 && !status.contains("на исходе") {
            format!("{status} · {urgent} на исходе")
        } else {
            status.clone()
        }
    } else if state.refreshing {
        "Обновляем данные…".to_string()
    } else if total == 0 {
        "Добавь первый аккаунт".to_string()
    } else if enabled == 0 {
        "Все аккаунты выключены".to_string()
    } else if urgent > 0 && stale_count > 0 {
        format!("{} на исходе · {} без свежих данных", urgent, stale_count)
    } else if urgent > 0 {
        format!("{} на исходе", urgent)
    } else if stale_count > 0 {
        format!("{} без свежих данных", stale_count)
    } else if last_ok.is_some() && state.data.accounts.iter().any(|a| a.enabled && a.last_usage.is_none()) {
        "Ожидаем первое обновление новой карточки".to_string()
    } else {
        match last_ok {
            Some(ts) => format!("Обновлено {}", crate::util::format_time_ago(Some(ts), now)),
            None => "Ожидаем первое обновление".to_string(),
        }
    };
    let sub_color = if state.status_line.is_some() || state.refreshing {
        palette.muted.clone()
    } else if urgent > 0 {
        palette.band(100.0)
    } else if stale_count > 0 {
        palette.band(70.0)
    } else {
        palette.muted.clone()
    };
    // Во всю ширину: что значат проценты, подписано в каждой карточке («осталось · 30д»).
    // Только что удалили карточку — рядом «Вернуть» (15 секунд, или ⌘Z).
    let undo = !state.removed.is_empty();
    let link_font = draw::font(10.5, FontWeight::Semibold);
    const UNDO: &str = "Вернуть";
    let link_w = if undo { draw::measure(UNDO, &link_font).width + 12.0 } else { 0.0 };
    let status = draw::ellipsize(&subtitle, &meta_font, PANEL_WIDTH - 2.0 * PADDING - 12.0 - link_w - if undo { 10.0 } else { 0.0 }); // текст с PADDING+12, «Вернуть» через 10
    draw::fill_round_rect(PADDING, 52.0, 6.0, 6.0, 3.0, &sub_color);
    draw::draw_text(&status, PADDING + 12.0, 45.0, &meta_font, &sub_color);
    if undo {
        let x = PADDING + 12.0 + draw::measure(&status, &meta_font).width + 10.0;
        draw::draw_text(UNDO, x, 45.0, &link_font, &palette.accent);
        // Сверху с 45: иначе полоса 40–44 перекрыла бы кнопки шапки и «Настройки» возвращали бы карточку.
        hitboxes.push((Rect { x: x - 5.0, y: 45.0, w: link_w, h: 19.0 }, Action::UndoRemove));
    }

    // Кнопки — одинаковые квадраты с иконками SF Symbols; подписи — во всплывающих подсказках.
    let busy = state.refreshing;
    let hovered_button = HOVER_BUTTON.load(std::sync::atomic::Ordering::Relaxed);
    for (index, (rect, action, symbol, _)) in header_buttons().into_iter().enumerate() {
        let hovered = hovered_button == index as i64;
        // Подмена работает (субагенты уходят в OpenCode) — зелёная точка на ⇄.
        let live = matches!(action, Action::OpenProxy) && crate::proxy::control::active();
        let disabled = busy && matches!(action, Action::RefreshAll);
        let fill = if hovered && !disabled { palette.card_hover.clone() } else { palette.card.clone() };
        draw::fill_round_rect(rect.x, rect.y, rect.w, rect.h, 9.0, &fill);
        draw::stroke_round_rect(rect.x, rect.y, rect.w, rect.h, 9.0, &palette.border, 0.7);
        let (color, tag) = if disabled {
            (palette.faint.colorWithAlphaComponent(0.55), "off")
        } else if hovered {
            (palette.text.clone(), "text")
        } else {
            (palette.muted.clone(), "muted")
        };
        draw::draw_symbol(symbol, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, 13.0, &color, tag);
        if live {
            draw::fill_round_rect(rect.x + rect.w - 10.0, rect.y + 4.0, 6.0, 6.0, 3.0, &palette.good()); // внутри скругления кнопки
        }
        hitboxes.push((Rect { x: rect.x, y: rect.y - 2.0, w: rect.w, h: rect.h + 4.0 }, action));
    }

    palette.border.setFill();
    NSBezierPath::fillRect(NSRect::new(
        NSPoint::new(0.0, HEADER_HEIGHT - 0.5),
        NSSize::new(PANEL_WIDTH, 0.5),
    ));
}

fn draw_scroll_indicator(
    state: &AppState,
    scroll: f64,
    palette: &Palette,
    panel_height: f64,
) {
    let viewport_top = HEADER_HEIGHT + 5.0;
    let viewport_height = panel_height - viewport_top - 5.0;
    let max_scroll = max_scroll(content_height(state), panel_height);
    if max_scroll <= 0.0 || viewport_height <= 0.0 {
        return;
    }
    // Доля видимого — от того же максимума прокрутки, по которому едет ползунок: иначе его длина
    // и ход считались бы от разных высот содержимого.
    let thumb_height =
        (viewport_height * viewport_height / (viewport_height + max_scroll)).max(24.0).min(viewport_height);
    let thumb_y =
        viewport_top + (scroll / max_scroll).clamp(0.0, 1.0) * (viewport_height - thumb_height);
    draw::fill_round_rect(
        PANEL_WIDTH - 5.0,
        viewport_top,
        3.0,
        viewport_height,
        1.5,
        &palette.track,
    );
    draw::fill_round_rect(
        PANEL_WIDTH - 5.0,
        thumb_y,
        3.0,
        thumb_height,
        1.5,
        &palette.muted.colorWithAlphaComponent(0.80),
    );
}

/// Аккаунтов нет — приглашение добавить первый.
fn draw_empty(palette: &Palette) {
    draw::fill_round_rect(176.0, 99.0, 48.0, 48.0, 14.0, &palette.card);
    draw::stroke_round_rect(176.0, 99.0, 48.0, 48.0, 14.0, &palette.border, 0.8);
    // Та же иконка, что на кнопке шапки.
    draw::draw_symbol("plus", 200.0, 123.0, 20.0, &palette.accent, "accent");
    draw::draw_text_centered("Начни с первого аккаунта", PANEL_WIDTH / 2.0, 160.0, &draw::font(15.0, FontWeight::Semibold), &palette.text);
    draw::draw_text_centered("Подключи сервис — лимиты появятся здесь", PANEL_WIDTH / 2.0, 186.0, &draw::font(10.5, FontWeight::Regular), &palette.muted);
    let b = EMPTY_BUTTON;
    draw::fill_round_rect(b.x, b.y, b.w, b.h, 9.0, &palette.accent.colorWithAlphaComponent(0.14));
    draw::draw_text_centered("Добавить аккаунт", PANEL_WIDTH / 2.0, b.y + 8.0, &draw::font(11.0, FontWeight::Semibold), &palette.accent);
}

fn draw_card_surface(account: &Account, y: f64, height: f64, hovered: bool, palette: &Palette) {
    draw::fill_round_rect(
        CARD_X,
        y,
        CARD_WIDTH,
        height - 1.0,
        11.0,
        if hovered {
            &palette.card_hover
        } else {
            &palette.card
        },
    );
    let critical = is_critical(account);
    let border = if critical {
        palette.band(100.0).colorWithAlphaComponent(0.48)
    } else {
        palette.border.clone()
    };
    draw::stroke_round_rect(CARD_X, y, CARD_WIDTH, height - 1.0, 11.0, &border, 0.8);
    if critical {
        draw::fill_round_rect(
            CARD_X + 1.0,
            y + 13.0,
            2.5,
            height - 27.0,
            1.2,
            &palette.band(100.0),
        );
    }
}

/// Что нужно карточкам кроме самой карточки: время, ключ субагентов, порог «данные устарели».
struct CardCtx {
    now: i64,
    /// Подпись карточки, на ключе которой сейчас работают субагенты (прокси включён).
    serving: Option<String>,
    offline_after: i64,
}

/// Через сколько данные считаются несвежими: два периода обновления с запасом, не меньше 7 минут.
fn offline_after(state: &AppState) -> i64 {
    // Автообновление выключено — данные не «устаревают», пользователь сам так решил.
    if state.data.settings.refresh_seconds == 0 {
        return i64::MAX;
    }
    let refresh_ms = (state.data.settings.refresh_seconds as i64) * 1000;
    (refresh_ms.saturating_mul(2).saturating_add(60_000)).max(7 * 60 * 1000)
}

/// План с заглавной («pro» → «Pro»); без плана — сервис, если его нет в названии карточки.
fn plan_label(account: &Account) -> Option<String> {
    if let Some(plan) = account.last_usage.as_ref().and_then(|u| u.plan_type.as_deref()).filter(|p| !p.trim().is_empty()) {
        let mut chars = plan.trim().chars();
        return chars.next().map(|first| first.to_uppercase().chain(chars).collect());
    }
    let service = account.provider.badge();
    // Сервис назван в карточке отдельным словом («Claude Pro», «мой Codex»), а не кусочком чужого
    // («Claudette»): только тогда бейдж лишний.
    let service_lc = service.to_lowercase();
    let label_lc = account.label.to_lowercase();
    let named = label_lc.match_indices(&service_lc).any(|(at, _)| {
        let before = label_lc[..at].chars().next_back();
        let after = label_lc[at + service_lc.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    });
    (!named).then(|| service.to_string())
}

/// Вторая строка карточки: когда вернётся лимит (главное для исчерпанного ключа), план,
/// а устаревшие данные и ошибки — тревожным цветом. В раскрытой сбросы видны у каждого окна —
/// там вместо сброса свежесть данных. (текст, тревожно ли).
fn card_meta(account: &Account, ctx: &CardCtx, expanded: bool) -> (String, bool) {
    let usage = account.last_usage.as_ref();
    let plan = plan_label(account);
    if !account.enabled {
        return (plan.map(|p| format!("Аккаунт выключен · {p}")).unwrap_or_else(|| "Аккаунт выключен".to_string()), false);
    }
    let age = usage.map(|u| u.last_ok_at.unwrap_or(u.updated_at));
    // «Недоступно» — штатное состояние (нет входа, подписка не активна), причина видна ниже на карточке.
    if usage.is_some_and(|u| matches!(u.status, FetchStatus::Unavailable)) {
        return match usage.and_then(|u| u.last_ok_at) {
            Some(ok) => (format!("Недоступно · данные {}", crate::util::format_time_ago(Some(ok), ctx.now)), false),
            None => ("Недоступно · данных ещё нет".to_string(), false),
        };
    }
    if usage.is_some_and(|u| !matches!(u.status, FetchStatus::Ok)) {
        // Время неудачной попытки — не время данных: удачных не было — так и пишем.
        return match usage.and_then(|u| u.last_ok_at) {
            Some(ok) => (format!("Ошибка обновления · данные {}", crate::util::format_time_ago(Some(ok), ctx.now)), true),
            None => ("Ошибка обновления · данных ещё нет".to_string(), true),
        };
    }
    let Some(usage) = usage else {
        return ("Ждём первых данных".to_string(), false);
    };
    let mut parts: Vec<String> = plan.into_iter().collect();
    // Сброс ограничивающего окна — если оно хоть чуть расходовано (у нетронутого сброс ничего не меняет).
    if let Some(window) = worst_window(account).filter(|w| w.used_percent > 0.5 && !expanded) {
        if let Some(reset) = crate::util::format_reset_countdown(window.resets_at, ctx.now) {
            parts.push(format!("сброс через {reset}"));
        }
    }
    let stale = age.is_some_and(|age| ctx.now.saturating_sub(age) > ctx.offline_after);
    if stale {
        parts.push(format!("данные {}", crate::util::format_time_ago(age, ctx.now)));
    } else if parts.is_empty() || usage.windows.is_empty() || expanded {
        parts.push(format!("обновлено {}", crate::util::format_time_ago(age, ctx.now)));
    }
    (parts.join(" · "), stale)
}

/// Где на карточке метки — для клика и подсказок.
#[derive(Default)]
struct CardMarks {
    chip: Option<Rect>,
    pin: Option<Rect>,
}

/// Верх карточки — общий у свёрнутой и раскрытой: точка сервиса, название с метками, крупный процент
/// ограничивающего окна с подписью, строка про сброс и «···».
fn draw_card_top(account: &Account, y: f64, hovered: bool, expanded: bool, palette: &Palette, state: &AppState, ctx: &CardCtx) -> CardMarks {
    let dimmed = !account.enabled;
    let (r, g, b) = account.provider.color();
    draw::fill_round_rect(INNER_X, y + 10.0, 9.0, 9.0, 4.5, &draw::srgb(r, g, b, if dimmed { 0.45 } else { 1.0 }));

    let name_font = draw::font(13.0, FontWeight::Semibold);
    let meta_font = draw::font(10.0, FontWeight::Regular);
    let value_font = draw::mono_font(19.0, FontWeight::Semibold);
    let chip_font = draw::font(8.5, FontWeight::Semibold);
    let value_x = INNER_RIGHT - 26.0;
    let name_x = INNER_X + 15.0;
    let pinned = account.pinned();
    let serving = !dimmed && ctx.serving.as_deref() == Some(account.label.as_str());
    const CHIP: &str = "субагенты";
    let chip_w = if serving { draw::measure(CHIP, &chip_font).width + 10.0 } else { 0.0 };
    let marks_w = (if pinned { 14.0 } else { 0.0 }) + (if serving { chip_w + 6.0 } else { 0.0 });
    let name = draw::ellipsize(&account.label, &name_font, (value_x - draw::measure("100%", &value_font).width - 6.0 - name_x - marks_w).max(30.0));
    draw::draw_text(&name, name_x, y + 5.0, &name_font, if dimmed { &palette.faint } else { &palette.text });
    let mut mark_x = name_x + draw::measure(&name, &name_font).width + 5.0;
    let mut marks = CardMarks::default();
    if pinned {
        // Закреплена — булавка (понятнее абстрактного ромба).
        draw::draw_symbol("pin.fill", mark_x + 4.5, y + 14.0, 9.5, &palette.accent, "accent");
        marks.pin = Some(Rect { x: mark_x - 2.0, y: y + 5.0, w: 13.0, h: 18.0 });
        mark_x += 14.0;
    }
    marks.chip = serving.then(|| {
        // На этом ключе сейчас работают субагенты Claude.
        draw::fill_round_rect(mark_x, y + 8.0, chip_w, 13.0, 4.0, &palette.good().colorWithAlphaComponent(0.12));
        draw::draw_text(CHIP, mark_x + 5.0, y + 8.5, &chip_font, &palette.good());
        Rect { x: mark_x - 2.0, y: y + 5.0, w: chip_w + 4.0, h: 19.0 }
    });

    let worst = worst_window(account);
    match worst {
        // worst_window уже отбросил не-числа.
        Some(worst) => {
            let shown = display_percent(worst.used_percent, state.data.settings.show_remaining);
            // Старые цифры (опрос упал) — не тревожным цветом: шапка их тоже «на исходе» не считает.
            let fetch_failed = account.last_usage.as_ref().is_some_and(|u| !matches!(u.status, FetchStatus::Ok));
            let color = if dimmed {
                palette.faint.clone()
            } else if fetch_failed {
                palette.muted.clone()
            } else {
                palette.band(worst.used_percent)
            };
            draw::draw_text_right(&format!("{}%", crate::util::display_percent(shown)), value_x, y + 20.0, &value_font, &color);
        }
        None => {
            draw::draw_text_right("—", value_x, y + 20.0, &value_font, &palette.faint);
        }
    }

    let (meta, warn) = card_meta(account, ctx, expanded);
    let meta = draw::ellipsize(&meta, &meta_font, (value_x - VALUE_COLUMN_W - name_x).max(70.0));
    let meta_color = if warn && !dimmed { palette.warn() } else { palette.muted.clone() };
    draw::draw_text(&meta, name_x, y + 25.0, &meta_font, &meta_color);

    let mode = if state.data.settings.show_remaining { "осталось" } else { "потрачено" };
    let chevron = if expanded { "⌃" } else { "⌄" };
    let caption = worst.map(|window| format!("{mode} · {}  {chevron}", window.label)).unwrap_or_else(|| format!("{mode} · {chevron}"));
    let caption_font = draw::font(9.0, FontWeight::Regular);
    let caption = draw::ellipsize(&caption, &caption_font, 130.0);
    draw::draw_text_right(&caption, value_x, y + 42.0, &caption_font, &palette.muted);
    draw_menu_button(y, hovered, palette);
    marks
}

/// Строка ошибки под окнами раскрытой карточки: высоту под неё резервирует `row_height`, рисует `draw_card`.
fn shows_error(account: &Account, usage: &crate::model::AccountUsage) -> bool {
    account.enabled && !usage.windows.is_empty() && usage.error.is_some() && matches!(usage.status, FetchStatus::Error)
}

fn draw_compact_card(account: &Account, y: f64, height: f64, hovered: bool, palette: &Palette, state: &AppState, ctx: &CardCtx) -> CardMarks {
    let usage = account.last_usage.as_ref();
    let dimmed = !account.enabled;
    draw_card_surface(account, y, height, hovered, palette);
    let marks = draw_card_top(account, y, hovered, false, palette, state, ctx);
    let meta_font = draw::font(10.0, FontWeight::Regular);
    // Только настоящий сбой: «недоступно» — штатное состояние с подсказкой, не тревога.
    let failed = account.enabled && usage.is_some_and(|u| matches!(u.status, FetchStatus::Error));

    let windows = usage.map(|u| u.windows.as_slice()).unwrap_or(&[]);
    let strip_y = y + 53.0;
    if windows.is_empty() {
        let status = if dimmed {
            if usage.is_some() { "Аккаунт выключен" } else { "Аккаунт выключен · данных нет" }
        } else {
            match usage {
                // Провайдер ответил, а окон нет — не «подключаемся»: так же, как в раскрытой карточке.
                Some(u) if u.error.is_none() && matches!(u.status, FetchStatus::Ok) => "Лимиты пока не активны",
                _ => usage.and_then(|u| u.error.as_deref()).unwrap_or("Подключаемся к провайдеру…"),
            }
        };
        let status_color = if failed {
            palette.band(70.0)
        } else {
            palette.muted.clone()
        };
        let status = draw::ellipsize(status, &meta_font, INNER_RIGHT - INNER_X);
        draw::draw_text(&status, INNER_X, strip_y, &meta_font, &status_color);
        if !failed {
            // Там же и той же толщины, что настоящая полоса (strip_y + 14): иначе при первых данных она прыгала.
            draw::fill_round_rect(
                INNER_X,
                strip_y + 14.0,
                INNER_RIGHT - INNER_X,
                4.5,
                2.2,
                &palette.track,
            );
        }
        return marks;
    }

    let visible = compact_windows(windows);
    // Опрос упал, а окна остались с прошлого раза: полосы серые, как и число в шапке карточки.
    let faded = dimmed || usage.is_some_and(|u| !matches!(u.status, FetchStatus::Ok));
    let gap = 8.0;
    let cell_width =
        (INNER_RIGHT - INNER_X - gap * (visible.len() - 1) as f64) / visible.len() as f64;
    for (index, window) in visible.iter().enumerate() {
        let label = if index == 2 && windows.len() > 3 {
            format!("{} +{}", window.label, windows.len() - 3)
        } else {
            window.label.clone()
        };
        draw_compact_window(
            window,
            &label,
            INNER_X + index as f64 * (cell_width + gap),
            strip_y,
            cell_width,
            palette,
            state,
            dimmed,
            faded,
        );
    }
    marks
}

// В превью из трёх колонок ограничивающее окно есть всегда, даже если
// сервис прислал его четвёртым или дальше. Раскрытая карточка показывает все окна.
fn compact_windows(
    windows: &[crate::model::RateLimitWindow],
) -> Vec<&crate::model::RateLimitWindow> {
    let mut visible: Vec<_> = windows.iter().take(3).collect();
    if windows.len() > 3 {
        if let Some(worst) = windows
            .iter()
            .filter(|w| w.used_percent.is_finite())
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
        {
            if !visible.iter().any(|w| std::ptr::eq(*w, worst)) {
                visible[2] = worst;
            }
        }
    }
    visible
}

fn draw_compact_window(
    window: &crate::model::RateLimitWindow,
    label: &str,
    x: f64,
    y: f64,
    width: f64,
    palette: &Palette,
    state: &AppState,
    dimmed: bool,
    faded: bool,
) {
    let label_font = draw::font(9.5, FontWeight::Semibold);
    let value_font = draw::mono_font(9.5, FontWeight::Semibold);
    let used = window.used_percent;
    let shown = display_percent(used, state.data.settings.show_remaining);
    let shown_text = if used.is_finite() {
        format!("{}%", crate::util::display_percent(shown))
    } else {
        "—".to_string()
    };
    let value_width = draw::measure(&shown_text, &value_font).width;
    let label = draw::ellipsize(label, &label_font, (width - value_width - 5.0).max(16.0));
    let label_color = if dimmed {
        &palette.faint
    } else {
        &palette.muted
    };
    draw::draw_text(&label, x, y, &label_font, label_color);
    let value_color = if dimmed {
        palette.faint.clone()
    } else if used.is_finite() && used >= 60.0 {
        // С 60% число в цвет полоски (янтарь, с 85% красный); ниже — серое, чтобы спокойные окна не пестрили.
        palette.band(used)
    } else {
        palette.muted.clone()
    };
    draw::draw_text_right(&shown_text, x + width, y, &value_font, &value_color);

    let track_y = y + 14.0;
    draw::fill_round_rect(x, track_y, width, 4.5, 2.2, &palette.track);
    if used.is_finite() {
        let fill = (width * shown / 100.0)
            .clamp(0.0, width)
            .max(if shown > 0.0 { 2.0 } else { 0.0 });
        if fill > 0.0 {
            let color = if faded {
                palette.faint.clone()
            } else {
                palette.band(used)
            };
            draw::fill_round_rect(x, track_y, fill, 4.5, 2.2, &color);
        }
    }
}

fn draw_menu_button(y: f64, hovered: bool, palette: &Palette) {
    // Без подложки, пока карточка не под мышью: девять серых квадратиков подряд — шум.
    let x = INNER_RIGHT - 23.0;
    if hovered {
        draw::fill_round_rect(x, y + 4.0, 26.0, 26.0, 7.0, &palette.track.colorWithAlphaComponent(0.7));
    }
    draw::draw_text_centered(
        "···",
        x + 13.0,
        y + 8.0,
        &draw::font(15.0, FontWeight::Semibold),
        if hovered { &palette.muted } else { &palette.faint },
    );
}

fn draw_card(account: &Account, y: f64, height: f64, hovered: bool, palette: &Palette, state: &AppState, ctx: &CardCtx) -> CardMarks {
    let usage = account.last_usage.as_ref();
    let dimmed = !account.enabled;
    draw_card_surface(account, y, height, hovered, palette);
    let marks = draw_card_top(account, y, hovered, true, palette, state, ctx);
    palette.hairline.setFill();
    NSBezierPath::fillRect(NSRect::new(
        NSPoint::new(INNER_X, y + CARD_HEADER - 4.0),
        NSSize::new(INNER_RIGHT - INNER_X, 0.6),
    ));

    let mut row_y = y + CARD_HEADER;
    if let Some(usage) = usage {
        if usage.windows.is_empty() {
            let message = if dimmed {
                "Аккаунт выключен"
            } else {
                usage.error.as_deref().unwrap_or("Лимиты пока не активны")
            };
            let message = draw::ellipsize(
                message,
                &draw::font(10.5, FontWeight::Regular),
                INNER_RIGHT - INNER_X,
            );
            let color = if !dimmed && matches!(usage.status, FetchStatus::Error) {
                palette.band(70.0)
            } else {
                palette.muted.clone()
            };
            draw::draw_text(
                &message,
                INNER_X,
                row_y + 3.0,
                &draw::font(10.5, FontWeight::Regular),
                &color,
            );
            row_y += WINDOW_ROW;
        } else {
            // Здесь видны все окна и сбросы, даже сверх трёх колонок превью.
            for window in &usage.windows {
                draw_window(window, row_y, palette, state, dimmed, !matches!(usage.status, FetchStatus::Ok), ctx.now);
                row_y += WINDOW_ROW;
            }
        }
        if !usage.notes.is_empty() {
            let note_font = draw::font(10.0, FontWeight::Regular);
            let note = draw::ellipsize(
                &usage.notes.join("  ·  "),
                &note_font,
                INNER_RIGHT - INNER_X,
            );
            draw::draw_text(&note, INNER_X, row_y + 3.0, &note_font, &palette.muted);
            row_y += NOTE_ROW;
        }
        if let Some(error) = usage.error.as_deref().filter(|_| shows_error(account, usage)) {
            let error_font = draw::font(10.0, FontWeight::Semibold);
            let last_success = usage
                .last_ok_at
                .or(Some(usage.updated_at))
                .map(|ts| crate::util::format_time_ago(Some(ts), ctx.now));
            let message = match last_success {
                Some(age) => {
                    // Причина важнее «не удалось»: по ней видно, что чинить (ключ, лимит, сеть).
                    format!("{} · данные {}", error, age)
                }
                _ => error.to_string(),
            };
            let message = draw::ellipsize(&message, &error_font, INNER_RIGHT - INNER_X);
            draw::draw_text(
                &message,
                INNER_X,
                row_y + 3.0,
                &error_font,
                &palette.band(70.0),
            );
        }
    } else {
        let text_font = draw::font(10.5, FontWeight::Regular);
        if dimmed {
            draw::draw_text(
                "Аккаунт выключен · данных нет",
                INNER_X,
                row_y + 6.0,
                &text_font,
                &palette.muted,
            );
        } else {
            draw::draw_text(
                "Подключаемся к провайдеру…",
                INNER_X,
                row_y + 6.0,
                &text_font,
                &palette.muted,
            );
            draw::fill_round_rect(
                INNER_X,
                row_y + 20.0,
                INNER_RIGHT - INNER_X,
                5.0,
                2.5,
                &palette.track,
            );
        }
    }
    marks
}

fn display_percent(used: f64, show_remaining: bool) -> f64 {
    if !used.is_finite() {
        return f64::NAN;
    }
    if show_remaining {
        crate::util::remaining_percent(used)
    } else {
        crate::util::clamp_percent(used)
    }
}

/// Строка окна в раскрытой карточке — как полосы лимитов Claude на экране «Субагенты»:
/// подпись и срок сброса слева, значение справа, полоса во всю ширину.
fn draw_window(window: &crate::model::RateLimitWindow, y: f64, palette: &Palette, state: &AppState, dimmed: bool, stale: bool, now: i64) {
    let used = window.used_percent;
    let shown = display_percent(used, state.data.settings.show_remaining);
    let label_font = draw::font(10.5, FontWeight::Semibold);
    let note_font = draw::font(9.5, FontWeight::Regular);
    let value_font = draw::mono_font(11.0, FontWeight::Semibold);
    let label = match window.note.as_deref().filter(|note| note.contains('/') || note.contains("исчерпан")) {
        Some(note) => format!("{} · {}", window.label, note),
        None => window.label.clone(),
    };
    // Неизвестный процент — не здоровый нулевой расход: прочерк рисуем нейтрально.
    let color = if dimmed || stale || !used.is_finite() { palette.faint.clone() } else { palette.band(used) };
    let value = if used.is_finite() { format!("{}%", crate::util::display_percent(shown)) } else { "—".to_string() };
    let value_w = draw::measure(&value, &value_font).width;
    // Без сброса ярлыку отдаём всю строку до процента; со сбросом — 200, остальное сбросу.
    let reset = crate::util::format_reset_countdown(window.resets_at, now);
    let label_budget = if reset.is_some() {
        200.0
    } else {
        (INNER_RIGHT - INNER_X - value_w - 12.0).max(40.0)
    };
    let label = draw::ellipsize(&label, &label_font, label_budget);
    draw::draw_text(&label, INNER_X, y + 3.0, &label_font, if dimmed { &palette.faint } else { &palette.text });
    let label_w = draw::measure(&label, &label_font).width;
    if let Some(reset) = reset {
        let reset = draw::ellipsize(&format!("сброс через {reset}"), &note_font, (INNER_RIGHT - INNER_X - label_w - value_w - 18.0).max(20.0));
        draw::draw_text(&reset, INNER_X + label_w + 7.0, y + 4.0, &note_font, &palette.faint);
    }
    draw::draw_text_right(&value, INNER_RIGHT, y + 3.0, &value_font, &color);

    let bar_width = INNER_RIGHT - INNER_X;
    let bar_y = y + 20.0;
    draw::fill_round_rect(INNER_X, bar_y, bar_width, 5.0, 2.5, &palette.track);
    if used.is_finite() {
        let fill = (bar_width * shown / 100.0).clamp(0.0, bar_width).max(if shown > 0.0 { 5.0 } else { 0.0 });
        if fill > 0.0 {
            draw::fill_round_rect(INNER_X, bar_y, fill, 5.0, 2.5, &color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountUsage, ProviderId, RateLimitWindow};
    use std::collections::BTreeMap;

    fn sample_account() -> Account {
        Account {
            id: "test".to_string(),
            provider: ProviderId::Codex,
            label: "Test".to_string(),
            enabled: true,
            credentials: BTreeMap::new(),
            options: BTreeMap::new(),
            created_at: 0,
            last_usage: Some(AccountUsage {
                status: FetchStatus::Ok,
                windows: vec![
                    RateLimitWindow {
                        key: "5h".to_string(),
                        label: "5ч".to_string(),
                        used_percent: 20.0,
                        window_minutes: 300,
                        resets_at: None,
                        note: None,
                    },
                    RateLimitWindow {
                        key: "7d".to_string(),
                        label: "7д".to_string(),
                        used_percent: 40.0,
                        window_minutes: 10_080,
                        resets_at: None,
                        note: None,
                    },
                ],
                plan_type: None,
                notes: Vec::new(),
                error: None,
                updated_at: 0,
                last_ok_at: Some(0),
            }),
            selected_model: None,
            selected_window: None,
        }
    }

    #[test]
    fn collapsed_card_is_compact_but_expanded_card_preserves_every_window() {
        let account = sample_account();
        assert_eq!(row_height(&account, false), COMPACT_CARD_HEIGHT);
        assert_eq!(
            row_height(&account, true),
            CARD_HEADER + 2.0 * WINDOW_ROW + CARD_BOTTOM
        );
        assert!(row_height(&account, false) < row_height(&account, true));
    }

    #[test]
    fn limiting_window_is_visible_in_compact_preview() {
        let mut account = sample_account();
        let windows = &mut account.last_usage.as_mut().unwrap().windows;
        windows.push(RateLimitWindow {
            key: "30d".into(),
            label: "30д".into(),
            used_percent: 15.0,
            window_minutes: 43200,
            resets_at: None,
            note: None,
        });
        windows.push(RateLimitWindow {
            key: "year".into(),
            label: "год".into(),
            used_percent: 99.0,
            window_minutes: 525600,
            resets_at: None,
            note: None,
        });
        assert_eq!(worst_window(&account).unwrap().label, "год");
        let shown = compact_windows(&account.last_usage.as_ref().unwrap().windows);
        assert_eq!(
            shown.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
            vec!["5ч", "7д", "год"]
        );
        assert_eq!(display_percent(99.0, true), 1.0);
        assert_eq!(display_percent(99.0, false), 99.0);
        assert!(display_percent(f64::NAN, true).is_nan());
        assert!(display_percent(f64::INFINITY, false).is_nan());
    }

    #[test]
    fn long_list_scrolls_and_short_does_not() {
        let three_cards = HEADER_HEIGHT + 3.0 * (COMPACT_CARD_HEIGHT + CARD_GAP) + 10.0;
        assert!(max_scroll(three_cards, 280.0) > 0.0);
        assert_eq!(max_scroll(three_cards, MAX_PANEL_HEIGHT), 0.0);
        assert_eq!(max_scroll(100.0, 280.0), 0.0);
    }

    #[test]
    fn russian_subscription_pluralization_handles_teens() {
        let pl = |n| crate::util::plural(n, "аккаунт", "аккаунта", "аккаунтов");
        assert_eq!(pl(1), "1 аккаунт");
        assert_eq!(pl(3), "3 аккаунта");
        assert_eq!(pl(5), "5 аккаунтов");
        assert_eq!(pl(12), "12 аккаунтов");
    }

    #[test]
    fn enter_expands_the_card_under_the_cursor_even_after_a_scroll() {
        let card = |y: f64, id: &str| (Rect { x: 10.0, y, w: 380.0, h: 78.0 }, Action::ToggleAccount(id.to_string()));
        // Прокрунутый список: первая карточка уехала под шапку, её хитбокса нет — номера сдвинуты.
        let hitboxes = vec![card(HEADER_HEIGHT, "second"), card(HEADER_HEIGHT + 85.0, "third")];
        let accounts = vec![1, 2];
        let under_cursor = enter_target(&hitboxes, &accounts, 2, 0.0);
        assert!(matches!(&under_cursor, Some(Action::ToggleAccount(id)) if id == "third"));
        // Без курсора — первая, чей верх под шапкой (вторая, а не уехавшая первая).
        let first_visible = enter_target(&hitboxes, &accounts, -1, 0.0);
        assert!(matches!(&first_visible, Some(Action::ToggleAccount(id)) if id == "second"));
        // Полностью закрытая шапкой карточка курсором не считается.
        // Как в жизни: хитбокс заехавшей карточки обрезан до HEADER_HEIGHT, отличает её только прокрутка.
        let clipped = vec![card(HEADER_HEIGHT, "first"), card(HEADER_HEIGHT + 40.0, "second")];
        let picked = enter_target(&clipped, &[0, 1], -1, 50.0);
        assert!(matches!(&picked, Some(Action::ToggleAccount(id)) if id == "second"));
        assert!(enter_target(&[], &[], -1, 0.0).is_none());
        // Кнопки шапки и «···» идут в тех же хитбоксах — номера карточек от них не сдвигаются.
        let button = (Rect { x: 0.0, y: 0.0, w: 20.0, h: 20.0 }, Action::RefreshAll);
        let mixed = vec![button.clone(), button.clone(), card(HEADER_HEIGHT, "second"), button, card(HEADER_HEIGHT + 85.0, "third")];
        let hovered = enter_target(&mixed, &[1, 2], 2, 0.0);
        assert!(matches!(&hovered, Some(Action::ToggleAccount(id)) if id == "third"));
    }
}
