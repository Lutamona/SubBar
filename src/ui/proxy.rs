//! Экран «Субагенты»: шапка (сколько субагентов и с какого момента, живость, главный переключатель),
//! «Модель» (модель, размышление, проверка связи), «Ключи OpenCode Go» — весь запас ключей со статусами
//! (основной, в работе, запас, пауза, исчерпан; клик — сделать основным) и ротация, «Поведение».
//! Любое изменение сохраняется сразу — прокси перечитывает конфиг на лету.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{NSAccessibility, NSApplication, NSButton, NSEvent, NSPopUpButton, NSSwitch, NSTextField, NSTrackingArea, NSTrackingAreaOptions, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use super::draw::{self, FontWeight};
use super::kit::{self, DotView, CARD_W, GAP, HEADER_H, INSET, ROW, SECTION_H};
use super::list::PANEL_WIDTH;
use super::Palette;
use crate::proxy::config::{ProxyConfig, EFFORTS, MODELS};
use crate::proxy::control::{KeyRow, Tone};

const MODEL_TITLES: [&str; 3] = ["deepseek-v4.1-flash", "space-bunny-free", "muse-spark-1.3 · через перевод"];
const EFFORT_TITLES: [&str; 4] = ["как просит Claude", "low", "high", "max"];
const MATCHES: [(&str, &str); 2] = [("haiku", "субагентов haiku"), ("haiku|sonnet", "субагентов haiku и sonnet")];

/// Ширина выпадающих списков: левые края совпадают — ровная колонка.
const POPUP_W: f64 = 222.0;
/// Строка ключа в списке ключей.
const KEY_ROW: f64 = 28.0;
/// Отступ списка ключей от краёв карточки сверху и снизу.
const KEYS_PAD: f64 = 4.0;
/// Ключей, помещающихся в карточку без прокрутки. Дальше список листается колесом:
/// иначе экран растёт выше поповера и кнопка «Готово» уезжает за край.
const KEYS_VISIBLE: usize = 7;
/// Низ экрана: строка сообщения, ряд кнопок, поля.
const FOOTER: f64 = 76.0;
/// Строк в «Поведении»: кого подменять, откат, строка в Claude Code, служба.
const BEHAVIOR_ROWS: f64 = 4.0;

// Подписи идут по индексам настроек: разойдутся длины — выберется не та модель.
const _: () = assert!(MODEL_TITLES.len() == MODELS.len() && EFFORT_TITLES.len() == EFFORTS.len());

const CHECK_HINT: &str = "Проверка связи: ключ, модель, время ответа";
const NO_KEYS_HINT: &str = "Добавь аккаунт OpenCode Go в списке — его ключ появится здесь";

/// Высота карточки ключей: строки (или одна строка-подсказка) и строка ротации.
fn keys_card_height(rows: usize) -> f64 {
    2.0 * KEYS_PAD + rows.clamp(1, keys_visible()) as f64 * KEY_ROW + ROW
}

/// Сколько строк ключей видно без прокрутки: на невысоком экране меньше семи,
/// иначе поповер обрежет низ вместе с «Готово».
fn keys_visible() -> usize {
    let cap = crate::ui::list::list_max_height();
    let fixed = screen_height_at(0);
    ((cap - fixed) / KEY_ROW).floor().clamp(2.0, KEYS_VISIBLE as f64) as usize
}

/// Значения для выпадающего списка: известные плюс текущее из proxy.json, если его там нет.
/// Иначе чужое значение молча превращалось в [0], и любой клик по списку его затирал.
fn options_with_current(titles: &[&str], values: &[&str], current: &str) -> Vec<(String, String)> {
    let mut options: Vec<(String, String)> = values.iter().zip(titles).map(|(value, title)| ((*value).to_string(), (*title).to_string())).collect();
    if !options.iter().any(|(value, _)| value == current) {
        options.push((current.to_string(), format!("своё: {current}")));
    }
    options
}

/// Высота всего экрана при таком числе ключей.
fn screen_height(rows: usize) -> f64 {
    screen_height_at(rows.clamp(1, keys_visible()))
}

/// Высота экрана ровно при `shown` видимых строках ключей (без ограничений).
fn screen_height_at(shown: usize) -> f64 {
    let card = 2.0 * KEYS_PAD + shown as f64 * KEY_ROW + ROW;
    HEADER_H + GAP + SECTION_H + 3.0 * ROW + GAP + SECTION_H + card + GAP + SECTION_H + BEHAVIOR_ROWS * ROW + FOOTER
}

// ─────────── список ключей ───────────

