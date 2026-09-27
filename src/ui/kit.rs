//! Общие детали экранов с нативными контролами — в том же стиле, что список аккаунтов:
//! фон `#F5F7F9`, белые карточки-группы со скруглением, строки «подпись слева — контрол справа»,
//! шапка как у списка. Раскладка сверху вниз (перевёрнутые виды).

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, DefinedClass, Message};
use objc2_app_kit::{
    NSAccessibility, NSBezierPath, NSButton, NSColor, NSControlSize, NSControlStateValueOff, NSControlStateValueOn, NSFont, NSLineBreakMode,
    NSPopUpButton, NSSwitch, NSTextField, NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use super::draw::{self, FontWeight};
use super::list::PANEL_WIDTH;
use super::Palette;

/// Карточка от края окна (как карточки аккаунтов).
pub const MARGIN: f64 = 10.0;
pub const CARD_W: f64 = PANEL_WIDTH - 2.0 * MARGIN;
/// Содержимое внутри карточки.
pub const INSET: f64 = 13.0;
/// Высота строки группы.
pub const ROW: f64 = 32.0;
/// Та же высота, что у шапки списка: линия под шапкой и отсечка прокрутки не должны разъехаться.
pub const HEADER_H: f64 = super::list::HEADER_HEIGHT;
/// Заголовок группы над карточкой: высота вместе с отступом до карточки.
pub const SECTION_H: f64 = 16.0;
/// Между карточками.
pub const GAP: f64 = 12.0;
/// Ряд кнопок внизу: от нижнего края и высота.
pub const FOOTER_BOTTOM: f64 = 14.0;
pub const BUTTON_H: f64 = 28.0;

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

// ─────────── виды ───────────

define_class!(
    /// Контейнер с началом координат сверху слева: экраны раскладываются сверху вниз.
    #[unsafe(super(NSView))]
    #[name = "SubBarFlippedView"]
    pub struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl FlippedView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        unsafe { msg_send![mtm.alloc::<Self>(), initWithFrame: frame] }
    }
}

#[derive(Default)]
pub struct CardIvars {
    /// Разделители строк: y от верха карточки.
    lines: RefCell<Vec<f64>>,
}

define_class!(
    /// Белая карточка-группа: скругление, тонкая рамка, волосяные разделители строк.
    #[unsafe(super(NSView))]
    #[name = "SubBarCardView"]
    #[ivars = CardIvars]
    pub struct CardView;

    impl CardView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let p = Palette::current();
            let size = self.bounds().size;
            draw::fill_round_rect(0.0, 0.0, size.width, size.height, 10.0, &p.card);
            draw::stroke_round_rect(0.0, 0.0, size.width, size.height, 10.0, &p.border, 0.7);
            p.hairline.setFill();
            for y in self.ivars().lines.borrow().iter() {
                NSBezierPath::fillRect(rect(INSET, y - 0.25, size.width - 2.0 * INSET, 0.5));
            }
        }
    }
);

impl CardView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, lines: Vec<f64>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(CardIvars { lines: RefCell::new(lines) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct DotIvars {
    color: RefCell<Retained<NSColor>>,
}

define_class!(
    /// Точка состояния (как у строки статуса в шапке списка).
    #[unsafe(super(NSView))]
    #[name = "SubBarDotView"]
    #[ivars = DotIvars]
    pub struct DotView;

    impl DotView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let size = self.bounds().size;
            draw::fill_round_rect(0.0, 0.0, size.width, size.height, size.width / 2.0, &self.ivars().color.borrow());
        }
    }
);

impl DotView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, color: &NSColor) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(DotIvars { color: RefCell::new(color.retain()) });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // Цвет точки повторяет текст статуса рядом — VoiceOver незачем спотыкаться о безымянный элемент.
        this.setAccessibilityElement(false);
        this
    }

    pub fn set_color(&self, color: &NSColor) {
        *self.ivars().color.borrow_mut() = color.retain();
        self.setNeedsDisplay(true);
    }
}

// ─────────── контролы и подписи ───────────

