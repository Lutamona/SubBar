pub mod draw;
pub mod form;
pub mod kit;
pub mod list;
pub mod proxy;
pub mod settings;

/// Палитра окна. Тема одна — светлая (решение 26.09): окно светлое при любой системной теме.
#[derive(Clone)]
pub struct Palette {
    pub background: objc2::rc::Retained<objc2_app_kit::NSColor>,
    pub card: objc2::rc::Retained<objc2_app_kit::NSColor>,
    pub card_hover: objc2::rc::Retained<objc2_app_kit::NSColor>,
    pub border: objc2::rc::Retained<objc2_app_kit::NSColor>,
    /// Разделители строк внутри карточек — тише рамки.
    pub hairline: objc2::rc::Retained<objc2_app_kit::NSColor>,
    pub track: objc2::rc::Retained<objc2_app_kit::NSColor>,
    pub text: objc2::rc::Retained<objc2_app_kit::NSColor>,
    pub muted: objc2::rc::Retained<objc2_app_kit::NSColor>,
    pub faint: objc2::rc::Retained<objc2_app_kit::NSColor>,
    pub accent: objc2::rc::Retained<objc2_app_kit::NSColor>,
    good: objc2::rc::Retained<objc2_app_kit::NSColor>,
    warn: objc2::rc::Retained<objc2_app_kit::NSColor>,
    bad: objc2::rc::Retained<objc2_app_kit::NSColor>,
}

impl Palette {
    /// Палитра строится один раз на поток (UI — главный поток); вызов отдаёт её копию (retain цветов).
    pub fn current() -> Self {
        thread_local! {
            static CACHED: Palette = Palette::build();
        }
        CACHED.with(Palette::clone)
    }

    fn build() -> Self {
        let rgb = |r: u8, g: u8, b: u8| draw::srgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, 1.0);
        Palette {
            background: rgb(245, 247, 249),
            card: rgb(255, 255, 255),
            card_hover: rgb(239, 245, 251),
            border: rgb(218, 225, 232),
            hairline: rgb(232, 237, 242),
            track: rgb(222, 230, 237),
            text: rgb(27, 38, 50),
            muted: rgb(83, 99, 115),
            faint: rgb(92, 107, 122), // ≥4.5:1 и на фоне #F5F7F9, не только на белом
            accent: rgb(30, 104, 185),
            // ≥4.5:1 на фоне окна: прежний srgb(0.09,0.51,0.36) давал там 4,46:1.
            good: draw::srgb(0.08, 0.48, 0.34, 1.0),
            // Темнее прежнего #AD6312: тот на фоне окна давал 4,25:1 — ниже WCAG AA.
            warn: draw::srgb(0.60, 0.34, 0.05, 1.0),
            bad: draw::srgb(0.76, 0.23, 0.30, 1.0),
        }
    }

    /// Цвет полосы и процента по остатку.
    pub fn band(&self, used: f64) -> objc2::rc::Retained<objc2_app_kit::NSColor> {
        match crate::util::band(used) {
            crate::util::Band::Calm => self.good(),
            crate::util::Band::Warn => self.warn(),
            crate::util::Band::Danger => self.bad(),
        }
    }

    /// Всё хорошо: работает, отвечает.
    pub fn good(&self) -> objc2::rc::Retained<objc2_app_kit::NSColor> {
        self.good.clone()
    }

    /// Внимание: пауза ключа, подмена выключена, старая версия.
    pub fn warn(&self) -> objc2::rc::Retained<objc2_app_kit::NSColor> {
        self.warn.clone()
    }

    /// Плохо: ошибка, лимит на исходе.
    pub fn bad(&self) -> objc2::rc::Retained<objc2_app_kit::NSColor> {
        self.bad.clone()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        // Правая и нижняя грани не входят: точка на общей грани соседей принадлежит одному.
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}
