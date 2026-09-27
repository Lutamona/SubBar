use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, sel, DefinedClass};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSBezierPath,
    NSCellImagePosition, NSColor, NSEvent, NSEventModifierFlags, NSHapticFeedbackManager, NSHapticFeedbackPattern,
    NSHapticFeedbackPerformer, NSImage, NSMenu, NSMenuItem, NSPasteboard, NSPasteboardTypeString,
    NSPopover, NSPopoverBehavior, NSPopoverWillCloseNotification, NSStatusBar,
    NSStatusBarButton, NSVariableStatusItemLength, NSView, NSViewController,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSNotificationCenter, NSObject, NSPoint, NSRect, NSSize,
    NSString,
};

use crate::model::{Account, ProviderId};
use crate::state::{self, FormState, Screen, APP};
use crate::ui::form::FormView;
use crate::ui::list::{self, Action, HOVER_ROW};
use crate::ui::proxy::ProxyView;
use crate::ui::settings::SettingsView;
use crate::ui::{self, Palette};
use crate::util;

pub const PANEL_WIDTH: f64 = list::PANEL_WIDTH;
pub const PANEL_HEIGHT: f64 = list::MAX_PANEL_HEIGHT;

thread_local! {
    static CONTROLLER: RefCell<Option<Retained<UiController>>> = const { RefCell::new(None) };
    static PANEL: RefCell<Option<Retained<PanelView>>> = const { RefCell::new(None) };
    static STATUS_BUTTON: RefCell<Option<Retained<NSStatusBarButton>>> = const { RefCell::new(None) };
    static POPOVER: RefCell<Option<Retained<NSPopover>>> = const { RefCell::new(None) };
    static FORM_VIEW: RefCell<Option<FormView>> = const { RefCell::new(None) };
    static SETTINGS_VIEW: RefCell<Option<SettingsView>> = const { RefCell::new(None) };
    static PROXY_VIEW: RefCell<Option<ProxyView>> = const { RefCell::new(None) };
    static PROXY_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CHECK_SHOWN: std::cell::Cell<(u64, bool)> = const { std::cell::Cell::new((0, false)) };
    static PROXY_SEEN: RefCell<ProxySeen> = RefCell::new(ProxySeen::default());
    static LAST_SCREEN: RefCell<Option<Screen>> = const { RefCell::new(None) };
    static NOTIFIED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Засеянные на старте по данным прошлой сессии: (ключи, когда засеяли). Первый свежий опрос ниже порога
    /// их снимает — иначе 95% → 85% → 92% после перезапуска молчало бы (гистерезис ждал бы 80%).
    static SEEDED: RefCell<(HashSet<String>, i64)> = RefCell::new((HashSet::new(), 0));
    /// Порог, к которому относится NOTIFIED (сменили порог — «уже сообщили» сбрасывается).
    static NOTIFIED_THRESHOLD: std::cell::Cell<f64> = const { std::cell::Cell::new(f64::NAN) };
    static FORM_BASELINE: RefCell<Option<Account>> = const { RefCell::new(None) };
    /// Поколение поиска: результат старого (не успел уложиться в 15 с) в новый поиск не сваливается.
    static DETECTION_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// (поколение поиска, id правимойся карточки, канал результата) — пока результат не пришёл.
    static DETECTION_RESULT_RX: RefCell<Option<(u64, Option<String>, Receiver<Option<usize>>)>> = const { RefCell::new(None) };
    static DETECTION_IN_FLIGHT: RefCell<bool> = const { RefCell::new(false) };
    /// Тексты подсказок кнопок шапки: AppKit их не удерживает — держим сами.
    static HEADER_TIPS: RefCell<Vec<Retained<NSString>>> = const { RefCell::new(Vec::new()) };
    /// Подсказки шапки уже висят: от прокрутки они не зависят, пересобирать незачем.
    static HEADER_TIPS_SHOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Тексты подсказок меток карточек (AppKit их не удерживает) и их теги — снять будем именно их.
    static CARD_TIPS: RefCell<(Vec<Retained<NSString>>, Vec<isize>)> = const { RefCell::new((Vec::new(), Vec::new())) };
    /// Какой набор подсказок меток сейчас стоит и на какой строке прокрутки (None — никакой).
    static LIST_TIPS_SHOWN: RefCell<Option<(Vec<(i64, i64, i64, &'static str)>, i64)>> = const { RefCell::new(None) };
}

pub(crate) static AUTO_REFRESH_LAST: AtomicI64 = AtomicI64::new(0);
static LOGIN_ITEM_REVISION: AtomicI64 = AtomicI64::new(0);
static LOGIN_ITEM_APPLIED: AtomicI64 = AtomicI64::new(-1);
static LOGIN_ITEM_FAILED: AtomicI64 = AtomicI64::new(-1);
static LOGIN_ITEM_LOCK: Mutex<()> = Mutex::new(());
static SINGLE_INSTANCE_LOCK: OnceLock<File> = OnceLock::new();

/* ------------------------------------------------------------------ */
/* Controller                                                          */
/* ------------------------------------------------------------------ */

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SubBarController"]
    struct UiController;

    impl UiController {
        #[unsafe(method(onTogglePopover:))]
        fn on_toggle_popover(&self, _sender: Option<&NSStatusBarButton>) {
            toggle_popover();
        }

        #[unsafe(method(onPopoverWillClose:))]
        fn on_popover_will_close(&self, _notification: &NSNotification) {
            capture_form_draft();
            if APP.lock().unwrap_or_else(|e| e.into_inner()).screen != Screen::Settings {
                return;
            }
            // Порог в настройках вписан, но не подтверждён Enter — закрытие окна не должно его терять:
            // сначала закончить редактирование поля (текст редактора → в поле), потом читать.
            // Окно достаём и отпускаем RefCell до makeFirstResponder: конец правки может позвать нас же.
            if let Some(w) = SETTINGS_VIEW.with(|v| v.borrow().as_ref().and_then(|s| s.view.window())) {
                w.makeFirstResponder(None);
            }
            let valid = SETTINGS_VIEW.with(|v| v.borrow().as_ref().is_none_or(|s| s.notify_percent().is_some()));
            // Остальные переключатели сохраняем всегда: битый порог apply_settings_form пропускает сама.
            apply_settings_form();
            if !valid {
                // Правка не разобралась — молча её терять нельзя: сказать, что не сохранилось.
                APP.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_status("Порог не сохранён: введи число от 0 до 100");
            }
            // Следующее открытие — со списка: в настройках Esc не работает, а висеть там незачем.
            APP.lock().unwrap_or_else(|e| e.into_inner()).screen = Screen::List;
            sync_content(); // как в «Готово»: не рисовать список поверх живого экрана настроек
        }

        #[unsafe(method(onTick:))]
        fn on_tick(&self, _timer: Option<&NSObject>) {
            expire_copied_key();
            tick();
        }

        #[unsafe(method(onShowOnStart:))]
        fn on_show_on_start(&self, _timer: Option<&NSObject>) {
            toggle_popover();
            #[cfg(debug_assertions)]
            if let Ok(kind) = std::env::var("LIMITBAR_NATIVE_SELF_TEST") {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match kind.as_str() {
                        "form" => native_form_self_test(self),
                        "list" => native_list_self_test(),
                        _ => panic!("unknown native self-test"),
                    }
                }));
                match result {
                    Ok(()) => {
                        println!("native {kind} lifecycle: PASS");
                        std::process::exit(0);
                    }
                    Err(_) => {
                        eprintln!("native {kind} lifecycle: FAIL");
                        std::process::exit(1);
                    }
                }
            }
        }

        #[unsafe(method(onAccountRefresh:))]
        fn on_account_refresh(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                state::refresh_account(&id);
                refresh_panel();
            }
        }

        #[unsafe(method(onAccountEdit:))]
        fn on_account_edit(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                let account = {
                    let app = APP.lock().unwrap_or_else(|e| e.into_inner());
                    app.data.accounts.iter().find(|a| a.id == id).cloned()
                };
                if let Some(account) = account {
                    open_form(Some(&account));
                }
            }
        }

        #[unsafe(method(onAccountToggle:))]
        fn on_account_toggle(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                state::toggle_account(&id);
                refresh_panel();
            }
        }

        #[unsafe(method(onAccountPin:))]
        fn on_account_pin(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                state::toggle_pinned(&id);
                refresh_panel();
            }
        }

        #[unsafe(method(onAccountMoveUp:))]
        fn on_account_move_up(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                state::move_pinned_account(&id, true);
                refresh_panel();
            }
        }

        #[unsafe(method(onAccountMoveDown:))]
        fn on_account_move_down(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                state::move_pinned_account(&id, false);
                refresh_panel();
            }
        }

        #[unsafe(method(onAccountCopy:))]
        fn on_account_copy(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                let label = {
                    let app = APP.lock().unwrap_or_else(|e| e.into_inner());
                    app.data.accounts.iter().find(|a| a.id == id).map(|a| a.label.clone())
                };
                if let Some(label) = label {
                    let text = if copy_text(&label) { "Скопировано" } else { "Не удалось скопировать" };
                    APP.lock().unwrap_or_else(|e| e.into_inner()).set_status(text);
                    refresh_panel();
                }
            }
        }

        #[unsafe(method(onAccountCopyKey:))]
        fn on_account_copy_key(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                let key = {
                    let app = APP.lock().unwrap_or_else(|e| e.into_inner());
                    app.data
                        .accounts
                        .iter()
                        .find(|a| a.id == id)
                        .and_then(|a| a.copyable_api_key().map(str::to_string))
                };
                if let Some(key) = key {
                    let text = if copy_text(&key) {
                        // Ключ не должен жить в буфере вечно: через 90 с стираем, если его не сменили.
                        let count = NSPasteboard::generalPasteboard().changeCount();
                        COPIED_KEY.with(|c| c.set(Some((count, crate::store::now_ms() + COPIED_KEY_TTL_MS))));
                        "API key скопирован — сотру из буфера через 90 с"
                    } else {
                        "Не удалось скопировать API key"
                    };
                    APP.lock().unwrap_or_else(|e| e.into_inner()).set_status(text);
                    refresh_panel();
                }
            }
        }

        #[unsafe(method(onAccountDelete:))]
        fn on_account_delete(&self, _sender: Option<&NSObject>) {
            if let Some(id) = take_menu_account() {
                state::remove_account(&id);
                refresh_panel();
            }
        }

        #[unsafe(method(onProviderChanged:))]
        fn on_provider_changed(&self, _sender: Option<&NSObject>) {
            // Формы нет (закрыли, пока шло событие) — менять нечего, а не подставлять Codex по умолчанию.
            if FORM_VIEW.with(|view| view.borrow().is_none()) {
                return;
            }
            let provider = FORM_VIEW.with(|view| {
                view.borrow()
                    .as_ref()
                    .map(|form| form.selected_provider())
                    .unwrap_or(ProviderId::Codex)
            });
            let label = FORM_VIEW.with(|view| {
                view.borrow()
                    .as_ref()
                    .map(|form| form.label_value())
                    .unwrap_or_default()
            });
            // Сохраняем значения полей до переключения — ввод пользователя не стираем.
            let current_values = FORM_VIEW.with(|view| {
                view.borrow()
                    .as_ref()
                    .map(|form| form.values())
                    .unwrap_or_default()
            });
            {
                let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
                app.form.switch_provider(provider, label, current_values);
            }
            sync_content();
        }

        #[unsafe(method(onFormSave:))]
        fn on_form_save(&self, _sender: Option<&NSObject>) {
            save_form();
        }

        #[unsafe(method(onFormCancel:))]
        fn on_form_cancel(&self, _sender: Option<&NSObject>) {
            close_form();
        }

        #[unsafe(method(onFormDelete:))]
        fn on_form_delete(&self, _sender: Option<&NSObject>) {
            let id = APP.lock().unwrap_or_else(|e| e.into_inner()).form.editing_id.clone();
            if let Some(id) = id {
                state::remove_account(&id);
                close_form();
            }
        }

        #[unsafe(method(onDetectCredentials:))]
        fn on_detect_credentials(&self, _sender: Option<&NSObject>) {
            run_detection();
        }

        #[unsafe(method(onProxyChanged:))]
        fn on_proxy_changed(&self, _sender: Option<&NSObject>) {
            apply_proxy_form();
        }

        #[unsafe(method(onProxyCopy:))]
        fn on_proxy_copy(&self, _sender: Option<&NSObject>) {
            if copy_text("claude-sub") {
                proxy_message("Скопировано: claude-sub — запускай вместо claude", false);
            } else {
                proxy_message("Не удалось скопировать — набери claude-sub вручную", true);
            }
        }

        #[unsafe(method(onProxyRestart:))]
        fn on_proxy_restart(&self, _sender: Option<&NSObject>) {
            proxy_message("Перезапускаю мягко: начатые запросы прокси доделает…", false);
            std::thread::spawn(|| match crate::proxy::control::restart_agent() {
                Ok(()) => crate::proxy::control::post_notice("Прокси перезапускается: поднимется, как только доделает начатое".into(), false),
                Err(e) if !crate::proxy::control::agent_installed() => crate::proxy::control::post_notice(format!("Сначала включи «Прокси как служба» ({e})"), true),
                Err(e) => crate::proxy::control::post_notice(format!("Не перезапустил: {e}"), true),
            });
        }

        #[unsafe(method(onProxyLog:))]
        fn on_proxy_log(&self, _sender: Option<&NSObject>) {
            // HOME как у control::install_agent: не-UTF-8 или пустой HOME увёл бы путь в относительный.
            let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
                proxy_message("Не открыл журнал: HOME не задан", true);
                return;
            };
            let log = PathBuf::from(home).join("Library/Logs/SubBar/proxy.log");
            if !log.exists() {
                proxy_message("Журнала ещё нет — прокси пока ничего не писал", false);
                return;
            }
            // Console показывает журнал живым и с поиском; нет его — откроется тем, что назначено для .log.
            // `open` ждёт запуска Console — не на главном потоке, итог придёт через post_notice.
            std::thread::spawn(move || {
                let opened = std::process::Command::new("/usr/bin/open").args(["-a", "Console"]).arg(&log).status().is_ok_and(|s| s.success())
                    || std::process::Command::new("/usr/bin/open").arg(&log).status().is_ok_and(|s| s.success());
                if !opened {
                    crate::proxy::control::post_notice(format!("Не открыл журнал: {}", log.display()), true);
                }
            });
        }

        #[unsafe(method(onProxyCheck:))]
        fn on_proxy_check(&self, _sender: Option<&NSObject>) {
            run_proxy_check();
        }

        #[unsafe(method(onProxyClose:))]
        fn on_proxy_close(&self, _sender: Option<&NSObject>) {
            apply_proxy_form();
            {
                let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
                app.screen = Screen::List;
            }
            sync_content();
        }

        #[unsafe(method(onSettingsChanged:))]
        fn on_settings_changed(&self, _sender: Option<&NSObject>) {
            // Тумблер не применяет недописанный порог («9» на пути к «90» зажгло бы все окна разом):
            // своего действия у поля порога нет — число коммитит закрытие экрана или поповера.
            apply_settings_form_with(false);
            SETTINGS_VIEW.with(|view| {
                if let Some(view) = view.borrow().as_ref() {
                    if view.notify_percent().is_none() {
                        view.set_notify_message("Введи число от 0 до 100", true);
                    } else {
                        view.set_notify_message(crate::ui::settings::NOTIFY_HINT, false);
                    }
                }
            });
        }

        #[unsafe(method(onSettingsClose:))]
        fn on_settings_close(&self, _sender: Option<&NSObject>) {
            // Фиксируем незаконченные правки полей перед закрытием.
            if let Some(window) = SETTINGS_VIEW.with(|view| view.borrow().as_ref().and_then(|v| v.view.window())) {
                window.makeFirstResponder(None);
            }
            let valid = SETTINGS_VIEW.with(|view| {
                view.borrow()
                    .as_ref()
                    .is_none_or(|settings| settings.notify_percent().is_some())
            });
            if !valid {
                SETTINGS_VIEW.with(|view| {
                    if let Some(view) = view.borrow().as_ref() {
                        view.set_notify_message("Введи число от 0 до 100", true);
                    }
                });
                // Тумблеры сохраняем и тут — как при закрытии поповера; битый порог остаётся ждать правки.
                apply_settings_form_with(false);
                return;
            }
            apply_settings_form();
            {
                let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
                app.screen = Screen::List;
            }
            sync_content();
        }

        #[unsafe(method(onQuit:))]
        fn on_quit(&self, _sender: Option<&NSObject>) {
            quit();
        }

        #[unsafe(method(onExport:))]
        fn on_export(&self, _sender: Option<&NSObject>) {
            // Экспорт с замаскированными секретами — токены в буфер не утекают.
            let json = {
                let app = APP.lock().unwrap_or_else(|e| e.into_inner());
                serde_json::to_string_pretty(&state::masked_export(&app.data)).unwrap_or_else(|e| {
                    eprintln!("[subbar] экспорт: {e}");
                    String::new()
                })
            };
            if json.is_empty() {
                APP.lock().unwrap_or_else(|e| e.into_inner()).set_status("Экспорт не удался — не собрал данные");
                refresh_panel();
            } else {
                let text = if copy_text(&json) { "Экспортировано (секреты скрыты)" } else { "Экспорт не удался — буфер обмена не принял" };
                APP.lock().unwrap_or_else(|e| e.into_inner()).set_status(text);
                refresh_panel();
            }
        }
    }
);