/// Однострочная подпись: текст начинается ровно в x (у NSTextField свои 2 пт слева), лишнее — «…».
pub fn text(mtm: MainThreadMarker, s: &str, x: f64, y: f64, w: f64, size: f64, weight: FontWeight, color: &NSColor) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(s), mtm);
    field.setFont(Some(&draw::font(size, weight)));
    field.setTextColor(Some(color));
    field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    field.setUsesSingleLineMode(true);
    field.setFrame(rect(x - 2.0, y, w + 4.0, (size * 1.3).ceil() + 2.0));
    // Обрезанную «…» подпись целиком видно наведением.
    if !s.is_empty() {
        field.setToolTip(Some(&NSString::from_str(s)));
    }
    field
}

/// Шапка экрана — как у списка: заголовок, мета рядом, строка статуса с точкой, линия снизу.
pub struct Header {
    pub meta: Retained<NSTextField>,
    pub status: Retained<NSTextField>,
    pub dot: Option<Retained<DotView>>,
}

pub fn header(mtm: MainThreadMarker, root: &NSView, title: &str, meta: &str, status: &str, with_dot: bool, right_reserve: f64) -> Header {
    let p = Palette::current();
    let title_font = draw::font(16.0, FontWeight::Semibold);
    root.addSubview(&text(mtm, title, 15.0, 10.0, 200.0, 16.0, FontWeight::Semibold, &p.text));
    // Поле заголовка — 200 пт и само режет «…»: мета не должна уезжать за него.
    let meta_x = 15.0 + draw::measure(title, &title_font).width.min(200.0) + 9.0;
    let meta_label = text(mtm, meta, meta_x, 16.5, (PANEL_WIDTH - meta_x - 15.0 - right_reserve).max(0.0), 10.0, FontWeight::Regular, &p.muted);
    root.addSubview(&meta_label);
    let (dot, status_x) = if with_dot {
        let dot = DotView::new(mtm, rect(15.0, 50.0, 6.0, 6.0), &p.faint);
        root.addSubview(&dot);
        (Some(dot), 27.0)
    } else {
        (None, 15.0)
    };
    let status_label = text(mtm, status, status_x, 44.0, // Резерв справа — под переключатель в строке заголовка; строка статуса ниже него и пользуется всей шириной.
        PANEL_WIDTH - status_x - 15.0, 10.5, FontWeight::Regular, &p.muted);
    root.addSubview(&status_label);
    root.addSubview(&hairline(mtm, HEADER_H - 0.5));
    Header { meta: meta_label, status: status_label, dot }
}

/// Волосяная линия во всю ширину (под шапкой, как у списка).
fn hairline(mtm: MainThreadMarker, y: f64) -> Retained<NSView> {
    Retained::into_super(LineView::new(mtm, rect(0.0, y, PANEL_WIDTH, 0.5)))
}

define_class!(
    #[unsafe(super(NSView))]
    #[name = "SubBarLineView"]
    struct LineView;

    impl LineView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            Palette::current().border.setFill();
            NSBezierPath::fillRect(self.bounds());
        }
    }
);

impl LineView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        unsafe { msg_send![mtm.alloc::<Self>(), initWithFrame: frame] }
    }
}

/// Заголовок группы над карточкой — на уровне текста внутри карточки.
pub fn section(mtm: MainThreadMarker, root: &NSView, y: f64, title: &str) {
    let p = Palette::current();
    root.addSubview(&text(mtm, title, MARGIN + INSET, y, CARD_W - 2.0 * INSET, 9.5, FontWeight::Semibold, &p.muted));
}

/// Карточка на экране: y — верх, высота, разделители (y от верха карточки).
pub fn card(mtm: MainThreadMarker, root: &NSView, y: f64, height: f64, lines: Vec<f64>) -> Retained<CardView> {
    let c = CardView::new(mtm, rect(MARGIN, y, CARD_W, height), lines);
    root.addSubview(&c);
    c
}

