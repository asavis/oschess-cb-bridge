//! The tray mark: the oschess logo whose own colour is the bridge's state,
//! a tooltip that names the state, and the menu of a right click.

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::ManagerExt;

use super::{commands, shared, windows};
use crate::i18n::Strings;
use crate::status::{Theme, View, icon_file, icon_size};

const TRAY: &str = "main";
const AUTOSTART: &str = "autostart";

/// The drawn marks, by file name (`icons/generate.py` draws them).
macro_rules! marks {
    ($($name:literal),* $(,)?) => {
        fn mark(name: &str) -> Option<&'static [u8]> {
            match name {
                $($name => Some(include_bytes!(concat!("../../icons/tray/", $name))),)*
                _ => None,
            }
        }
    };
}

marks!(
    "light-ready-16.png",
    "light-ready-20.png",
    "light-ready-24.png",
    "light-ready-32.png",
    "light-attention-16.png",
    "light-attention-20.png",
    "light-attention-24.png",
    "light-attention-32.png",
    "light-problem-16.png",
    "light-problem-20.png",
    "light-problem-24.png",
    "light-problem-32.png",
    "dark-ready-16.png",
    "dark-ready-20.png",
    "dark-ready-24.png",
    "dark-ready-32.png",
    "dark-attention-16.png",
    "dark-attention-20.png",
    "dark-attention-24.png",
    "dark-attention-32.png",
    "dark-problem-16.png",
    "dark-problem-20.png",
    "dark-problem-24.png",
    "dark-problem-32.png",
);

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let shared = shared(app);
    let strings = &shared.strings;
    let view = shared.view();
    let autostart = app.autolaunch().is_enabled().unwrap_or(false);
    let item = |id: &str, key: &str| MenuItem::with_id(app, id, strings.get(key), true, None::<&str>);
    let tick = CheckMenuItem::with_id(app, AUTOSTART, strings.get("menu.autostart"), true, autostart, None::<&str>)?;
    app.manage(AutostartTick(tick.clone()));
    let menu = Menu::with_items(
        app,
        &[
            &item("open", "menu.open")?,
            &item("settings", "menu.settings")?,
            &item("code", "menu.code")?,
            &PredefinedMenuItem::separator(app)?,
            &tick,
            // Updates come with the installer (#23, part 2b).
            &MenuItem::with_id(app, "update", strings.get("menu.update"), false, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &item("quit", "menu.quit")?,
        ],
    )?;
    let mut builder = TrayIconBuilder::with_id(TRAY)
        .tooltip(view.tooltip(strings))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left, button_state: MouseButtonState::Up, rect, ..
            } = event
            {
                windows::toggle_flyout(tray.app_handle(), rect);
            }
        });
    if let Some(image) = image(&icon_file(super::system::taskbar_theme(), view.tray(), 16)) {
        builder = builder.icon(image);
    }
    builder.build(app)?;
    Ok(())
}

fn image(file: &str) -> Option<Image<'static>> {
    mark(file).and_then(|bytes| Image::from_bytes(bytes).ok())
}

/// Redraws the mark and the tooltip when they differ from `drawn`, and returns
/// what is drawn now.
pub fn update(app: &AppHandle, strings: &Strings, view: &View, theme: Theme, scale: f64, drawn: &str) -> String {
    let file = icon_file(theme, view.tray(), icon_size(scale));
    let tooltip = view.tooltip(strings);
    let key = format!("{file}\n{tooltip}");
    if key == drawn {
        return key;
    }
    let Some(tray) = app.tray_by_id(TRAY) else { return drawn.to_string() };
    if let Some(image) = image(&file) {
        let _ = tray.set_icon(Some(image));
    }
    let _ = tray.set_tooltip(Some(tooltip));
    key
}

fn on_menu(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "open" => commands::open_oschess_now(app),
        "settings" => {
            let _ = windows::open_settings(app, "databases");
        }
        "code" => {
            let _ = windows::open_settings(app, "code");
        }
        AUTOSTART => {
            let on = !app.autolaunch().is_enabled().unwrap_or(false);
            let _ = commands::switch_autostart(app, on);
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

/// The menu's autostart tick, kept so that the settings window can move it.
struct AutostartTick(CheckMenuItem<Wry>);

/// Puts the menu's autostart tick in step with the Run key.
pub fn show_autostart(app: &AppHandle, on: bool) {
    if let Some(tick) = app.try_state::<AutostartTick>() {
        let _ = tick.0.set_checked(on);
    }
}