const COPIED_KEY_TTL_MS: i64 = 90_000;

thread_local! {
    /// (changeCount после копирования ключа, когда стереть).
    static COPIED_KEY: std::cell::Cell<Option<(isize, i64)>> = const { std::cell::Cell::new(None) };
}

/// Кладёт текст в буфер обмена; false — буфер его не принял.
fn copy_text(text: &str) -> bool {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    unsafe { pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) }
}

fn expire_copied_key() {
    let Some((count, until)) = COPIED_KEY.with(|c| c.get()) else { return };
    if crate::store::now_ms() < until {
        return;
    }
    COPIED_KEY.with(|c| c.set(None));
    let pasteboard = NSPasteboard::generalPasteboard();
    // Пользователь уже скопировал что-то своё — его не трогаем.
    if pasteboard.changeCount() == count {
        pasteboard.clearContents();
    }
}

impl UiController {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(());
        unsafe { objc2::msg_send![super(this), init] }
    }
}

/* ------------------------------------------------------------------ */
/* Panel view                                                          */
/* ------------------------------------------------------------------ */

#[derive(Default)]
struct A11yIvars {
    index: std::cell::Cell<usize>,
}

define_class!(
    /// Зона клика нарисованного списка для VoiceOver: нажатие идёт тем же путём, что и мышь.
    #[unsafe(super(objc2_app_kit::NSAccessibilityElement))]
    #[name = "SubBarA11yButton"]
    #[ivars = A11yIvars]
    struct A11yButton;

    impl A11yButton {
        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> objc2::runtime::Bool {
            let index = self.ivars().index.get();
            let Some(view) = PANEL.with(|cell| cell.borrow().clone()) else { return objc2::runtime::Bool::NO };
            let hit = view.ivars().hitboxes.borrow().get(index).cloned();
            let Some((rect, action)) = hit else { return objc2::runtime::Bool::NO };
            handle_action(action, &view, NSPoint::new(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0));
            objc2::runtime::Bool::YES
        }
    }
);

/// Дети для VoiceOver по свежим хитбоксам; пересобираем, только если зоны или подписи сменились.
fn sync_a11y(view: &PanelView, hitboxes: &[(ui::Rect, Action)]) {
    let items: Vec<(ui::Rect, String)> =
        state::with_app(|app| hitboxes.iter().map(|(rect, action)| (*rect, list::a11y_label(app, action))).collect());
    let sig: Vec<(u64, u64, u64, u64, String)> = items
        .iter()
        .map(|(r, l)| (r.x.to_bits(), r.y.to_bits(), r.w.to_bits(), r.h.to_bits(), l.clone()))
        .collect();
    if *view.ivars().a11y_sig.borrow() == sig {
        return;
    }
    let mtm = MainThreadMarker::from(view);
    let children: Vec<Retained<A11yButton>> = items
        .into_iter()
        .enumerate()
        .map(|(index, (rect, label))| {
            let el: Retained<A11yButton> =
                unsafe { msg_send![super(mtm.alloc::<A11yButton>().set_ivars(A11yIvars { index: std::cell::Cell::new(index) })), init] };
            el.setAccessibilityRole(Some(unsafe { objc2_app_kit::NSAccessibilityButtonRole }));
            el.setAccessibilityLabel(Some(&NSString::from_str(&label)));
            el.setAccessibilityFrameInParentSpace(NSRect::new(NSPoint::new(rect.x, rect.y), NSSize::new(rect.w, rect.h)));
            unsafe { el.setAccessibilityParent(Some(view)) };
            el
        })
        .collect();
    let array = objc2_foundation::NSArray::from_retained_slice(&children);
    // NSArray<A11yButton> → NSArray<AnyObject>: тот же объект, только тип элемента шире.
    let any: &objc2_foundation::NSArray = unsafe { &*(Retained::as_ptr(&array) as *const objc2_foundation::NSArray) };
    unsafe { view.setAccessibilityChildren(Some(any)) };
    view.ivars().a11y_sig.replace(sig);
}

#[derive(Default)]
struct PanelIvars {
    /// Что сейчас отдано VoiceOver (зоны и подписи) — чтобы не пересоздавать детей на каждом кадре.
    a11y_sig: RefCell<Vec<(u64, u64, u64, u64, String)>>,
    hitboxes: RefCell<Vec<(ui::Rect, Action)>>,
    /// Номера карточек под хитбоксами раскрытия — тем же порядком (см. `list::Layout::toggle_rows`).
    toggle_rows: RefCell<Vec<usize>>,
    scroll: RefCell<f64>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[name = "SubBarPanelView"]
    #[ivars = PanelIvars]
    struct PanelView;

    impl PanelView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            // Свой autoreleasepool — без утечки NSString/NSDictionary на каждом кадре.
            objc2::rc::autoreleasepool(|_| {
                let palette = Palette::current();
                let screen = APP.lock().unwrap_or_else(|e| e.into_inner()).screen.clone();
                if screen == Screen::List {
                    let scroll = *self.ivars().scroll.borrow();
                    let layout = state::with_app(|app| list::draw(app, &palette, scroll, self.bounds().size.height));
                    sync_a11y(self, &layout.hitboxes);
                    self.ivars().hitboxes.replace(layout.hitboxes);
                    self.ivars().toggle_rows.replace(layout.toggle_rows);
                    set_list_tooltips(self, &layout.tips, scroll);
                } else {
                    // Хитбоксы списка на другом экране не действуют: наведение не должно цеплять карточки.
                    self.ivars().hitboxes.replace(Vec::new());
                    self.ivars().toggle_rows.replace(Vec::new());
                    sync_a11y(self, &[]);
                    // Тот же фон, что у списка: белые карточки экранов стоят на нём.
                    palette.background.setFill();
                    NSBezierPath::fillRect(NSRect::new(
                        NSPoint::new(0.0, 0.0),
                        NSSize::new(self.bounds().size.width, self.bounds().size.height),
                    ));
                }
            });
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let screen = APP.lock().unwrap_or_else(|e| e.into_inner()).screen.clone();
            if screen != Screen::List {
                return;
            }
            let delta = event.scrollingDeltaY();
            let content = state::with_app(list::content_height);
            let max = list::max_scroll(content, self.bounds().size.height);
            let mut scroll = self.ivars().scroll.borrow_mut();
            *scroll = (*scroll - delta).clamp(0.0, max);
            HOVER_ROW.store(-1, std::sync::atomic::Ordering::Relaxed);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            // Убираем старые зоны отслеживания и ставим одну на весь вид.
            unsafe {
                for area in self.trackingAreas().iter() {
                    self.removeTrackingArea(&area);
                }
                let options = objc2_app_kit::NSTrackingAreaOptions::MouseMoved
                    | objc2_app_kit::NSTrackingAreaOptions::MouseEnteredAndExited
                    | objc2_app_kit::NSTrackingAreaOptions::ActiveAlways;
                let Some(mtm) = MainThreadMarker::new() else {
                    return; // паника в методе AppKit — аварийный выход; без трекинга просто нет подсветки
                };
                let area = objc2_app_kit::NSTrackingArea::initWithRect_options_owner_userInfo(
                    mtm.alloc::<objc2_app_kit::NSTrackingArea>(),
                    self.bounds(),
                    options,
                    Some(self),
                    None,
                );
                self.addTrackingArea(&area);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            // Кнопки шапки подсвечиваются под мышью — как нативные.
            let on_list = APP.lock().unwrap_or_else(|e| e.into_inner()).screen == Screen::List;
            let button = if on_list {
                list::header_buttons().iter().position(|(rect, ..)| rect.contains(point.x, point.y) || rect.contains(point.x, point.y + 2.0) || rect.contains(point.x, point.y - 2.0)).map_or(-1, |i| i as i64) // та же полоса, что у клика
            } else {
                -1
            };
            if list::HOVER_BUTTON.swap(button, std::sync::atomic::Ordering::Relaxed) != button {
                self.setNeedsDisplay(true);
            }
            let found = self.hover_at(point);
            let prev = HOVER_ROW.swap(found, std::sync::atomic::Ordering::Relaxed);
            if prev != found {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            HOVER_ROW.store(-1, std::sync::atomic::Ordering::Relaxed);
            list::HOVER_BUTTON.store(-1, std::sync::atomic::Ordering::Relaxed);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            // Клики разбираем только на экране списка — у формы и настроек родные контролы.
            let screen = APP.lock().unwrap_or_else(|e| e.into_inner()).screen.clone();
            if screen != Screen::List {
                return;
            }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let action = {
                let hitboxes = self.hitboxes();
                hitboxes
                    .iter()
                    .find(|(rect, _)| rect.contains(point.x, point.y))
                    .map(|(_, action)| action.clone())
            };
            if let Some(action) = action {
                // Лёгкий тактильный отклик на клик.
                let manager = NSHapticFeedbackManager::defaultPerformer();
                manager.performFeedbackPattern_performanceTime(NSHapticFeedbackPattern::Generic, objc2_app_kit::NSHapticFeedbackPerformanceTime::Default);
                handle_action(action, self, point);
            }
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            // Тот же поиск цели, что у левого клика, — контекстное меню на строках.
            let screen = APP.lock().unwrap_or_else(|e| e.into_inner()).screen.clone();
            if screen != Screen::List {
                return;
            }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let account_id = {
                let hitboxes = self.hitboxes();
                hitboxes
                    .iter()
                    .find_map(|(rect, action)| match action {
                        Action::ToggleAccount(id) if rect.contains(point.x, point.y) => Some(id.clone()),
                        _ => None,
                    })
            };
            if let Some(id) = account_id {
                show_account_menu(&id, self, point);
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let screen = APP.lock().unwrap_or_else(|e| e.into_inner()).screen.clone();
            if screen != Screen::List {
                unsafe {
                    let _: () = msg_send![super(self), keyDown: event];
                }
                return;
            }
            let key_code = event.keyCode();
            // ⌘Z — вернуть только что удалённую карточку.
            let flags = event.modifierFlags();
            if key_code == 6 && flags.contains(NSEventModifierFlags::Command) && !flags.intersects(NSEventModifierFlags::Control | NSEventModifierFlags::Option) {
                // ⌘⇧Z (повтор) — не отмена: отдаём дальше, а не возвращаем карточку.
                if !flags.contains(NSEventModifierFlags::Shift) && state::restore_removed() {
                    refresh_panel();
                    return;
                }
                unsafe {
                    let _: () = msg_send![super(self), keyDown: event];
                }
                return;
            }
            // Одиночные клавиши ниже — без ⌃/⌥: ⌥R или ⌃«запятая» не должны обновлять или открывать настройки.
            if flags.intersects(NSEventModifierFlags::Control | NSEventModifierFlags::Option) {
                unsafe {
                    let _: () = msg_send![super(self), keyDown: event];
                }
                return;
            }
            // Стрелки — ходим по строкам.
            match key_code {
                125 | 126 | 116 | 121 => { // стрелки вниз/вверх, Page Up (116) / Page Down (121)
                    let direction = if matches!(key_code, 125 | 121) { 1.0 } else { -1.0 };
                    let step = if matches!(key_code, 116 | 121) { (self.bounds().size.height - list::HEADER_HEIGHT).max(40.0) } else { 40.0 };
                    let max_scroll = state::with_app(|app| list::max_scroll(list::content_height(app), self.bounds().size.height));
                    let mut scroll = self.ivars().scroll.borrow_mut();
                    *scroll = (*scroll + direction * step).clamp(0.0, max_scroll);
                    HOVER_ROW.store(-1, std::sync::atomic::Ordering::Relaxed);
                    self.setNeedsDisplay(true);
                    return;
                }
                36 | 76 => { // Enter (и Enter цифрового блока) — раскрыть карточку под курсором, иначе первую целиком видимую
                    let hover = HOVER_ROW.load(std::sync::atomic::Ordering::Relaxed);
                    let action = {
                        let hitboxes = self.hitboxes();
                        list::enter_target(&hitboxes, &self.ivars().toggle_rows.borrow(), hover, *self.ivars().scroll.borrow())
                    };
                    if let Some(action) = action {
                        handle_action(action, self, NSPoint::new(100.0, 100.0));
                    }
                    return;
                }
                53 => { // Esc — закрыть окно
                    POPOVER.with(|cell| {
                        if let Some(popover) = cell.borrow().as_ref() {
                            unsafe { popover.performClose(None) };
                        }
                    });
                    return;
                }
                _ => {}
            }
            // Как ⌘Z выше: по физической клавише, иначе на ЙЦУКЕН R приходит «к» и не работает.
            let by_key = match key_code {
                15 => "r",
                24 => "+",
                43 => ",",
                _ => "",
            };
            // Без символов (мёртвая клавиша, IME) по физической не узнали — отдаём дальше, а не глотаем.
            let chars = event.characters().map(|c| c.to_string()).unwrap_or_default();
            match if by_key.is_empty() { chars.as_str() } else { by_key } {
                "r" | "R" => {
                    let busy = APP.lock().unwrap_or_else(|e| e.into_inner()).refreshing;
                    if busy {
                        APP.lock().unwrap_or_else(|e| e.into_inner()).set_status("Уже обновляю…");
                        refresh_panel();
                    } else {
                        state::refresh_all();
                        refresh_panel();
                    }
                    return; // обработали — дальше не отдаём, иначе NSBeep
                }
                "+" | "=" => return open_form(None),
                "," => return open_settings(),
                _ => {}
            }
            unsafe {
                let _: () = msg_send![super(self), keyDown: event];
            }
        }
    }
);

impl PanelView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(PANEL_WIDTH, PANEL_HEIGHT),
        );
        let this = mtm.alloc::<Self>().set_ivars(PanelIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Какая карточка под точкой (-1 — никакая).
    fn hover_at(&self, point: NSPoint) -> i64 {
        // Через hitboxes(): после прокрутки прямоугольники протухли до отрисовки.
        let hovered_id = self.hitboxes().iter().find_map(|(rect, action)| match action {
            Action::ToggleAccount(id) if rect.contains(point.x, point.y) => Some(id.clone()),
            _ => None,
        });
        // Ищем по устойчивой позиции в отсортированном списке, а не по индексу зоны
        // (шапка и частично прокрученные карточки сдвигают индексы).
        hovered_id.and_then(|id| state::with_app(|app| list::visible_account_index(app, &id))).map_or(-1, |index| index as i64)
    }

    /// Хитбоксы рождаются в drawRect: после прокрутки или флика они протухли — досчитать их перед поиском.
    fn hitboxes(&self) -> std::cell::Ref<'_, Vec<(ui::Rect, Action)>> {
        if self.needsDisplay() {
            self.displayIfNeeded();
        }
        self.ivars().hitboxes.borrow()
    }
}