/// Подпись строки карточки.
pub fn row_label(mtm: MainThreadMarker, card: &NSView, row_y: f64, s: &str, width: f64) -> Retained<NSTextField> {
    let p = Palette::current();
    let l = text(mtm, s, INSET, row_y + (ROW - 17.0) / 2.0, width, 12.5, FontWeight::Regular, &p.text);
    card.addSubview(&l);
    l
}

/// Мелкая подпись в карточке (подсказка под строкой, итог проверки).
pub fn note(mtm: MainThreadMarker, card: &NSView, y: f64, x: f64, width: f64, s: &str) -> Retained<NSTextField> {
    let p = Palette::current();
    // Подсказка режется «…» по ширине карточки — целиком её видно наведением (тултип ставит text).
    let l = text(mtm, s, x, y, width, 10.0, FontWeight::Regular, &p.faint);
    card.addSubview(&l);
    l
}

fn bind(control: &objc2_app_kit::NSControl, target: &AnyObject, action: Sel) {
    unsafe {
        control.setTarget(Some(target));
        control.setAction(Some(action));
    }
}

/// Компактный выпадающий список справа в строке карточки.
pub fn popup(mtm: MainThreadMarker, card: &NSView, row_y: f64, width: f64, titles: &[&str], selected: usize, a11y: &str, target: &AnyObject, action: Sel) -> Retained<NSPopUpButton> {
    let p = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc::<NSPopUpButton>(), rect(0.0, 0.0, width, 22.0), false);
    p.setControlSize(NSControlSize::Small);
    p.setFont(Some(&NSFont::systemFontOfSize(11.5)));
    for t in titles {
        p.addItemWithTitle(&NSString::from_str(t));
    }
    // Пустой список — выбирать нечего: и открывать пустое меню незачем.
    match titles.len().checked_sub(1) {
        Some(last) => p.selectItemAtIndex(selected.min(last) as isize),
        None => p.setEnabled(false),
    }
    p.setAccessibilityLabel(Some(&NSString::from_str(a11y)));
    bind(&p, target, action);
    let h = p.fittingSize().height.max(20.0);
    // Видимый край списка совпадает с краем рамки: правый край — ровно на отступе карточки.
    p.setFrame(rect(CARD_W - INSET - width, row_y + (ROW - h) / 2.0, width, h));
    card.addSubview(&p);
    p
}

/// Видимая ширина маленького переключателя: рамка NSSwitch шире (54 пт), сам он — по центру.
const SWITCH_VISIBLE_W: f64 = 36.0;

/// Маленький переключатель, видимый правый край которого — ровно в `right`.
pub fn switch_at(mtm: MainThreadMarker, parent: &NSView, right: f64, center_y: f64, on: bool, a11y: &str, target: &AnyObject, action: Sel) -> Retained<NSSwitch> {
    let s = NSSwitch::initWithFrame(mtm.alloc::<NSSwitch>(), rect(0.0, 0.0, 54.0, 24.0));
    s.setControlSize(NSControlSize::Mini);
    s.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
    s.setAccessibilityLabel(Some(&NSString::from_str(a11y)));
    bind(&s, target, action);
    let size = s.fittingSize();
    let x = right - (size.width + SWITCH_VISIBLE_W) / 2.0;
    s.setFrame(rect(x, center_y - size.height / 2.0, size.width, size.height));
    parent.addSubview(&s);
    s
}

/// Переключатель справа в строке карточки (как в Системных настройках).
pub fn switch(mtm: MainThreadMarker, card: &NSView, row_y: f64, on: bool, a11y: &str, target: &AnyObject, action: Sel) -> Retained<NSSwitch> {
    switch_at(mtm, card, CARD_W - INSET, row_y + ROW / 2.0, on, a11y, target, action)
}

/// Левый край видимой части переключателя — чтобы ставить что-то рядом.
pub fn switch_left(s: &NSSwitch) -> f64 {
    let f = s.frame();
    f.origin.x + (f.size.width - SWITCH_VISIBLE_W) / 2.0
}

pub fn is_on(s: &NSSwitch) -> bool {
    s.state() == NSControlStateValueOn
}

