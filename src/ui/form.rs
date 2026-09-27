use std::collections::BTreeMap;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSAccessibility, NSButton, NSLineBreakMode, NSControlSize, NSFont, NSPopUpButton, NSScrollView, NSSecureTextField, NSTextField, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use super::draw::FontWeight;
use super::kit::{self, CARD_W, GAP, HEADER_H, INSET, ROW, SECTION_H};
use super::list::PANEL_WIDTH;
use crate::model::{credential_fields, CredentialField, ProviderId};

/// Низ экрана: строка сообщения и ряд кнопок.
const FOOTER_H: f64 = 84.0;
/// Потолок формы — тот же, что у списка: иначе resize_popover срежет низ с кнопками.
const MAX_FORM_HEIGHT: f64 = super::list::MAX_PANEL_HEIGHT;
/// Поле доступа в карточке: подпись, поле, (подсказка), отступы.
const FIELD_PAD: f64 = 10.0;
const FIELD_H: f64 = 24.0;
const HINT_H: f64 = 14.0;

pub struct FormView {
    pub view: Retained<NSView>,
    pub provider_popup: Retained<NSPopUpButton>,
    pub label_field: Retained<NSTextField>,
    pub fields: Vec<(String, Retained<NSTextField>)>,
    pub message_label: Retained<NSTextField>,
    pub detect_button: Retained<NSButton>,
    pub provider_order: Vec<ProviderId>,
    pub rendered_provider: ProviderId,
}

fn text_field(frame: NSRect, mtm: MainThreadMarker, placeholder: &str, value: &str, secure: bool) -> Retained<NSTextField> {
    let field: Retained<NSTextField> = if secure {
        let secure_field = NSSecureTextField::initWithFrame(mtm.alloc::<NSSecureTextField>(), frame);
        unsafe { Retained::cast_unchecked(secure_field) }
    } else {
        NSTextField::initWithFrame(mtm.alloc::<NSTextField>(), frame)
    };
    field.setStringValue(&NSString::from_str(value));
    if !placeholder.is_empty() {
        field.setPlaceholderString(Some(&NSString::from_str(placeholder)));
    }
    field.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    field.setControlSize(NSControlSize::Small);
    field
}

fn field_block(field: &CredentialField) -> f64 {
    FIELD_PAD + 15.0 + 4.0 + FIELD_H + if field.hint.is_empty() { 0.0 } else { 4.0 + HINT_H } + FIELD_PAD
}

/// Карточка доступа: поля стопкой; у Devin полей нет — одна строка-пояснение.
fn access_height(provider: ProviderId) -> f64 {
    let fields = credential_fields(provider);
    if fields.is_empty() {
        ROW
    } else {
        fields.iter().map(field_block).sum()
    }
}

fn document_height(provider: ProviderId) -> f64 {
    GAP + SECTION_H + 2.0 * ROW + GAP + SECTION_H + access_height(provider) + GAP
}