/* ------------------------------------------------------------------ */
/* Actions                                                             */
/* ------------------------------------------------------------------ */

/// Каталог внутри временного: /tmp на macOS — ссылка на /private/tmp, сравниваем канонические пути.
#[cfg(debug_assertions)]
fn under_temp(dir: &std::path::Path) -> bool {
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let dir = canon(dir);
    [std::path::PathBuf::from("/tmp"), std::env::temp_dir()].iter().any(|root| dir.starts_with(canon(root)))
}

fn take_menu_account() -> Option<String> {
    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    app.menu_account.take()
}

fn handle_action(action: Action, view: &PanelView, point: NSPoint) {
    match action {
        Action::RefreshAll => {
            // Защита от дребезга: пока идёт обновление, клики игнорируем.
            let busy = APP.lock().unwrap_or_else(|e| e.into_inner()).refreshing;
            if busy {
                APP.lock().unwrap_or_else(|e| e.into_inner()).set_status("Уже обновляю…");
            } else {
                state::refresh_all();
            }
            refresh_panel();
        }
        Action::AddAccount => open_form(None),
        Action::OpenSettings => open_settings(),
        Action::OpenProxy => open_proxy(),
        Action::UndoRemove => {
            state::restore_removed();
            refresh_panel();
        }
        Action::AccountMenu(id) => show_account_menu(&id, view, point),
        Action::ToggleAccount(id) => {
            let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
            if app.data.accounts.iter().any(|account| account.id == id)
                && !app.expanded_accounts.remove(&id)
            {
                app.expanded_accounts.insert(id);
            }
            drop(app);
            HOVER_ROW.store(-1, std::sync::atomic::Ordering::Relaxed);
            refresh_panel();
        }
    }
}

fn show_account_menu(id: &str, view: &NSView, point: NSPoint) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(controller) = CONTROLLER.with(|cell| cell.borrow().clone()) else {
        return;
    };
    let (enabled, pinned, has_api_key, place) = {
        let app = APP.lock().unwrap_or_else(|e| e.into_inner());
        let place = state::pin_position(&app.data.accounts, id);
        app.data
            .accounts
            .iter()
            .find(|account| account.id == id)
            .map(|account| (account.enabled, account.pinned(), account.copyable_api_key().is_some(), place))
            .unwrap_or((true, false, false, None))
    };

    let menu = NSMenu::new(mtm);
    // None — разделитель между группами.
    let mut items: Vec<Option<(&str, objc2::runtime::Sel)>> = vec![
        Some(("Обновить", sel!(onAccountRefresh:))),
        Some(("Изменить", sel!(onAccountEdit:))),
        Some(("Скопировать название", sel!(onAccountCopy:))),
    ];
    // Этот пункт — только у сервисов с простым API-ключом.
    if has_api_key {
        items.push(Some(("Скопировать API key", sel!(onAccountCopyKey:))));
    }
    items.push(None);
    items.push(Some((if pinned { "Открепить" } else { "Закрепить наверху" }, sel!(onAccountPin:))));
    if let Some((index, count)) = place {
        if index > 0 {
            items.push(Some(("Поднять выше", sel!(onAccountMoveUp:))));
        }
        if index + 1 < count {
            items.push(Some(("Опустить ниже", sel!(onAccountMoveDown:))));
        }
    }
    items.push(None);
    items.push(Some((if enabled { "Выключить" } else { "Включить" }, sel!(onAccountToggle:))));
    for entry in items {
        let Some((title, selector)) = entry else {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            continue;
        };
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str(title),
                Some(selector),
                &NSString::from_str(""),
            )
        };
        unsafe { item.setTarget(Some(&controller)) };
        menu.addItem(&item);
    }
    // Разделитель перед опасным действием.
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let delete = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Удалить"),
            Some(sel!(onAccountDelete:)),
            &NSString::from_str(""),
        )
    };
    unsafe { delete.setTarget(Some(&controller)) };
    menu.addItem(&delete);
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        app.menu_account = Some(id.to_string());
    }
    menu.popUpMenuPositioningItem_atLocation_inView(None, point, Some(view));
    // Меню закрыто (пункт выбран или отменён) — устаревший id убираем.
    let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
    app.menu_account = None;
}

fn open_form(account: Option<&Account>) {
    FORM_BASELINE.with(|baseline| *baseline.borrow_mut() = account.cloned());
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        app.screen = Screen::Form;
        app.form = match account {
            Some(account) => FormState {
                editing_id: Some(account.id.clone()),
                provider: account.provider,
                label: account.label.clone(),
                values: account.credentials.clone(),
                drafts: std::collections::HashMap::new(),
            },
            // Новая карточка — сразу на сервисе, которого больше всего (ключ OpenCode к ключам OpenCode).
            None => FormState { provider: state::likely_provider(&app.data.accounts), ..FormState::default() },
        };
    }
    sync_content();
}

fn close_form() {
    FORM_BASELINE.with(|baseline| *baseline.borrow_mut() = None);
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        app.screen = Screen::List;
        app.form = FormState::default();
    }
    sync_content();
}

fn open_proxy() {
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        app.screen = Screen::Proxy;
    }
    sync_content();
    // Сразу видно, что вся цепочка жива: ключ, модель, время ответа.
    // Каждая проверка — живой запрос к модели: удачная за последние 10 минут с теми же настройками — не повторяем.
    let cfg = crate::proxy::control::load_config();
    if crate::proxy::control::active() && cfg.enabled && !crate::proxy::control::checked_recently(&check_signature(&cfg), 600) {
        run_proxy_check();
    }
}

/// Что проверяет проверка связи: другой ключ, модель, уровень или выключенная подмена — это другая проверка.
fn check_signature(cfg: &crate::proxy::config::ProxyConfig) -> String {
    // matchModels тоже: от него зависит «⚠ подмена не сработает».
    // Хэш, а не сам ключ: подпись живёт в памяти и в сравнениях, секрету там делать нечего.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (cfg.api_key.trim(), &cfg.model, &cfg.effort, cfg.enabled, &cfg.match_models).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Строки ключей для экрана «Субагенты»: карточки OpenCode Go и статус прокси (паузы, какой в работе).
fn proxy_key_rows(cfg: &crate::proxy::config::ProxyConfig) -> Vec<crate::proxy::control::KeyRow> {
    let status = crate::proxy::control::cached().1;
    state::with_app(|app| {
        crate::proxy::control::key_rows(&app.data.accounts, cfg, status.as_ref(), crate::store::now_ms(), app.data.settings.show_remaining)
    })
}

/// Обновить строки ключей на открытом экране; другое их число — другая высота: экран пересобрать.
fn refresh_proxy_keys(cfg: &crate::proxy::config::ProxyConfig) {
    let rows = proxy_key_rows(cfg);
    let rebuild = PROXY_VIEW.with(|cell| match cell.borrow().as_ref() {
        // Режим «осталось/потрачено» запечён в заголовке: сменился извне — пересобрать экран.
        Some(view) if view.key_count() == rows.len() && view.show_remaining() == state::with_app(|app| app.data.settings.show_remaining) => {
            view.set_keys(rows);
            false
        }
        Some(_) => true,
        None => false,
    });
    if rebuild {
        // Пересборка создаёт пустую строку сообщения — «Сохранено» пропадало ровно на клике по ключу.
        let msg = PROXY_VIEW.with(|cell| cell.borrow().as_ref().and_then(|v| v.message()));
        sync_content();
        if let Some((text, error)) = msg {
            proxy_message(&text, error);
        }
    }
}

fn proxy_message(text: &str, error: bool) {
    PROXY_VIEW.with(|cell| {
        if let Some(view) = cell.borrow().as_ref() {
            view.set_message(text, error);
        }
    });
}

/// Проверка связи через прокси (в фоне); итог подхватит таймер окна.
/// Та же проверка (ключ и модель) уже идёт — второй запрос к модели не шлём.
fn run_proxy_check() {
    let cfg = crate::proxy::control::load_config();
    if crate::proxy::control::start_check(&check_signature(&cfg)) {
        PROXY_VIEW.with(|cell| {
            if let Some(view) = cell.borrow().as_ref() {
                view.set_check("Проверяю: крошечный запрос через прокси…", None);
            }
        });
    } else {
        // Молчание выглядит как сломанная кнопка.
        PROXY_VIEW.with(|cell| {
            if let Some(view) = cell.borrow().as_ref() {
                view.set_check("Проверка уже идёт — дождись итога", None);
            }
        });
    }
}

/// Сохранить форму «Субагенты»: прокси подхватит на лету; служба — в фоне (launchctl медленный).
fn apply_proxy_form() {
    // Битый proxy.json не затираем дефолтами (с пустым ключом): сначала пусть его поправят.
    let before = match crate::proxy::config::try_load(&crate::proxy::config::config_path()) {
        Ok(c) => c,
        Err(e) => {
            proxy_message(&format!("proxy.json повреждён ({e}) — не сохраняю, чтобы не потерять ключ"), true);
            return;
        }
    };
    let snapshot = PROXY_VIEW.with(|cell| cell.borrow().as_ref().map(|view| (view.apply_to(before.clone()), view.service_wanted(), view.statusline_wanted())));
    let Some((cfg, want_service, want_statusline)) = snapshot else {
        return;
    };
    // Строка в Claude Code — не конфиг прокси: своя правка (settings.json Claude Code), свой ответ.
    // Сравниваем с намерением, а не с диском: прошлая правка могла ещё не дописаться.
    let statusline_touched = want_statusline != crate::proxy::control::statusline_intent();
    if statusline_touched {
        // Замок settings.json ждётся до 5 с — не на главном потоке, итог придёт через post_notice.
        proxy_message("Правлю строку в Claude Code…", false);
        crate::proxy::control::set_statusline(want_statusline);
    }
    if cfg == before && want_service == crate::proxy::control::service_intent() {
        return; // «Готово» без изменений — не трогаем файл
    }
    match crate::proxy::control::save_config(&cfg) {
        // Ответ про строку в Claude Code важнее «Сохранено» — не затирать его.
        Ok(()) if statusline_touched => {}
        Ok(()) => proxy_message(&format!("Сохранено · субагенты → {}", cfg.model), false),
        Err(e) => {
            // Не сохранилось — ни проверки по несохранённому ключу, ни переустановки службы.
            // Экран пересобираем (черновик поверх конфига с диска): введённое остаётся видно для повтора,
            // а сообщение ниже говорит, что на диск оно не легло.
            sync_content();
            proxy_message(&format!("Не сохранил настройки: {e}"), true);
            return;
        }
    }
    // Другой ключ, модель или размышление (оно в подписи проверки) — сразу проверить, что отвечают.
    // Только если прокси уже хоть раз ответил поллеру: иначе проверять не у кого.
    if cfg.enabled && check_signature(&cfg) != check_signature(&before) && crate::proxy::control::cached().1.is_some() {
        run_proxy_check();
    }
    PROXY_VIEW.with(|cell| {
        if let Some(view) = cell.borrow().as_ref() {
            view.set_status(crate::proxy::control::cached().1.as_ref(), &cfg);
        }
    });
    // Сначала намерение службы: refresh_proxy_keys может пересобрать экран, и тумблер читается из него.
    if want_service != crate::proxy::control::service_intent() {
        crate::proxy::control::set_service(want_service);
    }
    // Основной ключ сменился — строки ключей по новому конфигу («основной», «в работе»).
    refresh_proxy_keys(&cfg);
}

fn open_settings() {
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        app.screen = Screen::Settings;
    }
    sync_content();
}