/// Нативная кнопка в строке карточки (справа) — компактная.
pub fn row_button(mtm: MainThreadMarker, card: &NSView, row_y: f64, title: &str, width: f64, target: &AnyObject, action: Sel) -> Retained<NSButton> {
    let b = NSButton::initWithFrame(mtm.alloc::<NSButton>(), rect(0.0, 0.0, width, 22.0));
    b.setTitle(&NSString::from_str(title));
    b.setBezelStyle(objc2_app_kit::NSBezelStyle::Push);
    b.setControlSize(NSControlSize::Small);
    b.setFont(Some(&NSFont::systemFontOfSize(11.5)));
    bind(&b, target, action);
    let h = b.fittingSize().height.max(20.0);
    b.setFrame(rect(CARD_W - INSET - width, row_y + (ROW - h) / 2.0, width, h));
    card.addSubview(&b);
    b
}

/// Значок-кнопка без рамки (SF Symbol) — для второстепенных действий в строке.
pub fn symbol_button(mtm: MainThreadMarker, card: &NSView, x: f64, row_y: f64, symbol: &str, tip: &str, target: &AnyObject, action: Sel) -> Retained<NSButton> {
    let b = NSButton::initWithFrame(mtm.alloc::<NSButton>(), rect(x, row_y + (ROW - 22.0) / 2.0, 24.0, 22.0));
    b.setBordered(false);
    b.setTitle(&NSString::from_str(""));
    // Имя кнопки для VoiceOver — до двоеточия, пояснение после него — справкой.
    let (name, help) = tip.split_once(':').map_or((tip, ""), |(n, h)| (n.trim(), h.trim()));
    match objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(symbol), Some(&NSString::from_str(name))) {
        Some(image) => b.setImage(Some(&image)),
        // Символа нет в этой macOS — пустая кликабельная кнопка была бы невидимой ловушкой.
        None => b.setTitle(&NSString::from_str("•")),
    }
    b.setContentTintColor(Some(&Palette::current().muted));
    b.setToolTip(Some(&NSString::from_str(tip)));
    b.setAccessibilityLabel(Some(&NSString::from_str(name)));
    if !help.is_empty() {
        b.setAccessibilityHelp(Some(&NSString::from_str(help)));
    }
    bind(&b, target, action);
    card.addSubview(&b);
    b
}

/// Кнопка нижнего ряда (нативная). `x` — левый край; `height` экрана нужен, чтобы встать у низа.
pub fn footer_button(mtm: MainThreadMarker, root: &NSView, screen_h: f64, x: f64, width: f64, title: &str, target: &AnyObject, action: Sel) -> Retained<NSButton> {
    let b = NSButton::initWithFrame(mtm.alloc::<NSButton>(), rect(x, screen_h - FOOTER_BOTTOM - BUTTON_H, width, BUTTON_H));
    b.setTitle(&NSString::from_str(title));
    b.setBezelStyle(objc2_app_kit::NSBezelStyle::Push);
    bind(&b, target, action);
    root.addSubview(&b);
    b
}

/// Атрибуты текста кнопки: цвет (например, красный у «Удалить») и обычный шрифт кнопки.
pub fn text_attributes(color: &NSColor) -> Retained<objc2_foundation::NSDictionary<objc2_foundation::NSAttributedStringKey, AnyObject>> {
    let font: Retained<AnyObject> = unsafe { Retained::cast_unchecked(NSFont::systemFontOfSize(13.0)) };
    let color: Retained<AnyObject> = unsafe { Retained::cast_unchecked(color.retain()) };
    let keys = unsafe { [objc2_app_kit::NSFontAttributeName, objc2_app_kit::NSForegroundColorAttributeName] };
    objc2_foundation::NSDictionary::from_retained_objects(&keys, &[font, color])
}

/// Текст подписи (полный — во всплывающей подсказке: вдруг не влез).
pub fn set_text(field: &NSTextField, s: &str, color: &NSColor) {
    let value = NSString::from_str(s);
    field.setStringValue(&value);
    field.setToolTip(if s.is_empty() { None } else { Some(&*value) });
    field.setTextColor(Some(color));
}