pub struct KeyRowsIvars {
    rows: RefCell<Vec<KeyRow>>,
    hover: Cell<Option<usize>>,
    target: RefCell<Option<Retained<AnyObject>>>,
    /// Прокрутка длинного запаса ключей — целыми строками.
    offset: Cell<f64>,
    /// Недокрученный остаток колеса: трекпад шлёт дельты по 1–2 пикселя, округление их съедало.
    scroll_rest: Cell<f64>,
    /// Тексты подсказок строк: AppKit их не удерживает.
    tips: RefCell<Vec<Retained<NSString>>>,
    /// Какой набор подсказок висит и с какой строки: пересобирать только при смене.
    tips_shown: RefCell<Option<(Vec<(String, String)>, i64)>>,
}

define_class!(
    /// Ключи OpenCode Go строками: кружок основного, название, статус, процент (остаток или потрачено — как в заголовке) с полоской.
    /// Клик — сделать ключ основным (действие `onProxyChanged:` контроллеру).
    #[unsafe(super(NSView))]
    #[name = "SubBarKeyRowsView"]
    #[ivars = KeyRowsIvars]
    pub struct KeyRowsView;

    impl KeyRowsView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            objc2::rc::autoreleasepool(|_| self.paint());
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            for area in self.trackingAreas().iter() {
                self.removeTrackingArea(&area);
            }
            let options = NSTrackingAreaOptions::MouseMoved | NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::ActiveAlways;
            let area = unsafe { NSTrackingArea::initWithRect_options_owner_userInfo(self.mtm().alloc::<NSTrackingArea>(), self.bounds(), options, Some(self), None) };
            self.addTrackingArea(&area);
            unsafe { msg_send![super(self), updateTrackingAreas] }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            // Полоса прокрутки — мёртвая зона для клика, но подсветку наведения на ней не гасим.
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.max_offset() > 0.0 && point.x >= self.bounds().size.width - 5.0 {
                return;
            }
            let hover = self.row_at(event);
            if self.ivars().hover.replace(hover) != hover {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            if self.ivars().hover.replace(None).is_some() {
                self.setNeedsDisplay(true);
            }
        }

        /// Запас длиннее окна: листаем колесом, целыми строками (иначе полстроки в подсказках).
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let max = self.max_offset();
            if max <= 0.0 {
                return;
            }
            let current = self.ivars().offset.get();
            // Колесо без точной прокрутки отдаёт дельту в строках, а не в точках: 1 щелчок — примерно 16 пт.
            let delta = if event.hasPreciseScrollingDeltas() { event.scrollingDeltaY() } else { event.scrollingDeltaY() * 16.0 };
            let want = current + self.ivars().scroll_rest.get() - delta;
            let raw = want.clamp(0.0, max);
            let offset = (raw / KEY_ROW).round() * KEY_ROW;
            // У края остаток не копим: иначе он уводил бы список на строку назад.
            self.ivars().scroll_rest.set(if want <= 0.0 || want >= max { 0.0 } else { raw - offset });
            if offset == current {
                return;
            }
            self.ivars().offset.set(offset);
            self.ivars().hover.set(None);
            self.refresh_tips();
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let Some(index) = self.row_at(event) else { return };
            let before = self.ivars().rows.borrow().iter().position(|r| r.selected);
            {
                let mut rows = self.ivars().rows.borrow_mut();
                if rows.get(index).is_none_or(|r| r.selected) {
                    return;
                }
                for (i, row) in rows.iter_mut().enumerate() {
                    row.selected = i == index;
                }
            }
            self.setNeedsDisplay(true);
            let chosen = self.ivars().rows.borrow().get(index).map(|r| r.key.clone());
            // Сохранить сразу — как у остальных контролов экрана.
            // Сохранение может пересобрать экран и отпустить вид — держим себя до конца.
            let _keep = self.retain();
            let target = self.ivars().target.borrow().clone();
            if let Some(target) = target {
                // Цель — контроллер окна, у него есть onProxyChanged: (своя сильная ссылка: у NSView нет target, в отличие от NSControl).
                unsafe { NSApplication::sharedApplication(self.mtm()).sendAction_to_from(sel!(onProxyChanged:), Some(&target), Some(self)) };
            }
            // Не записалось (битый proxy.json, отказ записи) — выбор кружка должен совпадать с конфигом.
            if chosen.is_some_and(|chosen| crate::proxy::control::load_config().api_key != chosen) {
                let mut rows = self.ivars().rows.borrow_mut();
                for (i, row) in rows.iter_mut().enumerate() {
                    row.selected = Some(i) == before;
                }
                drop(rows);
                self.setNeedsDisplay(true);
            }
        }
    }
);