fn save_form() {
    // Второй клик по «Добавить», пока форма закрывается, ушёл бы в add_account ещё раз — дубль.
    if APP.lock().unwrap_or_else(|e| e.into_inner()).screen != Screen::Form {
        return;
    }
    // Фиксируем незаконченную правку поля до чтения значений.
    FORM_VIEW.with(|view| {
        if let Some(form) = view.borrow().as_ref() {
            form.view.window().map(|w| w.makeFirstResponder(None));
        }
    });
    let (editing, mut form) = {
        let app = APP.lock().unwrap_or_else(|e| e.into_inner());
        (app.form.editing_id.clone(), app.form.clone())
    };
    let values = FORM_VIEW.with(|view| {
        view.borrow()
            .as_ref()
            .map(|form| form.values())
            .unwrap_or_default()
    });
    let label = FORM_VIEW.with(|view| {
        view.borrow()
            .as_ref()
            .map(|form| form.label_value())
            .unwrap_or_default()
    });
    form.values = values;
    form.label = label;

    if let Some(id) = &editing {
        // Подтягиваем изменения с диска прямо перед оптимистичным слиянием: таймер
        // мог ещё не увидеть запись CLI, сделанную перед «Сохранить».
        APP.lock()
            .unwrap_or_else(|e| e.into_inner())
            .sync_external();
        let baseline = FORM_BASELINE.with(|cell| cell.borrow().clone());
        let current = APP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .data
            .accounts
            .iter()
            .find(|account| account.id == *id)
            .cloned();
        match (baseline, current) {
            (Some(baseline), Some(current)) => {
                match merge_external_form_changes(&baseline, &current, &mut form) {
                    Ok(()) => {}
                    Err(conflicts) => {
                        // Каждое введённое значение сохраняем, посторонние внешние правки вливаем
                        // в форму, а конфликт показываем явно — применит его только повторное сохранение.
                        if form.provider == current.provider {
                            sync_form_controls(&form);
                            {
                                let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
                                if app.screen == Screen::Form
                                    && app.form.editing_id.as_deref() == Some(id.as_str())
                                {
                                    app.form.capture(form.label.clone(), form.values.clone());
                                }
                            }
                            FORM_BASELINE.with(|cell| *cell.borrow_mut() = Some(current));
                            // Действие — первым: длинный список полей обрезался бы вместе с ним.
                            let shown: Vec<_> = conflicts.iter().take(3).map(String::as_str).collect();
                            let more = conflicts.len().saturating_sub(shown.len());
                            let message = format!(
                                "Аккаунт изменён извне — проверь поля и повтори сохранение: {}{}.",
                                shown.join(", "),
                                if more > 0 { format!(" и ещё {more}") } else { String::new() }
                            );
                            FORM_VIEW.with(|view| {
                                if let Some(view) = view.borrow().as_ref() {
                                    view.set_message(&message, true);
                                }
                            });
                        } else {
                            // Базу — на текущее состояние: иначе повторное сохранение сравнивало бы
                            // поля со старым провайдером и выдумывало конфликт по каждому введённому.
                            FORM_BASELINE.with(|cell| *cell.borrow_mut() = Some(current));
                            FORM_VIEW.with(|view| {
                                if let Some(view) = view.borrow().as_ref() {
                                    view.set_message(
                                        "Сервис карточки сменили извне — сохранить поверх нельзя. Введённое осталось в форме: скопируй нужное и создай новую карточку.",
                                        true,
                                    );
                                }
                            });
                        }
                        return;
                    }
                }
            }
            (Some(_), None) => {
                FORM_VIEW.with(|view| {
                    if let Some(view) = view.borrow().as_ref() {
                        view.set_message(
                            "Аккаунт удалён извне. Введённые данные остались в форме; скопируй нужные значения, нажми «Отмена» и создай новую карточку через «+».",
                            true,
                        );
                    }
                });
                return;
            }
            (None, _) => {}
        }
    }

    let result = match &editing {
        Some(id) => state::update_account(id, &form),
        None => state::add_account(&form).map(|_| ()),
    };
    match result {
        Ok(()) => {
            close_form();
            // close_form уже пересобирает панель через sync_content.
        }
        Err(message) => {
            FORM_VIEW.with(|view| {
                if let Some(form) = view.borrow().as_ref() {
                    form.set_message(&message, true);
                }
            });
        }
    }
}

/// Слить правки открытой формы с более новым снимком карточки. Нетронутые
/// поля берут новое значение; поля, изменённые обеими сторонами, выносятся
/// на подтверждение, а не затирают молча правки CLI или обновления.
fn merge_external_form_changes(
    baseline: &Account,
    current: &Account,
    form: &mut FormState,
) -> Result<(), Vec<String>> {
    let external_changed_provider = current.provider != baseline.provider;
    if external_changed_provider && current.provider != form.provider {
        return Err(vec!["провайдер".to_string()]);
    }

    let mut conflicts = Vec::new();
    // Пустое название при правке не применяется — это не правка, и конфликта по нему быть не может.
    if form.label.trim() == baseline.label.trim() || form.label.trim().is_empty() {
        form.label = current.label.clone();
    } else if current.label.trim() != baseline.label.trim()
        && current.label.trim() != form.label.trim()
    {
        conflicts.push("название".to_string());
    }

    for field in crate::model::credential_fields(form.provider) {
        let base = if baseline.provider == form.provider {
            baseline
                .credentials
                .get(field.key)
                .map(String::as_str)
                .unwrap_or("")
        } else {
            ""
        };
        let external = if current.provider == form.provider {
            current
                .credentials
                .get(field.key)
                .map(String::as_str)
                .unwrap_or("")
        } else {
            ""
        };
        // В credentials значения лежат обрезанными: вставка с переводом строки — не правка.
        let desired = form.values.get(field.key).map(|v| v.trim()).unwrap_or("");
        if desired == base.trim() {
            if current.provider == form.provider {
                form.values
                    .insert(field.key.to_string(), external.to_string());
            }
        } else if external.trim() != base.trim() && external.trim() != desired {
            conflicts.push(field.label.to_string());
        }
    }

    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(conflicts)
    }
}

fn sync_form_controls(form: &FormState) {
    FORM_VIEW.with(|view| {
        if let Some(view) = view.borrow().as_ref() {
            view.label_field
                .setStringValue(&NSString::from_str(&form.label));
            for (key, field) in &view.fields {
                if let Some(value) = form.values.get(key) {
                    field.setStringValue(&NSString::from_str(value));
                }
            }
        }
    });
}

/// Сколько ждать поиска подписок, прежде чем сдаться.
const DETECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn run_detection() {
    if DETECTION_IN_FLIGHT.with(|busy| *busy.borrow()) {
        return;
    }
    DETECTION_IN_FLIGHT.with(|busy| *busy.borrow_mut() = true);
    // Поколение поиска: через 15 с кнопка отпустится, и поиск можно запустить заново — тогда итог
    // первого не должен ни разблокировать кнопку второго, ни попасть в его форму.
    let gen = DETECTION_GEN.get().wrapping_add(1);
    DETECTION_GEN.set(gen);
    // Правится карточка: по ней и поймём, что форма та же (её пересобирают даже при смене провайдера).
    let editing = APP.lock().unwrap_or_else(|e| e.into_inner()).form.editing_id.clone();
    FORM_VIEW.with(|view| {
        if let Some(view) = view.borrow().as_ref() {
            view.detect_button.setEnabled(false);
        }
    });
    FORM_VIEW.with(|view| {
        if let Some(form) = view.borrow().as_ref() {
            form.set_message(DETECTING_MESSAGE, false);
        }
    });
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    DETECTION_RESULT_RX.with(|slot| *slot.borrow_mut() = Some((gen, editing, result_rx)));
    // Поиск идёт в фоне; когда он закончится, импортируем всё и
    // оставляем форму открытой, чтобы не потерять начатую правку.
    // Подписываемся до старта рабочего потока: очень быстрый поиск
    // иначе мог бы закончиться раньше, чем мы начнём слушать.
    let event_rx = state::subscribe();
    state::detect_credentials();
    spawn_import_watch(result_tx, event_rx);
}

fn spawn_import_watch(result_tx: Sender<Option<usize>>, rx: Receiver<state::WorkerEvent>) {
    // Ждём события DetectDone, а не фиксированную паузу.
    std::thread::spawn(move || {
        // Ждём окончания поиска не дольше DETECT_TIMEOUT.
        let deadline = std::time::Instant::now() + DETECT_TIMEOUT;
        let found_count = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(state::WorkerEvent::DetectDone(found)) => break Some(found.len()),
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if std::time::Instant::now() > deadline =>
                {
                    break None
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
            }
        };
        let _ = result_tx.send(found_count);
    });
}

fn poll_detection_results() {
    let pending = DETECTION_RESULT_RX.with(|slot| {
        let slot = slot.borrow();
        let (gen, editing, receiver) = slot.as_ref()?;
        let result = match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => None,
        };
        Some((*gen, editing.clone(), result))
    });
    let Some((gen, editing, result)) = pending else {
        return;
    };
    let stale = gen != DETECTION_GEN.get();
    // Флаг гасим всегда: у устаревшего поиска некому больше его снять — кнопка «Искать» залипла бы.
    DETECTION_IN_FLIGHT.with(|busy| *busy.borrow_mut() = false);
    // Канал забираем только свой: чужой поиск ещё ждёт свой результат.
    DETECTION_RESULT_RX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(g, _, _)| *g == gen) {
            *slot = None;
        }
    });
    if stale {
        // Пока шёл этот поиск, запустили новый — его итог и его форма.
        return;
    }
    let (message, error) = match result {
        Some(Some(found_count)) => {
            // `flush_events()` применяет DetectDone к AppState до рассылки
            // копии подписчикам. Импорт — на главном потоке после этого,
            // чтобы не гоняться с `app.detected` и не импортировать старое.
            let imported = state::import_detected();
            let message = if imported > 0 {
                format!("Добавлено: {}", crate::util::plural(imported as u64, "подписка", "подписки", "подписок"))
            } else if found_count > 0 {
                "Найденные уже добавлены".to_string()
            } else {
                "Ничего нового не нашлось".to_string()
            };
            (message, false)
        }
        Some(None) => (
            format!("Поиск не завершился за {} с. Результат не импортирован", DETECT_TIMEOUT.as_secs()),
            true,
        ),
        None => ("Поиск прервался. Попробуй ещё раз".to_string(), true),
    };
    APP.lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_status(message.clone());
    FORM_VIEW.with(|view| {
        if let Some(view) = view.borrow().as_ref() {
            view.detect_button.setEnabled(true);
            // Итог — той карточке, из которой запускали: форма могла закрыться, и писать ему некуда.
            let same = state::with_app(|app| app.form.editing_id.clone()) == editing;
            if same {
                view.set_message(&message, error);
                // Ошибку форма объявит сама; итог-успех VoiceOver иначе не услышал бы вовсе.
                if !error {
                    ui::form::announce(&view.message_label, &NSString::from_str(&message));
                }
            } else if view.message_label.stringValue().to_string() == DETECTING_MESSAGE {
                // Другая карточка, открытая во время поиска, показала «Ищу…» — поиск кончился, снимаем.
                view.set_message("", false);
            }
        }
    });
    refresh_panel();
}

const DETECTING_MESSAGE: &str = "Ищу сохранённые входы на этом Mac…";

fn apply_settings_form() {
    apply_settings_form_with(true);
}

fn apply_settings_form_with(include_notify: bool) {
    let snapshot = SETTINGS_VIEW.with(|view| {
        view.borrow().as_ref().map(|settings| {
            (
                settings.refresh_seconds(),
                settings.show_remaining(),
                settings.show_tray_percent(),
                settings.launch_at_login(),
                settings.notify_percent(),
            )
        })
    });
    if let Some((refresh, remaining, tray, login, notify)) = snapshot {
        state::update_settings(|settings| {
            settings.refresh_seconds = refresh;
            settings.show_remaining = remaining;
            settings.show_tray_percent = tray;
            settings.launch_at_login = login;
            if let Some(notify) = notify.filter(|_| include_notify) {
                settings.notify_used_percent = notify;
            }
        });
    }
    refresh_panel();
    // Порог могли поменять прямо сейчас: пересчитать уведомления, не дожидаясь внешнего события.
    notify_thresholds();
}

/* ------------------------------------------------------------------ */
/* Content sync                                                        */
/* ------------------------------------------------------------------ */

fn capture_form_draft() {
    let provider = state::with_app(|app| (app.screen == Screen::Form).then_some(app.form.provider));
    let Some(provider) = provider else {
        return;
    };
    let snapshot = FORM_VIEW.with(|cell| {
        let form = cell.borrow();
        let form = form.as_ref()?;
        // Смена сервиса уже спрятала старые значения в onProviderChanged:.
        if form.rendered_provider != provider {
            return None;
        }
        // Читаем активный редактор *до* смены фокуса и исчезновения окна:
        // последние нажатия могут ещё не дойти до ячейки контрола.
        let snapshot = (form.label_value(), form.values());
        if let Some(window) = form.view.window() {
            window.makeFirstResponder(None);
        }
        Some(snapshot)
    });
    if let Some((label, values)) = snapshot {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        if app.screen == Screen::Form && app.form.provider == provider {
            app.form.capture(label, values);
        }
    }
}

#[cfg(debug_assertions)]
fn native_form_self_test(controller: &UiController) {
    // Только на изолированном синтетическом состоянии, никогда на настоящих данных.
    let data_dir = crate::store::data_dir();
    let isolated = data_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("limitbar-native-selftest."));
    assert!(
        isolated && under_temp(&data_dir),
        "native self-test needs its own temporary data directory"
    );
    assert!(POPOVER.with(|cell| cell.borrow().as_ref().is_some_and(|p| p.isShown())));
    let codex_key = "synthetic-uncommitted-key";
    FORM_VIEW.with(|cell| {
        let borrow = cell.borrow();
        let form = borrow.as_ref().expect("form must be visible");
        form.label_field
            .setStringValue(&NSString::from_str("Synthetic draft"));
        let token: &objc2_app_kit::NSTextField = &form
            .fields
            .iter()
            .find(|(key, _)| key == "accessToken")
            .expect("Codex token field")
            .1;
        let window = form.view.window().expect("form window");
        assert!(window.makeFirstResponder(Some(token)));
        let editor = token.currentEditor().expect("active field editor");
        editor.setString(&NSString::from_str(codex_key));
        assert!(form
            .values()
            .get("accessToken")
            .is_some_and(|v| v == codex_key));
    });
    POPOVER.with(|cell| {
        let borrow = cell.borrow();
        unsafe { borrow.as_ref().unwrap().performClose(None) };
    });
    assert!(state::with_app(|app| app
        .form
        .values
        .get("accessToken")
        .is_some_and(|v| v == codex_key)));
    assert!(state::with_app(|app| app.form.label == "Synthetic draft"));
    toggle_popover();
    assert!(POPOVER.with(|cell| cell.borrow().as_ref().is_some_and(|p| p.isShown())));
    FORM_VIEW.with(|cell| {
        let borrow = cell.borrow();
        let form = borrow.as_ref().expect("reopened form");
        assert!(
            form.values()
                .get("accessToken")
                .is_some_and(|v| v == codex_key),
            "reopen lost uncommitted editor text"
        );
        assert!(form.label_value() == "Synthetic draft");
        let index = form
            .provider_order
            .iter()
            .position(|p| *p == ProviderId::OpenCodeGo)
            .unwrap();
        form.provider_popup.selectItemAtIndex(index as isize);
    });
    unsafe {
        let _: () = msg_send![controller, onProviderChanged: Option::<&NSObject>::None];
    }
    FORM_VIEW.with(|cell| {
        let borrow = cell.borrow();
        let form = borrow.as_ref().expect("OpenCode form");
        assert!(
            form.values().get("apiKey").is_some_and(String::is_empty),
            "providers shared a secret"
        );
        form.fields
            .iter()
            .find(|(key, _)| key == "apiKey")
            .unwrap()
            .1
            .setStringValue(&NSString::from_str("synthetic-opencode-key"));
        let index = form
            .provider_order
            .iter()
            .position(|p| *p == ProviderId::Codex)
            .unwrap();
        form.provider_popup.selectItemAtIndex(index as isize);
    });
    unsafe {
        let _: () = msg_send![controller, onProviderChanged: Option::<&NSObject>::None];
    }
    FORM_VIEW.with(|cell| {
        let borrow = cell.borrow();
        let form = borrow.as_ref().expect("restored Codex form");
        assert!(form
            .values()
            .get("accessToken")
            .is_some_and(|v| v == codex_key));
    });
    close_form();
    assert!(state::with_app(
        |app| app.form.values.is_empty() && app.form.drafts.is_empty()
    ));
    open_form(None);
    FORM_VIEW.with(|cell| {
        let borrow = cell.borrow();
        let form = borrow.as_ref().expect("new empty form");
        assert!(form
            .values()
            .get("accessToken")
            .is_some_and(String::is_empty));
    });
    let disk = std::fs::read_to_string(crate::store::state_path()).unwrap_or_default();
    assert!(!disk.contains(codex_key), "unsaved draft reached the disk");
}

