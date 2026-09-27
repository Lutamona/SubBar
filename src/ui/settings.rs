use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSAccessibility, NSControlSize, NSFont, NSPopUpButton, NSSwitch, NSTextField, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use super::list::PANEL_WIDTH;

/// Высота экрана настроек.
const HEIGHT: f64 = 434.0;

const REFRESH_OPTIONS: [(u64, &str); 6] = [
    (0, "выключено"),
    (60, "каждую минуту"),
    (300, "каждые 5 минут"),
    (600, "каждые 10 минут"),
    (1800, "каждые 30 минут"),
    (3600, "каждый час"),
];

pub struct SettingsView {
    pub view: Retained<NSView>,
    refresh_popup: Retained<NSPopUpButton>,
    remaining_switch: Retained<NSSwitch>,
    tray_switch: Retained<NSSwitch>,
    login_switch: Retained<NSSwitch>,
    notify_field: Retained<NSTextField>,
    notify_hint: Retained<NSTextField>,
    status_label: Retained<NSTextField>,
    custom_refresh_seconds: Option<u64>,
    initial_refresh_seconds: u64,
    /// Порог как в файле: поле показывает его округлённым, и нетронутое поле не должно сдвигать 87,555 → 87,56.
    initial_notify_percent: f64,
    /// Последний статус, отданный VoiceOver.
    announced: std::cell::RefCell<Option<(String, bool)>>,
}

fn refresh_option_index(seconds: u64) -> (usize, Option<u64>) {
    match REFRESH_OPTIONS
        .iter()
        .position(|(value, _)| *value == seconds)
    {
        Some(index) => (index, None),
        None => (REFRESH_OPTIONS.len(), Some(seconds)),
    }
}

/// Порог, прижатый к двум знакам и без лишних нулей: 90 → «90», 87.5 → «87,5», 0.1+0.2 → «0,3».
fn format_percent(value: f64) -> String {
    // Округление — против хвостов f64 («0,30000000000000004»); 90.0 f64 печатает как «90» сам.
    format!("{}", (value * 100.0).round() / 100.0).replace('.', ",")
}

fn parse_notify_percent(value: &str) -> Option<f64> {
    // Только цифры и запятая/точка (лишние точки отсечёт parse): «1e2», «inf» и «+5» — не проценты.
    let value = value.trim().replace(',', ".");
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let number = value.parse::<f64>().ok()?;
    (number.is_finite() && (0.0..=100.0).contains(&number)).then_some(number)
}

/// Подсказка под порогом уведомлений (и на неё же — текст ошибки ввода).
pub const NOTIFY_HINT: &str = "Когда в окне потрачено столько процентов · 0 — не уведомлять";