impl KeyRowsView {
    fn new(mtm: MainThreadMarker, frame: NSRect, target: &AnyObject) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(KeyRowsIvars {
            rows: RefCell::new(Vec::new()),
            hover: Cell::new(None),
            target: RefCell::new(Some(target.retain())),
            offset: Cell::new(0.0),
            scroll_rest: Cell::new(0.0),
            tips: RefCell::new(Vec::new()),
            tips_shown: RefCell::new(None),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn row_at(&self, event: &NSEvent) -> Option<usize> {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        // Полоса прокрутки у правого края — не строка: промах мимо неё не должен менять основной ключ.
        if self.max_offset() > 0.0 && point.x >= self.bounds().size.width - 5.0 {
            return None;
        }
        let index = ((point.y + self.ivars().offset.get()) / KEY_ROW).floor();
        (point.y >= 0.0 && (index >= 0.0) && (index as usize) < self.ivars().rows.borrow().len()).then_some(index as usize)
    }

    /// Насколько список длиннее окна: столько прокручивается колесом.
    fn max_offset(&self) -> f64 {
        let content = self.ivars().rows.borrow().len() as f64 * KEY_ROW;
        (content - self.bounds().size.height).max(0.0)
    }

    /// Строки, поместившиеся в окно (по прокрутке).
    fn visible_range(&self) -> std::ops::Range<usize> {
        let total = self.ivars().rows.borrow().len();
        let first = ((self.ivars().offset.get() / KEY_ROW).round().max(0.0) as usize).min(total);
        let count = ((self.bounds().size.height / KEY_ROW).ceil().max(1.0) as usize).min(total - first);
        first..first + count
    }

    /// Подсказки видимых строк: пересобираются, только когда сменился набор или список доехал до соседней строки.
    fn refresh_tips(&self) {
        let offset = self.ivars().offset.get();
        let top = (offset / KEY_ROW).round() as i64;
        let visible = self.visible_range();
        let key: Vec<(String, String)> = {
            let rows = self.ivars().rows.borrow();
            rows[visible.clone()].iter().map(|r| (r.label.clone(), format!("{}\n{}", r.status, r.tip))).collect()
        };
        if self.ivars().tips_shown.borrow().as_ref() == Some(&(key.clone(), top)) {
            return;
        }
        self.removeAllToolTips();
        let width = self.bounds().size.width;
        let mut tips = self.ivars().tips.borrow_mut();
        tips.clear();
        {
            let rows = self.ivars().rows.borrow();
            let first = visible.start;
            for (i, row) in rows[visible].iter().enumerate() {
                // Прямоугольник — по абсолютной строке, как её рисует paint, иначе после прокрутки
                // подсказка висит на чужом ключе.
                let i = first + i;
                // Название и статус режутся «…» по ширине строки — в подсказке они целиком.
                let text = NSString::from_str(&format!("{} — {}\n{}", row.label, row.status, row.tip));
                let rect = NSRect::new(NSPoint::new(0.0, i as f64 * KEY_ROW - offset), NSSize::new(width, KEY_ROW));
                // owner — сам текст: AppKit зовёт у него description, userData не нужен.
                unsafe { self.addToolTipRect_owner_userData(rect, &text, std::ptr::null_mut()) };
                tips.push(text);
            }
        }
        drop(tips);
        *self.ivars().tips_shown.borrow_mut() = Some((key, top));
    }

    /// Новые строки (лимиты обновились, прокси сменил статус, выбран другой ключ).
    pub fn set_rows(&self, rows: Vec<KeyRow>) {
        let same_len = self.ivars().rows.borrow().len() == rows.len();
        *self.ivars().rows.borrow_mut() = rows;
        // Индекс наведения относится к старому списку — сбрасываем, только если список сменил длину:
        // иначе фоновое обновление раз в 30 с гасило бы подсветку строки под курсором.
        if !same_len {
            self.ivars().hover.set(None);
            // Недокрученный хвост колеса относится к старой длине списка.
            self.ivars().scroll_rest.set(0.0);
        }
        let max = self.max_offset();
        let offset = self.ivars().offset.get();
        self.ivars().offset.set(offset.clamp(0.0, max));
        // Кэш подсказок не сбрасывать: он сравнивает тексты, и без перемен подсказки не пересоздаются
        // (иначе каждые 30 с мигает та, что сейчас читают).
        self.refresh_tips();
        self.setNeedsDisplay(true);
    }

    /// Выбранный основным ключ: (подпись, ключ).
    pub fn selected(&self) -> Option<(String, String)> {
        self.ivars().rows.borrow().iter().find(|r| r.selected).map(|r| (r.label.clone(), r.key.clone()))
    }

    fn paint(&self) {
        let p = Palette::current();
        let bounds = self.bounds();
        let width = bounds.size.width;
        let height = bounds.size.height;
        let offset = self.ivars().offset.get();
        let rows = self.ivars().rows.borrow();
        if rows.is_empty() {
            let font = draw::font(10.5, FontWeight::Regular);
            draw::draw_text_centered(&draw::ellipsize(NO_KEYS_HINT, &font, width - 2.0 * INSET), width / 2.0, (KEY_ROW - 14.0) / 2.0, &font, &p.faint);
            return;
        }
        // Список прокручивается: рисуем со сдвигом и обрезаем по окну — в сохранённом состоянии, как в панели списка (app.rs).
        let context = objc2_app_kit::NSGraphicsContext::currentContext();
        if let Some(context) = &context {
            context.saveGraphicsState();
        }
        objc2_app_kit::NSRectClip(bounds);
        let name_font = draw::font(12.5, FontWeight::Regular);
        let status_font = draw::font(10.0, FontWeight::Regular);
        let value_font = draw::mono_font(11.0, FontWeight::Semibold);
        let hover = self.ivars().hover.get();
        // Колонки справа: число (до «100%») и полоска перед ним — ровно друг под другом во всех строках.
        let value_right = width - INSET;
        let value_w = draw::measure("100%", &value_font).width;
        let bar_w = 44.0;
        let bar_x = value_right - value_w - 8.0 - bar_w;
        for (i, row) in rows.iter().enumerate() {
            let y = i as f64 * KEY_ROW - offset;
            if y + KEY_ROW < 0.0 || y > height {
                continue;
            }
            if hover == Some(i) && !row.selected {
                draw::fill_round_rect(4.0, y + 1.0, width - 8.0, KEY_ROW - 2.0, 7.0, &p.card_hover);
            }
            // Кружок «основной»: как радио-кнопка macOS.
            let (cx, cy) = (INSET + 7.0, y + KEY_ROW / 2.0);
            if row.selected {
                draw::fill_round_rect(cx - 7.0, cy - 7.0, 14.0, 14.0, 7.0, &p.accent);
                draw::fill_round_rect(cx - 2.5, cy - 2.5, 5.0, 5.0, 2.5, &p.card);
            } else {
                draw::stroke_round_rect(cx - 7.0, cy - 7.0, 14.0, 14.0, 7.0, &p.border, 1.2);
            }
            let dim = row.dim;
            let name_x = INSET + 22.0;
            // Название важнее статуса: режется, только если не оставляет статусу и 70 пт.
            let name = draw::ellipsize(&row.label, &name_font, (bar_x - 12.0 - name_x - 70.0).max(60.0));
            draw::draw_text(&name, name_x, y + (KEY_ROW - 16.0) / 2.0, &name_font, if dim { &p.faint } else { &p.text });
            let status_x = name_x + draw::measure(&name, &name_font).width + 8.0;
            let status = draw::ellipsize(&row.status, &status_font, (bar_x - 10.0 - status_x).max(20.0));
            let status_color = match row.tone {
                Tone::Ok => p.good(),
                Tone::Warn => p.warn(),
                Tone::Off => p.faint.clone(),
            };
            draw::draw_text(&status, status_x, y + (KEY_ROW - 13.0) / 2.0, &status_font, &status_color);
            if let (Some(shown), Some(used)) = (row.shown, row.used) {
                let color = if dim { p.faint.clone() } else { p.band(used) };
                draw::draw_text_right(&format!("{}%", crate::util::display_percent(shown)), value_right, y + (KEY_ROW - 15.0) / 2.0, &value_font, &color);
                let bar_y = y + KEY_ROW / 2.0 - 2.0;
                draw::fill_round_rect(bar_x, bar_y, bar_w, 4.0, 2.0, &p.track);
                // Та же округлённая цифра, что в подписи: иначе «1%» на пустом треке и толстая «2%».
                let fill = (bar_w * crate::util::display_percent(shown) as f64 / 100.0).clamp(0.0, bar_w);
                if fill > 0.0 {
                    draw::fill_round_rect(bar_x, bar_y, fill, 4.0, 2.0, &color);
                }
            }
        }
        drop(rows);
        if let Some(context) = &context {
            context.restoreGraphicsState();
        }
        // Полоса прокрутки: запас не влез — об этом должен говорить сам список.
        let content = self.ivars().rows.borrow().len() as f64 * KEY_ROW;
        if content > height {
            let thumb = (height * height / content).clamp(18.0, height);
            let at = (offset / self.max_offset().max(1.0)).clamp(0.0, 1.0) * (height - thumb);
            draw::fill_round_rect(width - 3.0, at, 2.0, thumb, 1.0, &p.faint);
        }
    }
}

// ─────────── экран ───────────

pub struct ProxyView {
    pub view: Retained<NSView>,
    keys_view: Retained<KeyRowsView>,
    model_popup: Retained<NSPopUpButton>,
    effort_popup: Retained<NSPopUpButton>,
    match_popup: Retained<NSPopUpButton>,
    enabled_switch: Retained<NSSwitch>,
    rotate_switch: Retained<NSSwitch>,
    fallback_switch: Retained<NSSwitch>,
    service_switch: Retained<NSSwitch>,
    statusline_switch: Retained<NSSwitch>,
    dot: Retained<DotView>,
    status_label: Retained<NSTextField>,
    meta_label: Retained<NSTextField>,
    check_note: Retained<NSTextField>,
    message_label: Retained<NSTextField>,
    last_message: RefCell<Option<(String, bool)>>,
    show_remaining: bool,
    /// Значения за списками (в списках может быть чужое из proxy.json — его не затираем).
    models: Vec<String>,
    efforts: Vec<String>,
    matches: Vec<String>,
}

impl ProxyView {
    pub fn new(mtm: MainThreadMarker, controller: &AnyObject, cfg: &ProxyConfig, keys: Vec<KeyRow>, service: bool, statusline: bool, show_remaining: bool) -> Self {
        let p = Palette::current();
        let height = screen_height(keys.len());
        let root = kit::FlippedView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_WIDTH, height)));
        let action = sel!(onProxyChanged:);

