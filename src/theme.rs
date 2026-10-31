//! Look and feel: Adwaita (dark by default) plus a small stylesheet with an orange accent.

use gtk::prelude::*;

use crate::redfish::Health;

/// Status colours, readable on both the dark and light themes.
pub fn health_color(h: Health) -> &'static str {
    match h {
        Health::Ok => "#3fb950",
        Health::Warning => "#e3b341",
        Health::Critical => "#ff4d4f",
        Health::Unknown => "#8b949e",
    }
}

/// "● Critical" in the health colour, as Pango markup.
pub fn health_markup(h: Health) -> String {
    dot_markup(health_color(h), h.label())
}

pub fn dot_markup(color: &str, label: &str) -> String {
    format!(
        "<span foreground='{color}'>●</span> {}",
        gtk::glib::markup_escape_text(label)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Theme {
    /// Adwaita dark with the app's own styling (default).
    Dark,
    /// Adwaita light with the app's own styling.
    Light,
    /// Whatever GTK theme the desktop uses; only the accent styling is added.
    System,
}

impl Theme {
    pub fn parse(s: &str) -> Option<Theme> {
        match s.trim().to_lowercase().as_str() {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            "system" => Some(Theme::System),
            _ => None,
        }
    }
}

/// Shared by every theme: accent colour, tabs, buttons, search box.
const BASE_CSS: &str = r#"
@define-color accent #e76125;
@define-color accent_hover #f07a43;
@define-color danger #d21419;

* { font-family: "Roboto", "Noto Sans", "Cantarell", sans-serif; }

headerbar .title { font-weight: 700; }
headerbar .subtitle { color: @accent; opacity: 1; }

notebook > header.top { padding: 0 6px; }
notebook > header.top tab {
    padding: 8px 14px;
    min-height: 22px;
    border: none;
    border-radius: 0;
    box-shadow: none;
}
notebook > header.top tab:checked { box-shadow: inset 0 -3px @accent; }
notebook > header.top tab button {
    min-width: 20px; min-height: 20px; padding: 0; margin-left: 4px;
    border-radius: 10px;
}
notebook > header.top tab button:hover { background: alpha(@danger, 0.85); color: white; }

button.suggested-action {
    background-image: none;
    background-color: @accent;
    border-color: shade(@accent, 0.8);
    color: white;
    text-shadow: none;
    -gtk-icon-shadow: none;
}
button.suggested-action:hover { background-color: @accent_hover; }
button.suggested-action:disabled { background-color: alpha(@accent, 0.35); color: alpha(white, 0.6); }

entry.search-pill {
    border-radius: 18px;
    padding: 6px 14px;
    min-height: 26px;
}
entry.search-pill:focus { border-color: @accent; box-shadow: inset 0 0 0 1px @accent; }

treeview.view:selected,
treeview.view:selected:focus { background-color: @accent; color: white; }
treeview.view header button { padding: 6px 8px; font-weight: 600; }

button.destructive-action {
    background-image: none;
    background-color: alpha(@danger, 0.85);
    border-color: shade(@danger, 0.7);
    color: white;
    text-shadow: none;
}
button.destructive-action:hover { background-color: @danger; }
.section-title { font-weight: 700; font-size: 1.05em; }
.health-overall { margin-bottom: 2px; }
paned > separator { min-height: 8px; background: none; }

.app-toast {
    padding: 8px 10px 8px 16px;
    border-radius: 10px;
    background-color: #2b3035;
    color: #eceeef;
    border: 1px solid alpha(@accent, 0.6);
    box-shadow: 0 4px 16px alpha(black, 0.45);
}
.app-toast button { color: #eceeef; }

.manager-footer { font-size: 0.9em; opacity: 0.6; }
.empty-title { font-size: 1.6em; font-weight: 300; }
.empty-hint { opacity: 0.6; }
.empty-icon { color: @accent; }
.connecting-status { font-size: 1.15em; }
.error-message { font-family: monospace; }
checkbutton check:checked, radiobutton radio:checked {
    background-image: none; background-color: @accent; border-color: shade(@accent, 0.8); color: white;
}
"#;

/// Dark surfaces, a little softer than stock Adwaita dark.
const DARK_CSS: &str = r#"
window.background, dialog .dialog-vbox, notebook > stack { background-color: #1d2023; }
headerbar { background-image: none; background-color: #16181b; border-color: #0f1012; box-shadow: none; }
notebook > header.top { background-color: #16181b; border-color: #0f1012; }
notebook > header.top tab { color: #9aa2a8; }
notebook > header.top tab:hover { color: #e6e8ea; background-color: #22262a; }
notebook > header.top tab:checked { color: #ffffff; background-color: #1d2023; }
scrolledwindow.frame, scrolledwindow { border-color: #2c3136; }
treeview.view { background-color: #212528; color: #dfe3e6; }
treeview.view:hover { background-color: #2a2f33; }
treeview.view header button { background-image: none; background-color: #1a1d20; color: #9aa2a8; border-color: #2c3136; }
entry { background-color: #262a2e; border-color: #353b40; color: #eceeef; }
textview text { background-color: #262a2e; color: #eceeef; }
"#;

pub fn apply(theme: Theme) {
    if let Some(settings) = gtk::Settings::default() {
        match theme {
            Theme::Dark | Theme::Light => {
                settings.set_gtk_theme_name(Some("Adwaita"));
                settings.set_gtk_application_prefer_dark_theme(theme == Theme::Dark);
            }
            Theme::System => {}
        }
    }
    let css = match theme {
        Theme::Dark => format!("{BASE_CSS}{DARK_CSS}"),
        Theme::Light | Theme::System => BASE_CSS.to_string(),
    };
    let provider = gtk::CssProvider::new();
    if let Err(e) = provider.load_from_data(css.as_bytes()) {
        eprintln!("rustdrac: stylesheet: {e}");
        return;
    }
    if let Some(screen) = gtk::gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