#[cfg(debug_assertions)]
fn native_list_self_test() {
    // Все карточки живут в изолированном снимке и выключены: без HTTP. Тест пишет на диск —
    // значит, как и у формы, только во временном каталоге.
    let data_dir = crate::store::data_dir();
    let isolated = data_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("limitbar-native-selftest."));
    assert!(
        isolated && under_temp(&data_dir),
        "native self-test needs its own temporary data directory"
    );
    let make_account = |name: &str| Account {
        id: name.to_string(),
        provider: ProviderId::Codex,
        label: name.to_string(),
        enabled: false,
        credentials: Default::default(),
        options: Default::default(),
        created_at: 0,
        last_usage: None,
        selected_model: None,
        selected_window: None,
    };
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        assert!(app.data.accounts.is_empty());
        app.data.accounts.push(make_account("First"));
    }
    let panel = PANEL.with(|cell| cell.borrow().as_ref().unwrap().clone());
    // Панель — ровно по контейнеру поповера и от его верха: иначе экран съезжает вниз с пустотой сверху.
    let fits = |panel: &PanelView| {
        let container = unsafe { panel.superview() }.expect("container").bounds().size.height;
        panel.frame().origin.y == 0.0 && (panel.frame().size.height - container).abs() < 0.5
    };
    refresh_panel();
    let compact_height = panel.bounds().size.height;
    assert!((270.0..=280.0).contains(&compact_height));
    assert!(fits(&panel), "panel does not match the popover container");
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        for name in ["Second", "Third", "Fourth"] {
            app.data.accounts.push(make_account(name));
        }
    }
    refresh_panel();
    let four_height = panel.bounds().size.height;
    assert!(four_height > compact_height, "more cards did not grow the popover");
    assert!(fits(&panel), "panel does not match the popover container after growing");
    handle_action(
        Action::ToggleAccount("First".into()),
        &panel,
        NSPoint::new(0.0, 0.0),
    );
    assert!(
        panel.bounds().size.height > four_height,
        "expansion did not resize the popover"
    );
    assert!(
        POPOVER.with(|cell| cell.borrow().as_ref().is_some_and(|p| p.isShown())),
        "expansion dismissed the popover"
    );
    // Удаление возвращается на своё место (15 секунд, «Вернуть» или ⌘Z).
    let position = |id: &str| state::with_app(|app| app.data.accounts.iter().position(|a| a.id == id));
    let before = position("Second");
    state::remove_account("Second");
    assert!(position("Second").is_none(), "card was not removed");
    assert!(state::with_app(|app| !app.removed.is_empty()), "nothing to undo");
    assert!(state::restore_removed(), "undo failed");
    assert_eq!(position("Second"), before, "restored card moved");
    assert!(!state::restore_removed(), "undo twice");
    // Другой экран и обратно — высота меняется, панель остаётся на месте.
    open_settings();
    assert!(fits(&panel), "settings screen shifted inside the popover");
    {
        APP.lock().unwrap_or_else(|e| e.into_inner()).screen = crate::state::Screen::List;
    }
    sync_content();
    assert!(fits(&panel), "list shifted inside the popover after returning");
}

fn sync_content() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // После того как WillClose забрал активный редактор, отцепленные старые контролы
    // могут держать *прежнее* значение ячейки. При повторном открытии их не перечитываем.
    let popover_shown = POPOVER.with(|cell| cell.borrow().as_ref().is_some_and(|p| p.isShown()));
    if popover_shown {
        capture_form_draft();
    }
    let screen = APP.lock().unwrap_or_else(|e| e.into_inner()).screen.clone();
    PANEL.with(|cell| {
        let borrow = cell.borrow();
        let Some(panel) = borrow.as_ref() else {
            return;
        };
        // Убираем старые подвиды — проходим снимок один раз, а не цикл по счётчику.
        let subviews = panel.subviews();
        for i in 0..subviews.count() {
            let view = subviews.objectAtIndex(i);
            view.removeFromSuperview();
        }
        FORM_VIEW.with(|cell| *cell.borrow_mut() = None);
        SETTINGS_VIEW.with(|cell| *cell.borrow_mut() = None);
        // Экран «Субагенты» пересобирается и на месте (сменилось число ключей) — правки не теряем.
        let proxy_draft = PROXY_VIEW.with(|cell| cell.borrow_mut().take()).filter(|_| screen == Screen::Proxy).map(|v| v.draft(crate::proxy::control::load_config()));

        match screen {
            Screen::Proxy => {
                let controller = CONTROLLER.with(|cell| cell.borrow().clone());
                let show_remaining = state::with_app(|app| app.data.settings.show_remaining);
                if let Some(controller) = controller {
                    let (cfg, service, statusline) = proxy_draft.unwrap_or_else(|| {
                        (crate::proxy::control::load_config(), crate::proxy::control::service_intent(), crate::proxy::control::statusline_intent())
                    });
                    let view = ProxyView::new(mtm, &controller, &cfg, proxy_key_rows(&cfg), service, statusline, show_remaining);
                    view.set_status(crate::proxy::control::cached().1.as_ref(), &cfg);
                    // Экран пересобран (открыли окно заново) — последний итог проверки не теряем.
                    let (check_gen, result) = crate::proxy::control::check_result();
                    match &result {
                        Some((text, ok, at)) => view.set_check(&crate::proxy::control::check_text(text, *at, crate::store::now_ms()), Some(*ok)),
                        None if check_gen > 0 => view.set_check("Проверяю: крошечный запрос через прокси…", None),
                        None => {}
                    }
                    CHECK_SHOWN.with(|c| c.set((check_gen, result.is_some())));
                    panel.addSubview(&view.view);
                    panel.setFrameSize(NSSize::new(PANEL_WIDTH, view.view.frame().size.height));
                    // Иначе клавиши уходят списку за кадром (писк), и Tab по контролам не ходит.
                    if let Some(window) = panel.window() {
                        window.makeFirstResponder(None);
                    }
                    PROXY_VIEW.with(|cell| *cell.borrow_mut() = Some(view));
                }
            }
            Screen::List => {
                let height = state::with_app(|app| list::content_height(app).min(list::list_max_height()));
                panel.setFrameSize(NSSize::new(PANEL_WIDTH, height.max(280.0)));
                // Пока окно было закрыто, список мог усохнуть — старая прокрутка дала бы пустую панель.
                let content = state::with_app(list::content_height);
                let max_scroll = list::max_scroll(content, height.max(280.0));
                let mut scroll = panel.ivars().scroll.borrow_mut();
                *scroll = (*scroll).min(max_scroll);
            }
            Screen::Form => {
                let controller = CONTROLLER.with(|cell| cell.borrow().clone());
                let (provider, label, values, editing) = {
                    let app = APP.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        app.form.provider,
                        app.form.label.clone(),
                        app.form.values.clone(),
                        app.form.editing_id.is_some(),
                    )
                };
                if let Some(controller) = controller {
                    // Пустое название — это имя и будет (продолжение нумерации): видно заранее, в подсказке поля.
                    let placeholder = state::with_app(|app| state::suggested_label(&app.data.accounts, provider));
                    let form = FormView::new(mtm, &controller, provider, &label, &values, editing, &placeholder);
                    if DETECTION_IN_FLIGHT.with(|busy| *busy.borrow()) {
                        form.detect_button.setEnabled(false);
                        form.set_message(DETECTING_MESSAGE, false);
                    }
                    panel.addSubview(&form.view);
                    panel.setFrameSize(NSSize::new(PANEL_WIDTH, form.view.frame().size.height));
                    if let Some(window) = panel.window() {
                        window.makeFirstResponder(Some(&form.label_field));
                    }
                    FORM_VIEW.with(|cell| *cell.borrow_mut() = Some(form));
                }
            }
            Screen::Settings => {
                let controller = CONTROLLER.with(|cell| cell.borrow().clone());
                let settings = APP
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .data
                    .settings
                    .clone();
                if let Some(controller) = controller {
                    // «Закреплённая» — только если в кольцах и правда она (сломанную закреплённую трей пропускает).
                    let (tray_label, any_pinned) = state::with_app(|app| {
                        // По id: у двух карточек может быть одно название, а закреплена лишь одна.
                        let pick = tray_pick(app);
                        let pinned = pick.as_ref().is_some_and(|(id, ..)| app.data.accounts.iter().any(|a| a.pinned() && &a.id == id));
                        (pick.map(|(_, label, ..)| label), pinned)
                    });
                    let view = SettingsView::new(mtm, &controller, &settings, tray_label.as_deref().map(|label| (label, any_pinned)));
                    panel.addSubview(&view.view);
                    panel.setFrameSize(NSSize::new(PANEL_WIDTH, view.view.frame().size.height));
                    // Без фокус-ринга на первом контроле при открытии: Tab по-прежнему работает.
                    if let Some(window) = panel.window() {
                        window.makeFirstResponder(None);
                    }
                    SETTINGS_VIEW.with(|cell| *cell.borrow_mut() = Some(view));
                }
            }
        }
        if screen == Screen::List {
            if let Some(window) = panel.window() {
                window.makeFirstResponder(Some(panel));
            }
        }
        if screen != Screen::List {
            clear_list_tooltips(panel);
        }
        LAST_SCREEN.with(|cell| *cell.borrow_mut() = Some(screen.clone()));
        panel.setNeedsDisplay(true);
    });
    resize_popover();
}

/// Подсказки списка: кнопки шапки (подписей на них нет — только иконки) и метки карточек (булавка,
/// «субагенты»). Ключ дедупликации — без сдвига прокрутки (кто и сколько) плюс прокрутка шагами
/// `TOOLTIP_SCROLL_STEP`: иначе на каждый кадр колеса весь набор пересоздаётся, и всплывшая подсказка гаснет.
fn set_list_tooltips(panel: &PanelView, cards: &[(ui::Rect, &'static str)], scroll: f64) {
    // Место в содержимом (y + scroll) от прокрутки не зависит, но меняется при раскрытии/удалении карточки выше.
    let key: Vec<(i64, i64, i64, &'static str)> = cards
        .iter()
        .map(|(r, tip)| ((r.y + scroll).round() as i64, r.w.round() as i64, r.h.round() as i64, *tip))
        .collect();
    let step = (scroll / TOOLTIP_SCROLL_STEP).round() as i64;
    let headers_ready = HEADER_TIPS_SHOWN.with(|shown| shown.get());
    if headers_ready
        && LIST_TIPS_SHOWN.with(|shown| shown.borrow().as_ref().is_some_and(|(rows, at)| *rows == key && *at == step))
    {
        return;
    }
    if headers_ready {
        // Карточки: снимаем ровно те подсказки, что сами повесили (removeAllToolTips унёс бы и шапку).
        CARD_TIPS.with(|cell| {
            let (_, tags) = &mut *cell.borrow_mut();
            for tag in tags.drain(..) {
                panel.removeToolTip(tag as _);
            }
        });
    } else {
        // Шапка от прокрутки не зависит: её подсказки живут весь экран.
        panel.removeAllToolTips();
        HEADER_TIPS.with(|tips| {
            let mut tips = tips.borrow_mut();
            tips.clear();
            for (rect, _, _, tip) in list::header_buttons() {
                let text = NSString::from_str(tip);
                let frame = NSRect::new(NSPoint::new(rect.x, rect.y), NSSize::new(rect.w, rect.h));
                // owner — сам текст: AppKit зовёт у него description, userData не нужен.
                unsafe { panel.addToolTipRect_owner_userData(frame, &text, std::ptr::null_mut()) };
                tips.push(text);
            }
        });
        HEADER_TIPS_SHOWN.set(true);
    }
    CARD_TIPS.with(|cell| {
        let (tips, tags) = &mut *cell.borrow_mut();
        tips.clear();
        tags.clear();
        for (rect, tip) in cards {
            let text = NSString::from_str(tip);
            let frame = NSRect::new(NSPoint::new(rect.x, rect.y), NSSize::new(rect.w, rect.h));
            tags.push(unsafe { panel.addToolTipRect_owner_userData(frame, &text, std::ptr::null_mut()) } as isize);
            tips.push(text);
        }
    });
    LIST_TIPS_SHOWN.with(|shown| *shown.borrow_mut() = Some((key, step)));
}

/// Шаг прокрутки, на котором подсказки меток переезжают: меньше шага — не трогаем (полстроки не видно).
const TOOLTIP_SCROLL_STEP: f64 = 4.0;

/// Другой экран: подсказок списка там быть не должно; вернёмся — пересоберутся заново.
fn clear_list_tooltips(panel: &PanelView) {
    panel.removeAllToolTips();
    HEADER_TIPS.with(|tips| tips.borrow_mut().clear());
    CARD_TIPS.with(|cell| {
        let (texts, tags) = &mut *cell.borrow_mut();
        texts.clear();
        tags.clear();
    });
    HEADER_TIPS_SHOWN.set(false);
    // Сбросить запомненный набор: следующая отрисовка списка добавит подсказки снова.
    LIST_TIPS_SHOWN.with(|shown| *shown.borrow_mut() = None);
}

fn resize_popover() {
    let height = PANEL.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|panel| {
                // Содержимое уменьшилось — поджимаем прокрутку, чтобы не показывать пустоту.
                // Только на списке: высота под-экрана — не высота списка, иначе «Назад» терял прокрутку.
                let on_list = APP.lock().unwrap_or_else(|e| e.into_inner()).screen == Screen::List;
                if !on_list {
                    return panel.frame().size.height;
                }
                let content = state::with_app(list::content_height);
                let max_scroll = list::max_scroll(content, panel.bounds().size.height);
                let mut scroll = panel.ivars().scroll.borrow_mut();
                *scroll = (*scroll).min(max_scroll);
                panel.frame().size.height
            })
            .unwrap_or(PANEL_HEIGHT)
    });
    let height = height.min(list::list_max_height());
    POPOVER.with(|cell| {
        if let Some(popover) = cell.borrow().as_ref() {
            popover.setContentSize(NSSize::new(PANEL_WIDTH, height));
        }
    });
    // Контейнер — под поповер, панель — ровно по контейнеру, от верхнего края.
    PANEL.with(|cell| {
        if let Some(panel) = cell.borrow().as_ref() {
            if let Some(superview) = unsafe { panel.superview() } {
                superview.setFrameSize(NSSize::new(PANEL_WIDTH, height));
            }
            panel.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_WIDTH, height)));
        }
    });
}