        // ── шапка: заголовок и счётчики, живость, главный переключатель
        let head = kit::header(mtm, &root, "Субагенты", "", "", true, 104.0);
        // Главный переключатель — справа в шапке, по краю кнопок списка (15 пт), на уровне заголовка.
        let enabled_switch = kit::switch_at(mtm, &root, PANEL_WIDTH - 15.0, 21.0, cfg.enabled, "Подмена субагентов", controller, action);
        let label_right = kit::switch_left(&enabled_switch) - 8.0;
        let label_w = draw::measure("Подмена", &draw::font(10.5, FontWeight::Semibold)).width.ceil();
        root.addSubview(&kit::text(mtm, "Подмена", label_right - label_w, 14.5, label_w, 10.5, FontWeight::Semibold, &p.muted));
        let meta_label = head.meta;
        let status_label = head.status;
        let dot = head.dot.expect("шапка с точкой");

        // ── модель: что отвечает субагентам, и проверка всей цепочки
        let mut y = HEADER_H + GAP;
        kit::section(mtm, &root, y, "МОДЕЛЬ");
        y += SECTION_H;
        let model_card = kit::card(mtm, &root, y, 3.0 * ROW, vec![ROW, 2.0 * ROW]);
        kit::row_label(mtm, &model_card, 0.0, "Модель", 120.0).setAccessibilityElement(false);
        let models = options_with_current(&MODEL_TITLES, &MODELS, &cfg.model);
        let model_idx = models.iter().position(|(value, _)| *value == cfg.model).unwrap_or(0);
        let model_titles: Vec<&str> = models.iter().map(|(_, title)| title.as_str()).collect();
        let model_popup = kit::popup(mtm, &model_card, 0.0, POPUP_W, &model_titles, model_idx, "Модель для субагентов", controller, action);
        kit::row_label(mtm, &model_card, ROW, "Размышление", 120.0).setAccessibilityElement(false);
        let efforts = options_with_current(&EFFORT_TITLES, &EFFORTS, &cfg.effort);
        let effort_idx = efforts.iter().position(|(value, _)| *value == cfg.effort).unwrap_or(0);
        let effort_titles: Vec<&str> = efforts.iter().map(|(_, title)| title.as_str()).collect();
        let effort_popup = kit::popup(mtm, &model_card, ROW, POPUP_W, &effort_titles, effort_idx, "Уровень размышления", controller, action);
        let check_y = 2.0 * ROW;
        let check_note = kit::note(mtm, &model_card, check_y + (ROW - 15.0) / 2.0, INSET, CARD_W - 2.0 * INSET - 104.0, CHECK_HINT);
        let check = kit::row_button(mtm, &model_card, check_y, "Проверить", 92.0, controller, sel!(onProxyCheck:));
        check.setToolTip(Some(&NSString::from_str("Крошечный запрос через прокси: ключ, модель, время ответа")));
        y += 3.0 * ROW + GAP;

