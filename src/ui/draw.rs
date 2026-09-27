use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::Message;
use objc2_app_kit::{
    NSBezierPath, NSColor, NSFont, NSFontAttributeName, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSStringDrawing,
};
use objc2_foundation::{NSAttributedStringKey, NSDictionary, NSPoint, NSRect, NSSize, NSString};

pub fn srgb(r: f64, g: f64, b: f64, a: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a)
}

pub fn font(size: f64, weight: FontWeight) -> Retained<NSFont> {
    let value = unsafe {
        match weight {
            FontWeight::Regular => NSFontWeightRegular,
            FontWeight::Semibold => NSFontWeightSemibold,
        }
    };
    NSFont::systemFontOfSize_weight(size, value)
}

pub fn mono_font(size: f64, weight: FontWeight) -> Retained<NSFont> {
    let value = unsafe {
        match weight {
            FontWeight::Regular => NSFontWeightRegular,
            FontWeight::Semibold => NSFontWeightSemibold,
        }
    };
    NSFont::monospacedDigitSystemFontOfSize_weight(size, value)
}

#[derive(Clone, Copy)]
pub enum FontWeight {
    Regular,
    Semibold,
}

pub fn round_rect(x: f64, y: f64, width: f64, height: f64, radius: f64) -> Retained<NSBezierPath> {
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height)),
        radius,
        radius,
    )
}

pub fn fill_round_rect(x: f64, y: f64, width: f64, height: f64, radius: f64, color: &NSColor) {
    color.setFill();
    round_rect(x, y, width, height, radius).fill();
}

pub fn stroke_round_rect(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
    color: &NSColor,
    width_px: f64,
) {
    color.setStroke();
    let path = round_rect(
        x + width_px / 2.0,
        y + width_px / 2.0,
        (width - width_px).max(0.0),
        (height - width_px).max(0.0),
        radius,
    );
    path.setLineWidth(width_px);
    path.stroke();
}

fn attributes(
    font: &NSFont,
    color: &NSColor,
) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    let font_object: Retained<AnyObject> = unsafe { Retained::cast_unchecked(font.retain()) };
    let color_object: Retained<AnyObject> = unsafe { Retained::cast_unchecked(color.retain()) };
    let keys: [&NSAttributedStringKey; 2] =
        unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values: [Retained<AnyObject>; 2] = [font_object, color_object];
    NSDictionary::from_retained_objects(&keys, &values)
}

pub struct TextMetrics {
    pub width: f64,
    pub height: f64,
}

pub fn measure(text: &str, font: &NSFont) -> TextMetrics {
    let string = NSString::from_str(text);
    let size: NSSize =
        unsafe { string.sizeWithAttributes(Some(&attributes(font, &NSColor::blackColor()))) };
    TextMetrics {
        width: size.width,
        height: size.height,
    }
}

/// Текст с левым верхним углом в (x, y).
/// Высоту здесь не меряем: она нужна только `draw_text_center_at`, а замер — полная раскладка текста на каждой строке.
pub fn draw_text(text: &str, x: f64, y: f64, font: &NSFont, color: &NSColor) {
    if text.is_empty() {
        return;
    }
    let string = NSString::from_str(text);
    let attrs = attributes(font, color);
    // Вид перевёрнутый: (x, y) — левый верхний угол строки, смещать не нужно.
    let point = NSPoint::new(x, y);
    unsafe {
        string.drawAtPoint_withAttributes(point, Some(&attrs));
    }
}

/// Текст по правому краю: правый край ложится на `right`.
pub fn draw_text_right(text: &str, right: f64, y: f64, font: &NSFont, color: &NSColor) {
    let metrics = measure(text, font);
    draw_text(text, right - metrics.width, y, font, color)
}

/// Текст по центру относительно `center_x`.
pub fn draw_text_centered(
    text: &str,
    center_x: f64,
    y: f64,
    font: &NSFont,
    color: &NSColor,
) {
    let metrics = measure(text, font);
    draw_text(text, center_x - metrics.width / 2.0, y, font, color)
}

/// Текст по центру точки (cx, cy) — по обеим осям; годится и для неперевёрнутой картинки
/// (значок строки меню), и для перевёрнутого вида.
pub fn draw_text_center_at(text: &str, cx: f64, cy: f64, font: &NSFont, color: &NSColor) {
    let metrics = measure(text, font);
    draw_text(text, cx - metrics.width / 2.0, cy - metrics.height / 2.0, font, color);
}

/// SF Symbol в цвете, по центру (cx, cy) — иконки нарисованных кнопок. Картинки кэшируются по имени,
/// размеру и `tag` цвета: рисование идёт на каждое движение мыши.
pub fn draw_symbol(name: &str, cx: f64, cy: f64, size: f64, color: &NSColor, tag: &str) {
    use objc2_app_kit::{NSImage, NSImageSymbolConfiguration};
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, Option<Retained<NSImage>>>> = Default::default();
    }
    let key = format!("{name}|{size}|{tag}");
    let image = CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| {
                let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?;
                let weight = unsafe { objc2_app_kit::NSFontWeightMedium };
                let colors = objc2_foundation::NSArray::from_retained_slice(&[color.retain()]);
                let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(size, weight)
                    .configurationByApplyingConfiguration(&NSImageSymbolConfiguration::configurationWithPaletteColors(&colors));
                base.imageWithSymbolConfiguration(&config)
            })
            .clone()
    });
    if let Some(image) = image {
        let s = image.size();
        image.drawInRect(NSRect::new(NSPoint::new((cx - s.width / 2.0).round(), (cy - s.height / 2.0).round()), s));
    }
}

/// Обрезать текст с многоточием, чтобы влез в `max_width`.
/// Двоичный поиск вместо линейного — O(log n) замеров, а не O(n).
pub fn ellipsize(text: &str, font: &NSFont, max_width: f64) -> String {
    if max_width <= 4.0 {
        return String::new(); // бюджета нет — лучше ничего, чем текст поверх соседней колонки
    }
    if measure(text, font).width <= max_width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    // Двоичный поиск самого длинного начала, что влезает вместе с «…».
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let candidate: String = chars.iter().take(mid).collect::<String>() + "…";
        if measure(&candidate, font).width <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        // Даже «…» не влезает — пусто, а не текст поверх соседней колонки.
        if measure("…", font).width > max_width { String::new() } else { "…".to_string() }
    } else {
        chars.iter().take(lo).collect::<String>() + "…"
    }
}