fn refresh_panel() {
    // Подсветку не гасим, а пересчитываем под курсором: иначе раз в 30 с она пропадала
    // с карточки под неподвижной мышью, а карточки могли и переехать.
    PANEL.with(|cell| {
        let found = cell.borrow().as_ref().and_then(|panel| {
            let window = panel.window()?;
            let point = panel.convertPoint_fromView(window.mouseLocationOutsideOfEventStream(), None);
            Some(panel.hover_at(point))
        });
        HOVER_ROW.store(found.unwrap_or(-1), std::sync::atomic::Ordering::Relaxed);
    });
    let screen = APP.lock().unwrap_or_else(|e| e.into_inner()).screen.clone();
    let last = LAST_SCREEN.with(|cell| cell.borrow().clone());
    if last != Some(screen.clone()) {
        sync_content();
        return;
    }
    if screen == Screen::List {
        let height = state::with_app(|app| list::content_height(app).min(list::list_max_height()).max(280.0));
        PANEL.with(|cell| {
            if let Some(panel) = cell.borrow().as_ref() {
                panel.setFrameSize(NSSize::new(PANEL_WIDTH, height));
                panel.setNeedsDisplay(true);
            }
        });
        resize_popover();
    }
}

/// Запомнить метрики экрана: список может быть выше экранов с формами, кольца значка — по строке меню.
/// Считаем на каждое открытие окна: монитор мог смениться, а второй раз никто не посчитает.
fn sync_screen_metrics(mtm: MainThreadMarker) {
    if let Some(screen) = objc2_app_kit::NSScreen::mainScreen(mtm) {
        let visible = screen.visibleFrame();
        list::set_screen_height(visible.size.height);
        set_menu_bar_height(screen.frame().size.height - (visible.origin.y + visible.size.height));
    }
}

fn toggle_popover() {
    if let Some(mtm) = MainThreadMarker::new() {
        sync_screen_metrics(mtm);
    }
    POPOVER.with(|cell| {
        let borrow = cell.borrow();
        let Some(popover) = borrow.as_ref() else {
            return;
        };
        if popover.isShown() {
            unsafe { popover.performClose(None) };
            return;
        }
        // Активируем приложение, чтобы поля ввода могли получить фокус.
        if let Some(mtm) = MainThreadMarker::new() {
            let app = NSApplication::sharedApplication(mtm);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        }
        sync_content();
        let button = STATUS_BUTTON.with(|cell| cell.borrow().clone());
        if let Some(button) = button {
            popover.showRelativeToRect_ofView_preferredEdge(
                button.bounds(),
                &button,
                objc2_foundation::NSRectEdge::MinY,
            );
            // После повторного открытия ставим полезный первый ответчик.
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    if let Some(window) = panel.window() {
                        match state::with_app(|app| app.screen.clone()) {
                            Screen::List => {
                                window.makeFirstResponder(Some(panel));
                            }
                            Screen::Form => FORM_VIEW.with(|cell| {
                                if let Some(form) = cell.borrow().as_ref() {
                                    window.makeFirstResponder(Some(&form.label_field));
                                }
                            }),
                            Screen::Settings | Screen::Proxy => {
                                window.makeFirstResponder(None);
                            }
                        }
                    }
                }
            });
        }
    });
}

/* ------------------------------------------------------------------ */
/* Tray + timer                                                        */
/* ------------------------------------------------------------------ */

/// Кольцо значка строки меню: доля дуги и число внутри (None — только кольцо).
#[derive(Clone, PartialEq)]
struct TrayRing {
    fraction: Option<f64>,
    text: Option<String>,
}

/// Диаметр колец значка (в битах f64): по высоте строки меню — на экране с вырезом она 33 пт, обычная — 24.
static TRAY_RING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Запомнить высоту строки меню (на каждое открытие окна — монитор мог смениться): кольца — на 7 пт меньше неё, от 20 до 26 пт.
fn set_menu_bar_height(height: f64) {
    let height = if height >= 20.0 { height } else { 24.0 }; // строка скрывается сама — берём обычную
    TRAY_RING.store((height - 7.0).clamp(20.0, 26.0).to_bits(), Ordering::Relaxed);
}

/// Значок строки меню: по кольцу на окно лимита (до трёх — в том же порядке, что в карточке: 5ч, 7д, 30д),
/// дуга — доля, число внутри — тот же процент. Шаблонная картинка: система сама красит её под строку меню.
fn tray_icon(rings: &[TrayRing]) -> Retained<NSImage> {
    let bits = TRAY_RING.load(Ordering::Relaxed);
    // Всё — от диаметра кольца: при 26 пт цифры 12,5 пт (почти как часы в строке меню), «100» — 11 пт.
    let d: f64 = if bits == 0 { 22.0 } else { f64::from_bits(bits) };
    let (gap, h, stroke) = ((d * 0.16).round(), d + 2.0, (d * 0.085 * 10.0).round() / 10.0);
    let (two_digits, three_digits) = ((d * 0.48 * 2.0).round() / 2.0, (d * 0.42 * 2.0).round() / 2.0);
    let rings: Vec<TrayRing> = if rings.is_empty() { vec![TrayRing { fraction: None, text: Some("–".into()) }] } else { rings.to_vec() };
    let count = rings.len() as f64;
    let size = NSSize::new(count * d + (count - 1.0) * gap, h);
    let handler = block2::RcBlock::new(move |_rect: NSRect| -> objc2::runtime::Bool {
        let black = NSColor::blackColor();
        for (i, ring) in rings.iter().enumerate() {
            let (cx, cy, r) = (i as f64 * (d + gap) + d / 2.0, h / 2.0, (d - stroke) / 2.0);
            let circle = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(NSPoint::new(cx - r, cy - r), NSSize::new(2.0 * r, 2.0 * r)));
            circle.setLineWidth(stroke);
            black.colorWithAlphaComponent(0.3).setStroke();
            circle.stroke();
            match ring.fraction {
                Some(f) if f >= 0.995 => {
                    black.setStroke();
                    circle.stroke();
                }
                Some(f) if f > 0.005 => {
                    let arc = NSBezierPath::bezierPath();
                    // От 12 часов по часовой стрелке (картинка не перевёрнута: углы — против часовой).
                    arc.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(NSPoint::new(cx, cy), r, 90.0, 90.0 - 360.0 * f, true);
                    arc.setLineWidth(stroke);
                    // Круглые шапки съедали щель: 97–99 выглядели полным кольцом «100». Шапки только до 90%.
                    let cap = if f < 0.9 { objc2_app_kit::NSLineCapStyle::Round } else { objc2_app_kit::NSLineCapStyle::Butt };
                    arc.setLineCapStyle(cap);
                    black.setStroke();
                    arc.stroke();
                }
                _ => {}
            }
            if let Some(text) = &ring.text {
                // «100» — узким шрифтом, чтобы влезло в кольцо; две цифры — обычным.
                let weight = unsafe { objc2_app_kit::NSFontWeightSemibold };
                let font = if text.chars().count() >= 3 {
                    objc2_app_kit::NSFont::systemFontOfSize_weight_width(three_digits, weight, unsafe { objc2_app_kit::NSFontWidthCondensed })
                } else {
                    objc2_app_kit::NSFont::monospacedDigitSystemFontOfSize_weight(two_digits, weight)
                };
                ui::draw::draw_text_center_at(text, cx, cy, &font, &black);
            }
        }
        objc2::runtime::Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &handler);
    image.setTemplate(true);
    image
}

/// Что показывать в строке меню: верхнюю из закреплённых (порядок задаёт человек), а без закреплённых —
/// самую израсходованную. (название, самое тесное окно, все окна).
/// (id, название, самое тесное окно, окна) карточки, что показана в строке меню.
fn tray_pick(app: &state::AppState) -> Option<(String, String, crate::model::RateLimitWindow, Vec<crate::model::RateLimitWindow>)> {
    let usable = |account: &&Account| account.enabled && account.last_usage.as_ref().is_some_and(|u| matches!(u.status, crate::model::FetchStatus::Ok) && u.windows.iter().any(|w| w.used_percent.is_finite()));
    let worst = |account: &Account| {
        account.last_usage.as_ref()?.windows.iter().filter(|w| w.used_percent.is_finite()).max_by(|a, b| a.used_percent.total_cmp(&b.used_percent)).cloned()
    };
    let account = app
        .data
        .accounts
        .iter()
        .filter(usable)
        .filter(|a| a.pinned())
        .min_by(|a, b| a.pin_cmp(b))
        .or_else(|| {
            app.data.accounts.iter().filter(usable).max_by(|a, b| {
                let used = |x: &Account| worst(x).map_or(f64::NEG_INFINITY, |w| w.used_percent);
                // Равенство — как в списке: выше та, что раньше по названию.
                used(a).total_cmp(&used(b)).then_with(|| crate::proxy::control::natural_cmp(&b.label, &a.label))
            })
        })?;
    Some((account.id.clone(), account.label.clone(), worst(account)?, account.last_usage.as_ref()?.windows.clone()))
}

fn update_tray() {
    thread_local! {
        static SHOWN: RefCell<Option<(Vec<(i64, Option<String>)>, u64)>> = const { RefCell::new(None) };
    }
    let (pick, show_numbers, show_remaining) = {
        let app = APP.lock().unwrap_or_else(|e| e.into_inner());
        (tray_pick(&app), app.data.settings.show_tray_percent, app.data.settings.show_remaining)
    };
    let shown = |used: f64| if show_remaining { util::remaining_percent(used) } else { util::clamp_percent(used) };
    let mode = if show_remaining { "осталось" } else { "потрачено" };
    let (rings, mut tip) = match &pick {
        Some((_, label, worst, windows)) => {
            let mut windows: Vec<_> = windows.iter().filter(|w| w.used_percent.is_finite()).take(3).collect();
            // Как в списке: самое тесное окно всегда в кольцах, даже если сервис отдал его четвёртым.
            if !windows.iter().any(|w| w.key == worst.key) {
                if let Some(last) = windows.last_mut() {
                    *last = worst;
                }
            }
            let rings = windows
                .iter()
                .map(|w| {
                    // Как в подсказке и списке: 99,6 — это «99», а не полное кольцо «100».
                    let value = crate::util::display_percent(shown(w.used_percent));
                    TrayRing { fraction: Some(value as f64 / 100.0), text: show_numbers.then(|| value.to_string()) }
                })
                .collect();
            // Подсказка расшифровывает кольца по порядку: какое окно, сколько и когда сброс самого тесного.
            let legend = windows.iter().map(|w| format!("{} {}%", w.label, util::display_percent(shown(w.used_percent)))).collect::<Vec<_>>().join(" · ");
            let reset = util::format_reset_countdown(worst.resets_at, crate::store::now_ms()).map(|r| format!("\nСамое тесное — {}: сброс через {r}", worst.label)).unwrap_or_default();
            (rings, format!("{label} — {mode}\nКольца слева направо: {legend}{reset}"))
        }
        None => (Vec::new(), "SubBar — лимиты подписок: данных пока нет".to_string()),
    };
    if crate::proxy::control::active() {
        tip += "\nСубагенты Claude → OpenCode Go";
    }
    // Подсказка тикает отсчётом раз в минуту — ради неё картинку не перерисовываем.
    let key = (rings.iter().map(|r: &TrayRing| (r.fraction.map_or(-1, |f| (f * 1000.0).round() as i64), r.text.clone())).collect::<Vec<_>>(), TRAY_RING.load(std::sync::atomic::Ordering::Relaxed));
    if SHOWN.with(|cell| cell.borrow().as_ref() == Some(&key)) {
        STATUS_BUTTON.with(|cell| {
            if let Some(button) = cell.borrow().as_ref() {
                if button.toolTip().map(|t| t.to_string()) != Some(tip.clone()) {
                    button.setToolTip(Some(&NSString::from_str(&tip)));
                    button.setAccessibilityLabel(Some(&NSString::from_str(&tip)));
                }
            }
        });
        return;
    }
    let drawn = STATUS_BUTTON.with(|cell| {
        let Some(button) = cell.borrow().as_ref().cloned() else { return false };
        {
            // Всё в картинке — кольца с числами; заголовка нет.
            button.setImage(Some(&tray_icon(&rings)));
            button.setImagePosition(NSCellImagePosition::ImageOnly);
            button.setTitle(&NSString::from_str(""));
            button.setToolTip(Some(&NSString::from_str(&tip)));
            // Числа нарисованы в картинке — VoiceOver без подписи прочёл бы пустую кнопку.
            button.setAccessibilityLabel(Some(&NSString::from_str(&tip)));
        }
        true
    });
    // Значка ещё нет — не помечаем показанным, иначе он так и останется «–».
    if drawn {
        SHOWN.with(|cell| *cell.borrow_mut() = Some(key));
    }
}

/// Окна, уже перешагнувшие порог: их «уже сообщили» (засев при старте и при повышении порога).
fn over_threshold_keys(threshold: f64) -> Vec<String> {
    state::with_app(|app| {
        app.data
            .accounts
            .iter()
            .filter(|a| a.enabled)
            // Как в основном цикле: только свежие данные. Старые окна упавшего опроса «уже сообщили» не делают.
            .filter_map(|a| a.last_usage.as_ref().filter(|u| matches!(u.status, crate::model::FetchStatus::Ok)).map(|u| (a, u)))
            .flat_map(|(a, u)| {
                u.windows
                    .iter()
                    .filter(|w| w.used_percent.is_finite() && util::clamp_percent(w.used_percent) >= threshold)
                    .map(move |w| format!("{}:{}", a.id, w.key))
            })
            .collect()
    })
}