        // ── ключи: весь запас — какой основной, какой в работе, какие в запасе, на паузе, исчерпаны
        let mode = if show_remaining { "ОСТАЛОСЬ" } else { "ПОТРАЧЕНО" };
        kit::section(mtm, &root, y, &format!("КЛЮЧИ OPENCODE GO · {mode}"));
        y += SECTION_H;
        let keys_h = keys_card_height(keys.len());
        let list_h = keys_h - ROW - 2.0 * KEYS_PAD;
        let keys_card = kit::card(mtm, &root, y, keys_h, vec![keys_h - ROW]);
        let keys_view = KeyRowsView::new(mtm, NSRect::new(NSPoint::new(0.0, KEYS_PAD), NSSize::new(CARD_W, list_h)), controller);
        keys_view.set_rows(keys);
        keys_view.setAccessibilityLabel(Some(&NSString::from_str("Ключи OpenCode Go")));
        keys_card.addSubview(&keys_view);
        let rotate_y = keys_h - ROW;
        kit::row_label(mtm, &keys_card, rotate_y, "Кончился лимит — брать следующий ключ", 290.0).setAccessibilityElement(false);
        let rotate_switch = kit::switch(mtm, &keys_card, rotate_y, cfg.rotate, "Кончился лимит ключа — взять ключ с наибольшим запасом", controller, action);
        y += keys_h + GAP;