impl FormView {
    pub fn new(mtm: MainThreadMarker, controller: &AnyObject, provider: ProviderId, label_value: &str, values: &BTreeMap<String, String>, editing: bool, label_placeholder: &str) -> Self {
        let p = super::Palette::current();
        let doc_height = document_height(provider);
        let visible_height = doc_height.min(MAX_FORM_HEIGHT - HEADER_H - FOOTER_H);
        let height = HEADER_H + visible_height + FOOTER_H;
        let root = kit::FlippedView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_WIDTH, height)));
        // Один порог и для подсказки, и для полосы прокрутки: иначе «Прокрути вниз» без полосы.
        let overflows = doc_height > visible_height;
        let status = if overflows && editing {
            "Поменяй что нужно и нажми «Сохранить» · прокрути вниз"
        } else if overflows {
            "Прокрути вниз — там остальные поля"
        } else if editing {
            "Поменяй что нужно и нажми «Сохранить»"
        } else if credential_fields(provider).is_empty() {
            "Выбери сервис и назови карточку"
        } else {
            "Выбери сервис и укажи доступ"
        };
        kit::header(mtm, &root, if editing { "Изменить аккаунт" } else { "Добавить аккаунт" }, "", status, false, 0.0);

        let scroll = NSScrollView::initWithFrame(mtm.alloc::<NSScrollView>(), NSRect::new(NSPoint::new(0.0, HEADER_H), NSSize::new(PANEL_WIDTH, visible_height)));
        scroll.setDrawsBackground(false);
        scroll.setHasVerticalScroller(overflows);
        scroll.setHasHorizontalScroller(false);
        scroll.setAutohidesScrollers(true);
        // Перевёрнутый документ: при открытии видно начало формы, а не последнее поле.
        let document = kit::FlippedView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_WIDTH, doc_height)));

        // ── сервис и название
        let mut y = GAP;
        kit::section(mtm, &document, y, "АККАУНТ");
        y += SECTION_H;
        let main = kit::card(mtm, &document, y, 2.0 * ROW, vec![ROW]);
        kit::row_label(mtm, &main, 0.0, "Сервис", 120.0).setAccessibilityElement(false);
        let provider_order: Vec<ProviderId> = ProviderId::ALL.to_vec();
        let titles: Vec<&str> = provider_order.iter().map(|id| id.display_name()).collect();
        let selected = provider_order.iter().position(|id| *id == provider).unwrap_or(0);
        let popup = kit::popup(mtm, &main, 0.0, 204.0, &titles, selected, "Сервис", controller, sel!(onProviderChanged:));
        // Сервис у существующей карточки не меняем всегда, даже пустой (update_account отказал бы только при данных):
        // проще, чем объяснять, почему у одних карточек переключатель живой, а у других нет.
        popup.setEnabled(!editing);
        kit::row_label(mtm, &main, ROW, "Название", 120.0).setAccessibilityElement(false);
        let label_field = text_field(
            NSRect::new(NSPoint::new(CARD_W - INSET - 204.0, ROW + (ROW - 22.0) / 2.0), NSSize::new(204.0, 22.0)),
            mtm,
            label_placeholder,
            label_value,
            false,
        );
        label_field.setAccessibilityLabel(Some(&NSString::from_str("Название аккаунта")));
        main.addSubview(&label_field);
        y += 2.0 * ROW + GAP;

        // ── доступ
        kit::section(mtm, &document, y, "ДОСТУП");
        y += SECTION_H;
        let specs = credential_fields(provider);
        let mut lines = Vec::new();
        let mut acc = 0.0;
        for spec in specs.iter().take(specs.len().saturating_sub(1)) {
            acc += field_block(spec);
            lines.push(acc);
        }
        let access = kit::card(mtm, &document, y, access_height(provider), lines);
        let mut fields = Vec::new();
        if specs.is_empty() {
            // У Devin ключей нет: данные — из его локального кэша.
            // Мелкий приглушённый note: обрезанную строку прочитать можно — тултипом.
            kit::note(mtm, &access, (ROW - 15.0) / 2.0, INSET, CARD_W - 2.0 * INSET, "Ключ не нужен: данные берутся из локального кэша Devin CLI");
        }
        let mut top = 0.0;
        for spec in specs {
            let value = values.get(spec.key).cloned().unwrap_or_default();
            let title = if spec.optional { format!("{} · необязательно", spec.label) } else { spec.label.to_string() };
            let caption = kit::text(mtm, &title, INSET, top + FIELD_PAD, CARD_W - 2.0 * INSET, 11.0, FontWeight::Regular, &p.muted);
            // То же название уже на самом поле — иначе VoiceOver читает его дважды.
            caption.setAccessibilityElement(false);
            access.addSubview(&caption);
            let input = text_field(
                NSRect::new(NSPoint::new(INSET, top + FIELD_PAD + 19.0), NSSize::new(CARD_W - 2.0 * INSET, FIELD_H)),
                mtm,
                spec.placeholder,
                &value,
                spec.secret,
            );
            input.setAccessibilityLabel(Some(&NSString::from_str(&title)));
            if !spec.hint.is_empty() {
                // VoiceOver не видит соседнюю метку — подсказку вешаем и на само поле.
                input.setAccessibilityHelp(Some(&NSString::from_str(spec.hint)));
            }
            access.addSubview(&input);
            fields.push((spec.key.to_string(), input));
            if !spec.hint.is_empty() {
                // Та же подсказка уже висит на поле (accessibilityHelp) — вторым текстом VoiceOver прочёл бы её дважды.
                let note = kit::note(mtm, &access, top + FIELD_PAD + 19.0 + FIELD_H + 4.0, INSET, CARD_W - 2.0 * INSET, spec.hint);
                note.setAccessibilityElement(false);
            }
            top += field_block(spec);
        }
        debug_assert!((y + access_height(provider) + GAP - doc_height).abs() < 0.1);

        scroll.setDocumentView(Some(&document));
        root.addSubview(&scroll);

        // ── сообщение и кнопки
        // Сообщения формы бывают длинными (что сделать дальше) — две строки, а не обрезка.
        let message_label = kit::text(mtm, "", 15.0, height - kit::FOOTER_BOTTOM - kit::BUTTON_H - 36.0, PANEL_WIDTH - 30.0, 10.5, FontWeight::Regular, &p.muted);
        message_label.setUsesSingleLineMode(false);
        // С «…»-обрезкой AppKit не переносит строки вовсе — переносим по словам, «…» только в конце второй.
        message_label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        if let Some(cell) = message_label.cell() {
            cell.setTruncatesLastVisibleLine(true);
        }
        message_label.setMaximumNumberOfLines(2);
        message_label.setFrameSize(NSSize::new(PANEL_WIDTH - 26.0, 30.0));
        root.addSubview(&message_label);
        let detect_button = kit::footer_button(mtm, &root, height, kit::MARGIN, 150.0, "Найти на этом Mac", controller, sel!(onDetectCredentials:));
        detect_button.setToolTip(Some(&NSString::from_str("Найти сохранённые входы CLI: Codex, Claude, OpenCode Go, Command Code, Devin")));
        // Правка: поиск входов добавляет новые аккаунты — здесь не нужен; на его месте — «Удалить»
        // (сразу, но 15 секунд можно вернуть: «Вернуть» в шапке списка или ⌘Z).
        detect_button.setHidden(editing);
        if editing {
            let delete = kit::footer_button(mtm, &root, height, kit::MARGIN, 96.0, "Удалить", controller, sel!(onFormDelete:));
            // Атрибуты — шрифт и цвет (NSFont, NSColor): ровно то, чего ждёт NSAttributedString.
            let title = unsafe { objc2_foundation::NSAttributedString::new_with_attributes(&NSString::from_str("Удалить"), &kit::text_attributes(&p.bad())) };
            delete.setAttributedTitle(&title);
            delete.setToolTip(Some(&NSString::from_str("Удалить аккаунт из SubBar — 15 секунд его можно вернуть")));
        }
        let save_x = PANEL_WIDTH - kit::MARGIN - 96.0;
        let cancel_button = kit::footer_button(mtm, &root, height, save_x - 6.0 - 86.0, 86.0, "Отмена", controller, sel!(onFormCancel:));
        cancel_button.setKeyEquivalent(&NSString::from_str("\u{1b}"));
        let save_button = kit::footer_button(mtm, &root, height, save_x, 96.0, if editing { "Сохранить" } else { "Добавить" }, controller, sel!(onFormSave:));
        save_button.setKeyEquivalent(&NSString::from_str("\r"));

        // Tab — сверху вниз: сервис, название, затем поля доступа по порядку. Без цепочки AppKit обходит
        // поля в порядке добавления на экран, и из «Названия» Tab уводил на кнопки. Выключенный
        // выбор сервиса (правка) AppKit пропускает сам.
        unsafe { popup.setNextKeyView(Some(&label_field)) };
        let mut prev: Retained<objc2_app_kit::NSView> = Retained::into_super(Retained::into_super(label_field.clone()));
        for (_, field) in &fields {
            unsafe { prev.setNextKeyView(Some(field)) };
            prev = Retained::into_super(Retained::into_super(field.clone()));
        }
        FormView {
            view: Retained::into_super(root),
            provider_popup: popup,
            label_field,
            fields,
            message_label,
            detect_button,
            provider_order,
            rendered_provider: provider,
        }
    }

    pub fn selected_provider(&self) -> ProviderId {
        let index = self.provider_popup.indexOfSelectedItem();
        self.provider_order
            .get(index.max(0) as usize)
            .copied()
            .unwrap_or(self.rendered_provider)
    }

    pub fn label_value(&self) -> String {
        field_value(&self.label_field)
    }

    pub fn values(&self) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|(key, field)| (key.clone(), field_value(field)))
            .collect()
    }

    pub fn set_message(&self, text: &str, error: bool) {
        let p = super::Palette::current();
        let color = if error { p.bad() } else { p.muted.clone() };
        kit::set_text(&self.message_label, text, &color);
        // Тултип — мышь; подпись — для VoiceOver, если до строки дойдут. Ошибку ещё и объявляем:
        // иначе на ⌘↩ с пустым полем VoiceOver молчит, будто кнопка ничего не сделала.
        let label = NSString::from_str(text);
        self.message_label.setAccessibilityLabel(if text.is_empty() { None } else { Some(&label) });
        if error && !text.is_empty() {
            announce(&self.message_label, &label);
        }
    }
}