/// При запуске: окна, которые уже были за порогом в прошлый раз, — «уже сообщили». Иначе каждый перезапуск
/// (установка новой версии) присылал бы заново уведомления обо всех исчерпанных ключах.
fn seed_threshold_notices() {
    let threshold = state::with_app(|app| app.data.settings.notify_used_percent);
    if threshold <= 0.0 || !threshold.is_finite() {
        return;
    }
    NOTIFIED_THRESHOLD.with(|t| t.set(threshold));
    let keys = over_threshold_keys(threshold);
    SEEDED.with(|s| *s.borrow_mut() = (keys.iter().cloned().collect(), crate::store::now_ms()));
    NOTIFIED.with(|cell| cell.borrow_mut().extend(keys));
}

fn notify_thresholds() {
    let threshold = APP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .data
        .settings
        .notify_used_percent;
    if threshold <= 0.0 || !threshold.is_finite() {
        NOTIFIED.with(|cell| cell.borrow_mut().clear());
        // 0, а не NaN: NaN — «старт», где уже превышенное глушится. Включение порога — не старт,
        // о том, что уже выше, человек как раз и хочет узнать.
        NOTIFIED_THRESHOLD.with(|t| t.set(0.0));
        return;
    }
    let previous = NOTIFIED_THRESHOLD.with(|t| t.replace(threshold));
    if previous.is_nan() {
        // Старт или порог только что включили: всё, что уже выше, — «уже сообщили».
        let keys = over_threshold_keys(threshold);
        NOTIFIED.with(|cell| *cell.borrow_mut() = keys.into_iter().collect());
    } else if previous > threshold {
        // Порог опустили: о том, что было выше прежнего, уже сообщали — молчим. А окна, впервые
        // оказавшиеся выше нового порога, — ровно то, ради чего его опускали: о них сообщаем.
        let keys = over_threshold_keys(previous);
        NOTIFIED.with(|cell| cell.borrow_mut().extend(keys));
    } else if previous < threshold {
        // Порог подняли: окна ниже нового порога ещё не «сообщены» по нему — иначе после
        // временного понижения они молчали бы, пока расход не упадёт на 10 пунктов.
        let still: HashSet<String> = over_threshold_keys(threshold).into_iter().collect();
        NOTIFIED.with(|cell| cell.borrow_mut().retain(|k| still.contains(k)));
    }
    let mut to_notify: Vec<String> = Vec::new();
    {
        let app = APP.lock().unwrap_or_else(|e| e.into_inner());
        // Удалённые карточки — из «уже сообщили»: иначе набор рос бы, пока приложение открыто.
        let ids: HashSet<&str> = app.data.accounts.iter().map(|a| a.id.as_str()).collect();
        let alive = |k: &String| k.split_once(':').is_some_and(|(id, _)| ids.contains(id));
        NOTIFIED.with(|cell| cell.borrow_mut().retain(|k| alive(k)));
        SEEDED.with(|s| s.borrow_mut().0.retain(|k| alive(k)));
        let show_remaining = app.data.settings.show_remaining;
        for account in app.data.accounts.iter().filter(|a| a.enabled) {
            let Some(usage) = &account.last_usage else {
                continue;
            };
            if !matches!(usage.status, crate::model::FetchStatus::Ok) {
                continue;
            }
            for window in &usage.windows {
                if !window.used_percent.is_finite() {
                    continue;
                }
                let used = util::clamp_percent(window.used_percent);
                let key = format!("{}:{}", account.id, window.key);
                let seeded_stale = SEEDED.with(|s| {
                    let mut s = s.borrow_mut();
                    let since = s.1;
                    usage.updated_at > since && s.0.remove(&key)
                });
                let fresh = NOTIFIED.with(|cell| {
                    let mut notified = cell.borrow_mut();
                    if seeded_stale && used < threshold {
                        notified.remove(&key);
                    }
                    if used >= threshold {
                        if notified.contains(&key) {
                            false
                        } else {
                            notified.insert(key.clone());
                            true
                        }
                    } else {
                        // Гистерезис: сброс, когда расход упал на 10 пунктов ниже порога,
                        // но не требуем невозможного отрицательного значения.
                        let reset_at = (threshold - 10.0).max(threshold / 2.0);
                        if used <= reset_at {
                            notified.remove(&key);
                        }
                        false
                    }
                });
                if fresh {
                    // То же соглашение, что в трее и списке: «осталось» или «потрачено».
                    to_notify.push(if show_remaining {
                        format!("{}: осталось {}% на окне {}", account.label, util::display_percent(util::remaining_percent(used)), window.label)
                    } else {
                        format!("{}: потрачено {}% на окне {}", account.label, util::display_percent(used), window.label)
                    });
                }
            }
        }
    }
    show_batched(to_notify);
}

/// Что уже сообщали про прокси — чтобы не повторяться.
#[derive(Default)]
struct ProxySeen {
    /// None — статуса ещё не видели; Some(None) — видели, но прокси не назвал pid.
    pid: Option<Option<u64>>,
    paused: HashSet<String>,
    /// Когда последний раз сообщали про ключ (подпись → мс): не чаще раза в 30 минут.
    key_notice: std::collections::HashMap<String, i64>,
    fallback: u64,
    last_fallback_notice: i64,
}

/// Уведомления про прокси: ключ ушёл на долгую паузу (кончился лимит, ключ отвергнут) и куда делись
/// субагенты; короткие паузы (429 на секунды) — не повод. Про один ключ и про откаты в Claude —
/// не чаще раза в 30 минут. Что было до запуска окна — не повторяем.
fn proxy_notifications(status: Option<&serde_json::Value>) {
    let Some(s) = status else { return };
    let cfg = crate::proxy::control::load_config();
    let now = crate::store::now_ms();
    let last = s["keys"]["lastUsed"].as_str().unwrap_or("");
    let paused: Vec<(String, String, String)> = s["keys"]["paused"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["untilMs"].as_i64().unwrap_or(0).saturating_sub(now) >= 9 * 60_000) // 401/403 ставят ровно 10 мин — к опросу остаётся чуть меньше
        .map(|p| {
            let label = p["label"].as_str().unwrap_or("").to_string();
            // Срок паузы — момент сброса лимита: после перезапуска прокси он тот же с точностью до минуты.
            let id = format!("{label}@{}", p["untilMs"].as_i64().unwrap_or(0) / 60_000);
            (id, label, p["reason"].as_str().unwrap_or("ключ на паузе").to_string())
        })
        .collect();
    let fallback = s["stats"]["fallback"].as_u64().unwrap_or(0);
    let mut notes = Vec::new();
    PROXY_SEEN.with(|cell| {
        let mut seen = cell.borrow_mut();
        let pid = Some(s["pid"].as_u64());
        if seen.pid != pid {
            // Первый статус или прокси перезапустился: счётчики с нуля, прежнее — уже известно.
            let first = seen.pid.is_none();
            seen.pid = pid;
            seen.fallback = fallback;
            // Паузы прокси живут в памяти: после перезапуска новая пауза — новое событие.
            seen.key_notice.clear();
            if first {
                seen.paused.extend(paused.iter().map(|p| p.0.clone()));
                return;
            }
        }
        // Снятые паузы больше не нужны: id несёт срок, и без чистки набор рос бы с каждой паузой.
        seen.paused.retain(|id| paused.iter().any(|p| &p.0 == id));
        for (id, label, reason) in &paused {
            let recent = seen.key_notice.get(label).is_some_and(|at| (now - at).abs() < 30 * 60_000); // abs: перевод часов назад не глушит навсегда
            if seen.paused.insert(id.clone()) && !recent {
                seen.key_notice.insert(label.clone(), now);
                let tail = if cfg.rotate && !last.is_empty() && last != label {
                    format!("Субагенты перешли на {last}.")
                } else if cfg.rotate {
                    // Запасной ключ ещё отвечает — имя узнаем позже; не пугать «идут в Claude».
                    "Субагенты переходят на запасной ключ.".to_string()
                } else if cfg.fallback {
                    "Субагенты идут в Claude (подписка).".to_string()
                } else {
                    "Субагенты получают ошибку.".to_string()
                };
                notes.push(format!("{label}: {reason}. {tail}"));
            }
        }
        if fallback > seen.fallback {
            seen.fallback = fallback;
            if (now - seen.last_fallback_notice).abs() > 30 * 60_000 {
                seen.last_fallback_notice = now;
                let why = s["stats"]["recent"]
                    .as_array()
                    .and_then(|r| r.iter().rev().find(|e| e["route"] == "fallback"))
                    .and_then(|e| e["note"].as_str())
                    .unwrap_or("причина уже выпала из журнала");
                notes.push(format!("Запрос ушёл в Claude (подписка): {why}"));
            }
        }
    });
    show_batched(notes);
}

/// Пачку в одно уведомление: при волне субагентов иначе десятки osascript разом.
fn show_batched(notes: Vec<String>) {
    if notes.len() > 3 {
        // Баннер показывает две строки: первые три пункта и счёт остальных, а не простыню.
        let n = notes.len();
        let head = notes[..3].join(" · ");
        show_notification(&format!("{head} · и ещё {}", crate::util::plural((n - 3) as u64, "событие", "события", "событий")));
    } else if !notes.is_empty() {
        // Одним потоком по очереди: три osascript разом приходили в случайном порядке, а баннеры наползали.
        std::thread::spawn(move || {
            for (i, note) in notes.iter().enumerate() {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
                run_notification(note);
            }
        });
    }
}

fn show_notification(message: &str) {
    // В фоне — osascript идёт 100–300 мс, окно не блокируем.
    let message = message.to_string();
    std::thread::spawn(move || run_notification(&message));
}