        // ── поведение
        kit::section(mtm, &root, y, "ПОВЕДЕНИЕ");
        y += SECTION_H;
        let behavior = kit::card(mtm, &root, y, BEHAVIOR_ROWS * ROW, vec![ROW, 2.0 * ROW, 3.0 * ROW]);
        kit::row_label(mtm, &behavior, 0.0, "Кого подменять", 130.0).setAccessibilityElement(false);
        // Своё правило (задано в терминале) показываем как есть и не затираем.
        let matches = options_with_current(
            &MATCHES.iter().map(|(_, title)| *title).collect::<Vec<_>>(),
            &MATCHES.iter().map(|(value, _)| *value).collect::<Vec<_>>(),
            &cfg.match_models,
        );
        let match_idx = matches.iter().position(|(value, _)| *value == cfg.match_models).unwrap_or(0);
        let match_titles: Vec<&str> = matches.iter().map(|(_, title)| title.as_str()).collect();
        let match_popup = kit::popup(mtm, &behavior, 0.0, POPUP_W, &match_titles, match_idx, "Каких субагентов подменять", controller, action);
        kit::row_label(mtm, &behavior, ROW, "OpenCode молчит — откат в Claude", 280.0).setAccessibilityElement(false);
        let fallback_switch = kit::switch(mtm, &behavior, ROW, cfg.fallback, "OpenCode не отвечает — откат на настоящую модель Claude", controller, action);
        // Видно прямо в терминале: «модель · 3 аг · 12 отв» внизу Claude Code — своя строка остаётся.
        kit::row_label(mtm, &behavior, 2.0 * ROW, "Строка в Claude Code", 220.0).setAccessibilityElement(false);
        let statusline_switch = kit::switch(mtm, &behavior, 2.0 * ROW, statusline, "Строка «модель субагентов · N отв» внизу Claude Code", controller, action);
        statusline_switch.setToolTip(Some(&NSString::from_str("Внизу Claude Code: куда идут субагенты этой сессии — «deepseek-v4.1-flash · 3 аг · 12 отв»; ушли в Claude — «· N в Claude». Своя строка состояния остаётся.")));
        kit::row_label(mtm, &behavior, 3.0 * ROW, "Прокси как служба", 200.0).setAccessibilityElement(false);
        let service_switch = kit::switch(mtm, &behavior, 3.0 * ROW, service, "Прокси как служба: автозапуск и перезапуск при сбое", controller, action);
        let restart_x = kit::switch_left(&service_switch) - 30.0;
        kit::symbol_button(mtm, &behavior, restart_x, 3.0 * ROW, "arrow.clockwise", "Перезапустить прокси мягко: начатые запросы доделает", controller, sel!(onProxyRestart:));
        // Что-то не так — журнал под рукой, а не путь в тексте ошибки.
        kit::symbol_button(mtm, &behavior, restart_x - 26.0, 3.0 * ROW, "doc.text.magnifyingglass", "Открыть журнал прокси", controller, sel!(onProxyLog:));
        y += BEHAVIOR_ROWS * ROW;