impl SettingsView {
    /// `tray` — чья карточка сейчас в строке меню и закреплена ли она (для подсказки под «Числа в строке меню»).
    pub fn new(mtm: MainThreadMarker, controller: &AnyObject, settings: &crate::model::Settings, tray: Option<(&str, bool)>) -> Self {
        use super::draw::FontWeight;
        use super::kit::{self, CARD_W, GAP, HEADER_H, INSET, ROW, SECTION_H};
        let p = super::Palette::current();
        let height = HEIGHT;
        let root = kit::FlippedView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_WIDTH, height)));
        let changed = sel!(onSettingsChanged:);
        let version = concat!("SubBar ", env!("CARGO_PKG_VERSION"));
        // Клавиши списка нигде больше не видны — здесь их место.
        kit::header(mtm, &root, "Настройки", version, "В списке: R обновить · + добавить · , настройки · ⌘Z вернуть · Esc", false, 0.0);

        // ── обновление и уведомления
        let mut y = HEADER_H + GAP;
        kit::section(mtm, &root, y, "ОБНОВЛЕНИЕ И УВЕДОМЛЕНИЯ");
        y += SECTION_H;
        let upd_h = 2.0 * ROW + 20.0;
        let upd = kit::card(mtm, &root, y, upd_h, vec![ROW]);
        kit::row_label(mtm, &upd, 0.0, "Автообновление", 150.0).setAccessibilityElement(false);
        let mut titles: Vec<String> = REFRESH_OPTIONS.iter().map(|(_, t)| t.to_string()).collect();
        let (index, custom_refresh_seconds) = refresh_option_index(settings.refresh_seconds);
        if let Some(seconds) = custom_refresh_seconds {
            // Значение из state.json, правленного руками: «каждые 90061 секунду» не влезло бы в поповер.
            let every = match seconds {
                s if s % 3600 == 0 => format!("{} ч", s / 3600),
                s if s % 60 == 0 => crate::util::plural(s / 60, "минуту", "минуты", "минут"),
                s => format!("{s} с"),
            };
            titles.push(format!("каждые {every} · своё"));
        }
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        let refresh_popup = kit::popup(mtm, &upd, 0.0, 196.0, &refs, index, "Частота автообновления", controller, changed);

        kit::row_label(mtm, &upd, ROW, "Уведомлять при расходе", 220.0).setAccessibilityElement(false);
        let percent_w = 12.0;
        let field_w = 48.0;
        let field_x = CARD_W - INSET - percent_w - 4.0 - field_w;
        let notify_field = NSTextField::initWithFrame(
            mtm.alloc::<NSTextField>(),
            NSRect::new(NSPoint::new(field_x, ROW + (ROW - 21.0) / 2.0), NSSize::new(field_w, 21.0)),
        );
        notify_field.setControlSize(NSControlSize::Small);
        notify_field.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(11.5, unsafe { objc2_app_kit::NSFontWeightRegular })));
        notify_field.setAlignment(objc2_app_kit::NSTextAlignment::Right);
        notify_field.setStringValue(&NSString::from_str(&format_percent(settings.notify_used_percent)));
        // Своего действия у поля нет: Return забирает кнопка «Готово» (keyEquivalent), а уход фокуса
        // (клик по тумблеру) применял бы недописанное «9» из «90». Число коммитят закрытие экрана и поповера.
        if let Some(cell) = notify_field.cell() {
            cell.setSendsActionOnEndEditing(false);
        }
        notify_field.setAccessibilityLabel(Some(&NSString::from_str("Порог уведомления по расходу, от 0 до 100 процентов")));
        notify_field.setAccessibilityHelp(Some(&NSString::from_str(NOTIFY_HINT)));
        upd.addSubview(&notify_field);
        let percent_sign = kit::text(mtm, "%", CARD_W - INSET - percent_w, ROW + (ROW - 17.0) / 2.0, percent_w, 12.5, FontWeight::Regular, &p.muted);
        // «Процентов» уже в подписи поля — отдельный «%» VoiceOver не нужен.
        percent_sign.setAccessibilityElement(false);
        percent_sign.setToolTip(None);
        upd.addSubview(&percent_sign);
        let notify_hint = kit::note(mtm, &upd, 2.0 * ROW - 4.0, INSET, CARD_W - 2.0 * INSET, NOTIFY_HINT);
        y += upd_h + GAP;

        // ── отображение
        kit::section(mtm, &root, y, "ОТОБРАЖЕНИЕ");
        y += SECTION_H;
        let disp = kit::card(mtm, &root, y, 2.0 * ROW + 20.0, vec![ROW]);
        kit::row_label(mtm, &disp, 0.0, "Показывать остаток, а не расход", 280.0).setAccessibilityElement(false);
        let remaining_switch = kit::switch(mtm, &disp, 0.0, settings.show_remaining, "Показывать остаток, а не расход", controller, changed);
        kit::row_label(mtm, &disp, ROW, "Числа в кольцах строки меню", 280.0).setAccessibilityElement(false);
        let tray_switch = kit::switch(mtm, &disp, ROW, settings.show_tray_percent, "Числа в кольцах строки меню", controller, changed);
        // Чья карточка в кольцах — иначе непонятно, откуда числа.
        let tray_hint = match tray {
            // Без согласования рода: название карточки — любое («Codex», «OpenCode Go»).
            Some((label, true)) => format!("Кольца — закреплённая карточка: {label}"),
            Some((label, false)) => format!("Кольца — карточка с наибольшим расходом: {label}"),
            None => "Кольца: закреплённая карточка или самая израсходованная".to_string(),
        };
        kit::note(mtm, &disp, 2.0 * ROW - 4.0, INSET, CARD_W - 2.0 * INSET, &tray_hint);
        y += 2.0 * ROW + 20.0 + GAP;

        // ── система
        kit::section(mtm, &root, y, "СИСТЕМА");
        y += SECTION_H;
        let sys = kit::card(mtm, &root, y, ROW, vec![]);
        kit::row_label(mtm, &sys, 0.0, "Запускать при входе в систему", 280.0).setAccessibilityElement(false);
        let login_switch = kit::switch(mtm, &sys, 0.0, settings.launch_at_login, "Запускать при входе в систему", controller, changed);
        y += ROW;

        debug_assert!(y + 7.0 + 17.0 <= height - kit::FOOTER_BOTTOM - kit::BUTTON_H, "настройки не влезли над кнопками");
        let status_label = kit::text(mtm, "", 15.0, y + 7.0, PANEL_WIDTH - 30.0, 10.5, FontWeight::Regular, &p.muted);
        root.addSubview(&status_label);
        kit::footer_button(mtm, &root, height, kit::MARGIN, 80.0, "Выйти", controller, sel!(onQuit:));
        let export = kit::footer_button(mtm, &root, height, kit::MARGIN + 86.0, 92.0, "Экспорт", controller, sel!(onExport:));
        export.setToolTip(Some(&NSString::from_str("Скопировать данные без секретов — заменит содержимое буфера обмена")));
        export.setAccessibilityHelp(Some(&NSString::from_str("Скопирует данные без секретов и заменит буфер обмена")));
        let close = kit::footer_button(mtm, &root, height, PANEL_WIDTH - kit::MARGIN - 90.0, 90.0, "Готово", controller, sel!(onSettingsClose:));
        close.setKeyEquivalent(&NSString::from_str("\r"));

        SettingsView {
            view: Retained::into_super(root),
            refresh_popup,
            remaining_switch,
            tray_switch,
            login_switch,
            notify_field,
            notify_hint,
            status_label,
            custom_refresh_seconds,
            initial_refresh_seconds: settings.refresh_seconds,
            initial_notify_percent: settings.notify_used_percent,
            announced: Default::default(),
        }
    }

    pub fn refresh_seconds(&self) -> u64 {
        let selected = self.refresh_popup.indexOfSelectedItem();
        if selected < 0 {
            return self.initial_refresh_seconds;
        }
        REFRESH_OPTIONS
            .get(selected as usize)
            .map(|(seconds, _)| *seconds)
            .or(self.custom_refresh_seconds)
            .unwrap_or(self.initial_refresh_seconds)
    }

    pub fn show_remaining(&self) -> bool {
        super::kit::is_on(&self.remaining_switch)
    }

    pub fn show_tray_percent(&self) -> bool {
        super::kit::is_on(&self.tray_switch)
    }

    pub fn launch_at_login(&self) -> bool {
        super::kit::is_on(&self.login_switch)
    }

    pub fn notify_percent(&self) -> Option<f64> {
        // Как в форме: пока поле в редактировании, свежий текст — у редактора.
        let text = self.notify_field.currentEditor().map(|e| e.string().to_string()).unwrap_or_else(|| self.notify_field.stringValue().to_string());
        // Поле не трогали — возвращаем исходное число, а не его округлённый показ.
        if text.trim() == format_percent(self.initial_notify_percent) {
            return Some(self.initial_notify_percent);
        }
        parse_notify_percent(&text)
    }

    pub fn set_notify_message(&self, text: &str, error: bool) {
        let p = super::Palette::current();
        let color = if error { p.bad() } else { p.faint.clone() };
        super::kit::set_text(&self.notify_hint, text, &color);
        // Ошибка уже объявляется вслух; справка поля — обычная подсказка, без дубля.
        if error && !text.is_empty() {
            super::form::announce(&self.notify_hint, &NSString::from_str(text));
        }
    }

    pub fn set_status_message(&self, text: &str, error: bool) {
        let p = super::Palette::current();
        let color = if error { p.bad() } else { p.muted.clone() };
        super::kit::set_text(&self.status_label, text, &color);
        // Как form::set_message: подпись для VoiceOver — полный текст.
        let label = NSString::from_str(text);
        self.status_label.setAccessibilityLabel(if text.is_empty() { None } else { Some(&label) });
        // Тик перерисовывает статус каждые 0,75 с — вслух только новая ошибка, а не та же по кругу.
        let fresh = self.announced.replace(Some((text.to_string(), error))) != Some((text.to_string(), error));
        if fresh && error && !text.is_empty() {
            super::form::announce(&self.status_label, &label);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_refresh_period_is_not_replaced_by_five_minutes() {
        assert_eq!(
            refresh_option_index(120),
            (REFRESH_OPTIONS.len(), Some(120))
        );
        assert_eq!(refresh_option_index(300), (2, None));
    }

    #[test]
    fn percent_round_trips_through_text() {
        assert_eq!(format_percent(87.5), "87,5");
        assert_eq!(format_percent(90.0), "90");
        for x in [0.0, 90.0, 87.5] {
            assert_eq!(parse_notify_percent(&format_percent(x)), Some(x));
        }
    }

    #[test]
    fn invalid_notification_text_does_not_turn_it_off() {
        assert_eq!(parse_notify_percent(""), None);
        assert_eq!(parse_notify_percent("abc"), None);
        assert_eq!(parse_notify_percent("NaN"), None);
        assert_eq!(parse_notify_percent("150"), None);
        assert_eq!(parse_notify_percent("87,5"), Some(87.5));
        assert_eq!(parse_notify_percent("0"), Some(0.0));
        assert_eq!(parse_notify_percent("1.2.3"), None);
        assert_eq!(parse_notify_percent("."), None);
        assert_eq!(parse_notify_percent(" 90 "), Some(90.0));
        assert_eq!(format_percent(0.1 + 0.2), "0,3");
    }
}