fn run_notification(message: &str) {
    let escaped = applescript_string_contents(message);
    let script = format!("display notification \"{escaped}\" with title \"SubBar\"");
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn applescript_string_contents(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            character if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') => {
                escaped.push(' ')
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

fn tick() {
    let events_changed = state::flush_events();
    poll_detection_results();
    // Подхватываем добавления и удаления из CLI, даже без событий от рабочих потоков.
    let (disk_changed, status_expired) = {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        let changed = app.sync_external();
        (changed, app.expire_status())
    };
    let (screen, status) = state::with_app(|app| (app.screen.clone(), app.status_line.clone()));
    // Статус прокси сменился (опрос в фоне) — строка меню, ⇄ в шапке, экран «Субагенты».
    let (proxy_gen, proxy_status) = crate::proxy::control::cached();
    if PROXY_GEN.with(|g| g.replace(proxy_gen)) != proxy_gen {
        proxy_notifications(proxy_status.as_ref());
        update_tray();
        if screen == Screen::Proxy {
            let cfg = crate::proxy::control::load_config();
            PROXY_VIEW.with(|cell| {
                if let Some(view) = cell.borrow().as_ref() {
                    view.set_status(proxy_status.as_ref(), &cfg);
                }
            });
            // Паузы ключей и «в работе» — из статуса прокси.
            refresh_proxy_keys(&cfg);
        }
        PANEL.with(|cell| {
            if let Some(panel) = cell.borrow().as_ref() {
                panel.setNeedsDisplay(true);
            }
        });
    }
    // Отсчёты «сброс через …» и «ещё Nч» тикают сами — обновлять и без новых данных, раз в 30 с.
    static PROXY_CLOCK: AtomicI64 = AtomicI64::new(0);
    let now_ms = crate::store::now_ms();
    let clock_due = screen == Screen::Proxy && (now_ms - PROXY_CLOCK.load(Ordering::Relaxed)).abs() >= 30_000; // и после перевода часов
    let cfg = (screen == Screen::Proxy && (events_changed || disk_changed || clock_due)).then(crate::proxy::control::load_config);
    if let (true, Some(cfg)) = (clock_due, &cfg) {
        PROXY_CLOCK.store(now_ms, Ordering::Relaxed);
        PROXY_VIEW.with(|cell| {
            if let Some(view) = cell.borrow().as_ref() {
                view.set_status(proxy_status.as_ref(), cfg);
                // «(N мин назад)» у итога проверки тоже стареет.
                if let (_, Some((text, ok, at))) = crate::proxy::control::check_result() {
                    view.set_check(&crate::proxy::control::check_text(&text, at, now_ms), Some(ok));
                }
            }
        });
    }
    if let Some(cfg) = &cfg {
        // Пришли свежие лимиты (или карточка OpenCode добавилась) — строки ключей заново; «ещё 1д 3ч» тикает.
        refresh_proxy_keys(cfg);
    }
    if screen == Screen::Proxy {
        // Свои сообщения экрана (служба, перезапуск) — не общая строка статуса окна.
        // Окно закрыто — сообщение подождёт (до 10 минут, см. take_notice), а не уйдёт в невидимый экран.
        let shown = POPOVER.with(|c| c.borrow().as_ref().is_some_and(|p| p.isShown()));
        if shown {
            if let Some((text, error)) = crate::proxy::control::take_notice() {
                proxy_message(&text, error);
                // Установка не удалась — тумблер «служба» вернуть к правде.
                PROXY_VIEW.with(|cell| {
                    if let Some(view) = cell.borrow().as_ref() {
                        view.sync_service(crate::proxy::control::service_intent());
                        // И тумблер строки: правка settings.json могла не пройти.
                        view.sync_statusline(crate::proxy::control::statusline_intent());
                    }
                });
            }
        }
        // Итог проверки связи пришёл из фона.
        let (check_gen, result) = crate::proxy::control::check_result();
        if let Some((text, ok, at)) = result {
            if CHECK_SHOWN.with(|c| c.replace((check_gen, true))) != (check_gen, true) {
                PROXY_VIEW.with(|cell| {
                    if let Some(view) = cell.borrow().as_ref() {
                        view.set_check(&crate::proxy::control::check_text(&text, at, crate::store::now_ms()), Some(ok));
                    }
                });
            }
        }
    }
    if screen == Screen::Settings {
        SETTINGS_VIEW.with(|view| {
            if let Some(view) = view.borrow().as_ref() {
                let is_error = status.as_deref().is_some_and(|message| {
                    let m = message.to_lowercase();
                    let m = m.replace('ё', "е");
                    ["ошибка", "не удалось", "не удался", "не сохранен", "поврежден", "пропал", "битая"].iter().any(|needle| m.contains(needle))
                });
                view.set_status_message(status.as_deref().unwrap_or(""), is_error);
            }
        });
    }
    // На списке те же отсчёты («сброс через …», «N мин назад») и окраска по времени — перерисовать раз в 30 с.
    static LIST_CLOCK: AtomicI64 = AtomicI64::new(0);
    let list_due = screen == Screen::List && (now_ms - LIST_CLOCK.load(Ordering::Relaxed)).abs() >= 30_000;
    if list_due {
        LIST_CLOCK.store(now_ms, Ordering::Relaxed);
    }
    if events_changed || disk_changed || status_expired || list_due {
        refresh_panel();
        notify_thresholds();
    }
    update_tray();
    let seconds = APP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .data
        .settings
        .refresh_seconds;
    if seconds > 0 {
        let now = crate::store::now_ms();
        let last = AUTO_REFRESH_LAST.load(Ordering::Relaxed);
        let busy = APP.lock().unwrap_or_else(|e| e.into_inner()).refreshing;
        // Обновление уже идёт — автообновление пропускаем, волны не копим.
        // abs: перевод часов назад не должен усыплять таймер на час. Метку ставит сам refresh_all.
        // Нет включённых карточек — refresh_all не ставит метку и затирал бы строку статуса каждый тик.
        let any = APP.lock().unwrap_or_else(|e| e.into_inner()).data.accounts.iter().any(|a| a.enabled);
        if (now - last).abs() >= seconds as i64 * 1000 && !busy && any {
            state::refresh_all();
        }
    }
}

pub fn apply_settings() {
    update_tray();
    let login = APP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .data
        .settings
        .launch_at_login;
    apply_login_item(login);
}

fn apply_login_item(enabled: bool) {
    let desired = i64::from(enabled);
    if LOGIN_ITEM_APPLIED.load(Ordering::Relaxed) == desired {
        // Вернулись к применённому: прошлый отказ больше не про это желание — следующий клик
        // «включить» должен снова попробовать, а не молча упереться в старую отметку.
        LOGIN_ITEM_FAILED.store(-1, Ordering::Relaxed);
        LOGIN_ITEM_REVISION.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // Упавшую попытку с тем же желанием не повторяем на каждую правку других настроек:
    // иначе каждая смена порога или интервала — снова launchctl и уведомление об ошибке.
    if LOGIN_ITEM_FAILED.swap(desired, Ordering::Relaxed) == desired {
        return;
    }
    let revision = LOGIN_ITEM_REVISION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    // launchctl — не на главном потоке: он идёт сотни мс. Запросы по очереди,
    // а изменения галочки, которые обогнало новое, отбрасываем.
    std::thread::spawn(move || {
        // Отравленный мьютекс — не повод молча выключить автозапуск навсегда: забираем его изнутри.
        let _guard = LOGIN_ITEM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if LOGIN_ITEM_REVISION.load(Ordering::Relaxed) != revision {
            return;
        }
        match configure_login_item(enabled) {
            Ok(()) => {
                LOGIN_ITEM_APPLIED.store(desired, Ordering::Relaxed);
                LOGIN_ITEM_FAILED.store(-1, Ordering::Relaxed);
            }
            Err(error) => {
                // Настоящее состояние неизвестно (выключение могло пройти наполовину) — следующий клик
                // не должен упереться в ранний выход «уже так».
                LOGIN_ITEM_APPLIED.store(-1, Ordering::Relaxed);
                eprintln!("[subbar] не удалось обновить автозапуск: {error}");
                let _ = state::sender().send(state::WorkerEvent::Notice(format!(
                    "Не удалось обновить автозапуск: {error}"
                )));
            }
        }
    });
}

fn configure_login_item(enabled: bool) -> std::io::Result<()> {
    const LABEL: &str = "ai.subbar";
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME не задан"))?;
    let agents = PathBuf::from(home).join("Library").join("LaunchAgents");
    let plist_path = agents.join(format!("{LABEL}.plist"));
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let target = format!("{domain}/{LABEL}");

    if !enabled {
        // Сначала выключаем, потом удаляем plist: так следующие входы в систему
        // его не запустят, а текущий процесс SubBar продолжит работать.
        // Незагруженная служба на disable отвечает ошибкой — plist всё равно убрать, иначе
        // на следующем входе macOS запустит SubBar при выключенном тумблере.
        // Plist нет — выключать нечего: не дёргаем launchctl на каждом запуске.
        if !plist_path.exists() {
            return Ok(());
        }
        let _ = launchctl_succeeded(&["disable", &target])?;
        match fs::remove_file(&plist_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        return Ok(());
    }

    fs::create_dir_all(&agents)?;
    let executable = std::env::current_exe()?;
    let executable_xml = xml_text(&executable.to_string_lossy());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{executable_xml}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
</dict>
</plist>
"#
    );

    let launch_service_matches = launch_agent_matches_executable(&target, &executable)?;
    if !launch_service_matches && launchctl_succeeded(&["print", &target])? {
        // Служба может остаться загруженной после переноса приложения. Меняем её
        // описание запуска до bootstrap пути, который используется сейчас.
        if !launchctl_succeeded(&["bootout", &target])? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "launchctl bootout вернул ошибку",
            ));
        }
    }

    write_plist_atomically(&plist_path, plist.as_bytes())?;
    if !launchctl_succeeded(&["enable", &target])? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "launchctl enable вернул ошибку",
        ));
    }
    if launch_service_matches {
        return Ok(());
    }
    if !launchctl_succeeded(&["bootstrap", &domain, plist_path.to_string_lossy().as_ref()])? {
        // Другой источник мог загрузить ту же службу между запросом статуса
        // и bootstrap; успехом считаем, только если она загружена.
        if !launchctl_succeeded(&["print", &target])? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "launchctl bootstrap вернул ошибку",
            ));
        }
    }
    Ok(())
}

fn launch_agent_matches_executable(
    target: &str,
    executable: &std::path::Path,
) -> std::io::Result<bool> {
    let output = std::process::Command::new("/bin/launchctl")
        .args(["print", target])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let description = String::from_utf8_lossy(&output.stdout);
    let executable = executable.to_string_lossy();
    let current_pid = format!("pid = {}", std::process::id());
    // Проверка pid не даёт выгрузить само приложение, когда пользователь
    // выключает и снова включает автозапуск в одной сессии.
    // Целыми строками: «pid = 1234» — подстрока «pid = 12345», путь — подстрока «…-helper».
    let lines: Vec<&str> = description.lines().map(str::trim).collect();
    // Бинарь в выводе launchctl print — «program = …» или отдельной строкой в arguments.
    let program_line = format!("program = {executable}");
    Ok(lines.iter().any(|l| *l == current_pid || *l == program_line || *l == executable.as_ref()))
}

fn launchctl_succeeded(arguments: &[&str]) -> std::io::Result<bool> {
    let status = std::process::Command::new("/bin/launchctl")
        .args(arguments)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(status.success())
}

fn write_plist_atomically(path: &PathBuf, contents: &[u8]) -> std::io::Result<()> {
    let mut temporary = path.clone();
    temporary.set_extension(format!("plist.{}.tmp", std::process::id()));
    // Залипший tmp от прошлого сбоя с тем же pid иначе ломает create_new навсегда.
    let _ = std::fs::remove_file(&temporary);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn xml_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\r' => escaped.push_str("&#13;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn quit() {
    // Несохранённое при выходе не бросаем молча: первый ⌘Q при сбое записи выход отменяет и говорит об этом,
    // второй — выходит как есть (иначе приложение нельзя было бы закрыть при полном диске).
    // Когда предупредили: второй ⌘Q в течение минуты выходит, а позже — снова предупреждение (правки могли быть новые).
    // Свой счётчик на каждое предупреждение: иначе второй ⌘Q после «битого порога» молча выходил с несохранённым.
    static WARNED: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    static WARNED_PERCENT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    // Вписанное в открытые настройки, но не подтверждённое Enter, при выходе не теряем.
    if SETTINGS_VIEW.with(|v| v.borrow().is_some()) {
        SETTINGS_VIEW.with(|v| {
            if let Some(w) = v.borrow().as_ref().and_then(|s| s.view.window()) {
                w.makeFirstResponder(None);
            }
        });
        // Битый порог молча не выбрасываем: говорим и не выходим. Остальные переключатели
        // сохраняем сразу, как при закрытии настроек.
        apply_settings_form_with(false);
        let valid = SETTINGS_VIEW.with(|v| v.borrow().as_ref().is_none_or(|s| s.notify_percent().is_some()));
        // Порог починили — следующая ошибка снова заслуживает предупреждения, а не тихого выхода.
        if valid {
            WARNED_PERCENT.store(0, std::sync::atomic::Ordering::SeqCst);
        }
        if !valid && (crate::store::now_ms() - WARNED_PERCENT.swap(crate::store::now_ms(), std::sync::atomic::Ordering::SeqCst)).abs() > 60_000 {
            SETTINGS_VIEW.with(|v| {
                if let Some(v) = v.borrow().as_ref() {
                    v.set_notify_message("Введи число от 0 до 100 — повторный выход пройдёт без этой правки", true);
                }
            });
            return;
        }
        apply_settings_form();
    }
    {
        let mut app = APP.lock().unwrap_or_else(|e| e.into_inner());
        app.flush_save_now();
        let now = crate::store::now_ms();
        // Запись удалась — счёт «уже предупредили» обнуляется: новые правки снова спросят.
        let warned = if app.dirty { WARNED.swap(now, std::sync::atomic::Ordering::SeqCst) } else { WARNED.swap(0, std::sync::atomic::Ordering::SeqCst) };
        if app.dirty && (now - warned).abs() > 60_000 {
            app.set_status_for("Изменения не сохранены — выход отменён. ⌘Q ещё раз выйдет без них".to_string(), 60_000);
            return;
        }
    }
    let Some(mtm) = MainThreadMarker::new() else {
        std::process::exit(0);
    };
    NSApplication::sharedApplication(mtm).terminate(None);
}

/* ------------------------------------------------------------------ */
/* Bootstrap                                                           */
/* ------------------------------------------------------------------ */

/// Не даём двум копиям драться за значок в строке меню.
pub fn acquire_single_instance() -> bool {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = crate::store::data_dir();
    // Каталог с ключами и lock-файл — только владельцу: права по умолчанию (755/644) тут лишние.
    if let Err(error) = fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir) {
        eprintln!("[subbar] не могу создать каталог данных: {error}");
        return false;
    }
    let lock_path = dir.join("limitbar.lock");
    let file = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!("[subbar] не могу открыть lock-файл: {error}");
            return false;
        }
    };
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    loop {
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            eprintln!("[subbar] не удалось заблокировать lock-файл: {error}");
        }
        return false;
    }
    // Держим дескриптор открытым всю жизнь процесса; flock ОС
    // снимет сама при выходе.
    match SINGLE_INSTANCE_LOCK.set(file) {
        Ok(()) => true,
        Err(_) => {
            eprintln!("[subbar] блокировка второго экземпляра уже установлена");
            false
        }
    }
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("SubBar запускается только на главном потоке");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // Тема одна — светлая (решение 26.09): и нарисованное, и нативные контролы светлые при любой системной.
    app.setAppearance(objc2_app_kit::NSAppearance::appearanceNamed(unsafe { objc2_app_kit::NSAppearanceNameAqua }).as_deref());

    let controller = UiController::new(mtm);
    CONTROLLER.with(|cell| *cell.borrow_mut() = Some(controller.clone()));
    // Список может быть выше экранов с формами — сколько позволяет экран; кольца значка — по высоте строки меню.
    sync_screen_metrics(mtm);

    let panel = PanelView::new(mtm);
    PANEL.with(|cell| *cell.borrow_mut() = Some(panel.clone()));

    // Контейнер перевёрнут, как и панель: начало — сверху. Высоту панели ставим сами (resize_popover):
    // растягивание по высоте вычитало бы разницу второй раз, и экран после смены высоты
    // (список 760 → «Субагенты» 580) съезжал вниз с пустотой сверху.
    let container = crate::ui::kit::FlippedView::new(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(PANEL_WIDTH, PANEL_HEIGHT),
        ),
    );
    container.addSubview(&panel);
    panel.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
    panel.setFrame(container.bounds());

    let view_controller = NSViewController::new(mtm);
    view_controller.setView(&container);

    let popover = NSPopover::new(mtm);
    popover.setBehavior(NSPopoverBehavior::Semitransient);
    popover.setContentSize(NSSize::new(PANEL_WIDTH, PANEL_HEIGHT));
    popover.setContentViewController(Some(&view_controller));
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &controller,
            sel!(onPopoverWillClose:),
            Some(NSPopoverWillCloseNotification),
            Some(&popover),
        );
    }
    POPOVER.with(|cell| *cell.borrow_mut() = Some(popover));

    let status_bar = NSStatusBar::systemStatusBar();
    let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status_item.button(mtm) {
        // Кольца с числами вместо строки «5ч 100% 7д 41% 30д 21%»: та пряталась за вырезом экрана.
        button.setImage(Some(&tray_icon(&[])));
        button.setImagePosition(NSCellImagePosition::ImageOnly);
        // На экране с низкой строкой меню (внешний монитор) — уменьшить, а не обрезать.
        button.setImageScaling(objc2_app_kit::NSImageScaling::ScaleProportionallyDown);
        unsafe {
            button.setTarget(Some(&controller));
            button.setAction(Some(sel!(onTogglePopover:)));
        }
        STATUS_BUTTON.with(|cell| *cell.borrow_mut() = Some(button));
    }

    unsafe {
        let timer = objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.75,
            &controller,
            sel!(onTick:),
            None,
            true,
        );
        // И в common modes: иначе пока открыто контекстное меню карточки, тик (обновление, сохранение) стоит.
        objc2_foundation::NSRunLoop::mainRunLoop().addTimer_forMode(&timer, objc2_foundation::NSRunLoopCommonModes);
    }

    crate::proxy::control::start_poller();
    seed_threshold_notices();
    crate::bootstrap::ensure_claude_account();
    sync_content();
    // Пересобираем plist от бинаря, который запущен сейчас, чтобы
    // перенесённое приложение не указывало на старый путь установки.
    apply_settings();
    // Отмечаем старт, чтобы первый тик не обновил всё второй раз.
    AUTO_REFRESH_LAST.store(crate::store::now_ms(), Ordering::Relaxed);
    state::refresh_all();

    // Отладка: сразу открыть нужный экран (для скриншотов и тестов).
    match std::env::var("LIMITBAR_SCREEN").as_deref() {
        Ok("form") => open_form(None),
        Ok("settings") => open_settings(),
        Ok("proxy") => open_proxy(),
        _ => {}
    }

    if std::env::var("LIMITBAR_OPEN_ON_START").as_deref() == Ok("1") {
        unsafe {
            objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.6,
                &controller,
                sel!(onShowOnStart:),
                None,
                false,
            );
        }
    }

    app.run();
}