        // ── сообщение и кнопки
        let message_label = kit::text(mtm, "", 15.0, y + 8.0, PANEL_WIDTH - 30.0, 10.5, FontWeight::Regular, &p.muted);
        root.addSubview(&message_label);
        // Команда запуска — ссылкой, а не второй большой кнопкой: нужна раз, а главная тут — «Готово».
        let copy: Retained<NSButton> = kit::footer_button(mtm, &root, height, kit::MARGIN, 120.0, "claude-sub", controller, sel!(onProxyCopy:));
        copy.setBordered(false);
        if let Some(icon) = objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("doc.on.doc"), Some(&NSString::from_str("Скопировать команду claude-sub"))) {
            copy.setImage(Some(&icon));
            copy.setImagePosition(objc2_app_kit::NSCellImagePosition::ImageLeading);
        }
        copy.setFont(Some(&draw::mono_font(12.0, FontWeight::Regular)));
        copy.setContentTintColor(Some(&p.muted));
        copy.setAlignment(objc2_app_kit::NSTextAlignment::Left);
        copy.setAccessibilityLabel(Some(&NSString::from_str("Скопировать команду claude-sub")));
        copy.setToolTip(Some(&NSString::from_str("Скопировать команду: запускай claude-sub вместо claude — основная модель на подписке, субагенты в OpenCode")));
        let done = kit::footer_button(mtm, &root, height, PANEL_WIDTH - kit::MARGIN - 90.0, 90.0, "Готово", controller, sel!(onProxyClose:));
        done.setKeyEquivalent(&NSString::from_str("\r"));

        ProxyView {
            view: Retained::into_super(root),
            keys_view,
            model_popup,
            effort_popup,
            match_popup,
            enabled_switch,
            rotate_switch,
            fallback_switch,
            service_switch,
            statusline_switch,
            dot,
            status_label,
            meta_label,
            check_note,
            message_label,
            last_message: Default::default(),
            show_remaining,
            models: models.into_iter().map(|(value, _)| value).collect(),
            efforts: efforts.into_iter().map(|(value, _)| value).collect(),
            matches: matches.into_iter().map(|(value, _)| value).collect(),
        }
    }

    /// Текущие значения формы поверх прежнего конфига (порт, адреса и т.п. не трогаем).
    /// Несохранённое на экране: настройки поверх `cfg` и тумблеры службы и строки состояния.
    /// Пересборка экрана (другое число ключей) переносит их, а не откатывает к файлу.
    pub fn draft(&self, cfg: ProxyConfig) -> (ProxyConfig, bool, bool) {
        (self.apply_to(cfg), kit::is_on(&self.service_switch), kit::is_on(&self.statusline_switch))
    }

    pub fn apply_to(&self, mut cfg: ProxyConfig) -> ProxyConfig {
        if let Some((label, key)) = self.keys_view.selected() {
            cfg.api_key = key;
            cfg.account_label = label;
        }
        // Значения берём из списков, а не из констант: там может быть чужое из proxy.json.
        cfg.model = self
            .models
            .get(self.model_popup.indexOfSelectedItem().max(0) as usize)
            .cloned()
            .unwrap_or_else(|| MODELS[0].to_string());
        cfg.effort = self
            .efforts
            .get(self.effort_popup.indexOfSelectedItem().max(0) as usize)
            .cloned()
            .unwrap_or_default();
        cfg.match_models = self.matches.get(self.match_popup.indexOfSelectedItem().max(0) as usize).cloned().unwrap_or_else(|| "haiku".into());
        cfg.enabled = kit::is_on(&self.enabled_switch);
        cfg.rotate = kit::is_on(&self.rotate_switch);
        cfg.fallback = kit::is_on(&self.fallback_switch);
        cfg
    }

    pub fn sync_service(&self, on: bool) {
        self.service_switch.setState(if on { objc2_app_kit::NSControlStateValueOn } else { 0 });
    }

    pub fn sync_statusline(&self, on: bool) {
        self.statusline_switch.setState(if on { objc2_app_kit::NSControlStateValueOn } else { 0 });
    }

    pub fn service_wanted(&self) -> bool {
        kit::is_on(&self.service_switch)
    }

    pub fn statusline_wanted(&self) -> bool {
        kit::is_on(&self.statusline_switch)
    }

    /// Статус работающего прокси: точка и строка состояния, счётчики с периодом.
    pub fn set_status(&self, status: Option<&serde_json::Value>, cfg: &ProxyConfig) {
        use crate::proxy::control;
        let p = Palette::current();
        let now = crate::store::now_ms();
        let (tone, line, stats) = control::status_lines(status, cfg, now);
        let dot_color = match tone {
            Tone::Ok => p.good(),
            Tone::Warn => p.warn(),
            Tone::Off => p.faint.clone(),
        };
        self.dot.set_color(&dot_color);
        // Ключ не выбран — так и сказать: выбрать можно строкой ниже.
        // Перезапуск и старая версия важнее: про них иначе не узнать.
        // Про старую версию статус может сказать и в хвосте строки («… · прокси старой версии X»).
        let lower = line.to_lowercase();
        let urgent = lower.contains("прокси перезапускается") || lower.contains("прокси старой");
        let line = if !urgent && status.is_some() && cfg.enabled && cfg.api_key.trim().is_empty() {
            "Выбери основной ключ ниже — пока его нет, субагенты идут в Claude".to_string()
        } else {
            line
        };
        // Жёлтым — только тревожное, остальное приглушено: «ушёл в Claude» должно бросаться в глаза.
        let line_color = match tone {
            Tone::Warn => p.warn(),
            Tone::Ok | Tone::Off => p.muted.clone(),
        };
        kit::set_text(&self.status_label, &line, &line_color);
        let meta = control::status_meta(status, now);
        kit::set_text(&self.meta_label, &meta, &p.muted);
        let tip = match control::last_failure(status, now) {
            Some(why) => format!("{meta}\n{stats}\nПоследний сбой: {why}"),
            None => format!("{meta}\n{stats}"),
        };
        self.meta_label.setToolTip(Some(&NSString::from_str(&tip)));
    }

    /// В каком режиме нарисован заголовок ключей.
    pub fn show_remaining(&self) -> bool {
        self.show_remaining
    }

    /// Строки ключей заново: лимиты обновились или прокси сменил статус ключей.
    pub fn set_keys(&self, rows: Vec<KeyRow>) {
        self.keys_view.set_rows(rows);
    }

    /// Сколько строк ключей на экране: другое число — другая высота, экран надо пересобрать.
    pub fn key_count(&self) -> usize {
        self.keys_view.ivars().rows.borrow().len()
    }

    pub fn set_check(&self, text: &str, ok: Option<bool>) {
        let p = Palette::current();
        // «⚠ … но подмена не сработает» — связь есть, это предупреждение, а не отказ.
        let color = match ok {
            _ if text.starts_with(crate::proxy::control::WARN_MARK) => p.warn(),
            Some(true) => p.good(),
            Some(false) => p.bad(),
            None => p.muted.clone(),
        };
        kit::set_text(&self.check_note, text, &color);
    }

    pub fn set_message(&self, text: &str, error: bool) {
        let p = Palette::current();
        let color = if error { p.bad() } else { p.muted.clone() };
        kit::set_text(&self.message_label, text, &color);
        // Как в форме: подпись для VoiceOver, ошибку ещё и объявляем — иначе провал действия не слышен.
        let label = NSString::from_str(text);
        self.message_label.setAccessibilityLabel(if text.is_empty() { None } else { Some(&label) });
        if error && !text.is_empty() {
            super::form::announce(&self.message_label, &label);
        }
        *self.last_message.borrow_mut() = Some((text.to_string(), error));
    }

    /// Последнее сообщение — пережить пересборку экрана (другое число ключей).
    pub fn message(&self) -> Option<(String, bool)> {
        self.last_message.borrow().clone()
    }
}