// В активном редакторе NSTextField могут быть нажатия, ещё не переданные
// в ячейку, особенно пока закрывается окно. Забираем их до разборки.
fn field_value(field: &NSTextField) -> String {
    field
        .currentEditor()
        .map(|editor| editor.string().to_string())
        .unwrap_or_else(|| field.stringValue().to_string())
}

/// Объявление VoiceOver с высоким приоритетом — прочтёт сразу, не дожидаясь фокуса.
pub(crate) fn announce(element: &NSView, text: &NSString) {
    use objc2_app_kit::{NSAccessibilityAnnouncementKey, NSAccessibilityAnnouncementRequestedNotification, NSAccessibilityPostNotificationWithUserInfo, NSAccessibilityPriorityKey, NSAccessibilityPriorityLevel};
    use objc2_foundation::{NSDictionary, NSNumber};
    let priority = NSNumber::new_isize(NSAccessibilityPriorityLevel::High.0);
    // Ключи и значения — строка и число, ровно то, чего ждёт userInfo объявления.
    unsafe {
        let info: objc2::rc::Retained<NSDictionary<objc2_app_kit::NSAccessibilityNotificationUserInfoKey, AnyObject>> = NSDictionary::from_retained_objects(
            &[NSAccessibilityAnnouncementKey, NSAccessibilityPriorityKey],
            &[Retained::into_super(Retained::into_super(objc2_foundation::NSCopying::copy(text))), Retained::into_super(Retained::into_super(Retained::into_super(priority)))],
        );
        NSAccessibilityPostNotificationWithUserInfo(element, NSAccessibilityAnnouncementRequestedNotification, Some(&info));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn длинной_форме_нужна_прокрутка_а_обычные_влезают() {
        let available = MAX_FORM_HEIGHT - HEADER_H - FOOTER_H;
        assert!(document_height(ProviderId::Custom) > available);
        for provider in ProviderId::ALL {
            if provider != ProviderId::Custom {
                assert!(document_height(provider) <= available, "{} должен влезать без прокрутки", provider.display_name());
            }
        }
    }
}