// Раскладка должна влезать в поповер: проверяется тестом, а не глазами.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn экран_влезает_и_кнопки_не_наезжают_на_группы() {
        // Большой экран: иначе keys_visible() == 2 и продовая раскладка на 7 строк не проверяется.
        crate::ui::list::set_screen_height(2000.0);
        // Высота — общая на процесс: вернуть обычную и при падении, иначе соседние тесты увидят чужую.
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::ui::list::set_screen_height(0.0);
            }
        }
        let _restore = Restore;
        assert_eq!(keys_visible(), KEYS_VISIBLE, "на большом экране — полный список ключей");
        for keys in 0..=30 {
            let height = screen_height(keys);
            let message_bottom = height - FOOTER + 8.0 + 16.0;
            let buttons_top = height - kit::FOOTER_BOTTOM - kit::BUTTON_H;
            assert!(message_bottom <= buttons_top, "{keys} ключей: сообщение {message_bottom} наезжает на кнопки {buttons_top}");
            assert!(height <= 760.0, "{keys} ключей: экран {height} выше окна");
        }
        assert!(screen_height(5) > screen_height(1), "каждый ключ — строка");
        assert_eq!(keys_card_height(0), keys_card_height(1), "нет ключей — одна строка-подсказка");
        // Дальше KEYS_VISIBLE список не растёт — иначе «Готово» уедет за край поповера.
        assert_eq!(screen_height(KEYS_VISIBLE), screen_height(30));
    }

    #[test]
    fn чужое_значение_из_proxy_json_не_превращается_в_первый_пункт() {
        let models = options_with_current(&MODEL_TITLES, &MODELS, "muse-spark-1.3-contributor");
        assert!(models.iter().any(|(value, _)| value == "muse-spark-1.3-contributor"));
        assert_eq!(models.len(), MODELS.len(), "известное значение не дублируем");

        let custom = options_with_current(&MODEL_TITLES, &MODELS, "some-fresh-model");
        assert_eq!(custom.last().map(|(value, _)| value.as_str()), Some("some-fresh-model"));
        assert_eq!(custom.last().map(|(_, title)| title.as_str()), Some("своё: some-fresh-model"));

        let efforts = options_with_current(&EFFORT_TITLES, &EFFORTS, "ultra");
        assert_eq!(efforts.len(), EFFORTS.len() + 1);
        assert_eq!(efforts.last().map(|(value, _)| value.as_str()), Some("ultra"));
        // Пустое «как просит Claude» — тоже значение, не отсутствие.
        let default = options_with_current(&EFFORT_TITLES, &EFFORTS, "");
        assert_eq!(default.len(), EFFORTS.len());
    }
}
