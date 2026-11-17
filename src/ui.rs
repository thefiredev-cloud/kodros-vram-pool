//! Status page at `/` and `/status`. Static assets are compiled in. Colours are never typed into the
//! page: `/theme.css` is built on every request from the live Omarchy theme
//! (`~/.local/state/omarchy/current/theme/colors.toml`), so the page follows the hourly theme change.

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

const HTML: &str = include_str!("../assets/status.html");
const CSS: &str = include_str!("../assets/status.css");
const JS: &str = include_str!("../assets/status.js");

/// No inline script, no foreign origin: the page may only talk to this server.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

fn asset(content_type: &'static str, body: String) -> Response {
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    r
}

pub async fn page() -> Response {
    asset("text/html; charset=utf-8", HTML.to_owned())
}

pub async fn css() -> Response {
    asset("text/css; charset=utf-8", CSS.to_owned())
}

pub async fn js() -> Response {
    asset("text/javascript; charset=utf-8", JS.to_owned())
}

pub async fn theme() -> Response {
    asset("text/css; charset=utf-8", theme_css(&read_theme_file()))
}

fn read_theme_file() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    std::fs::read_to_string(
        std::path::Path::new(&home).join(".local/state/omarchy/current/theme/colors.toml"),
    )
    .ok()
}

/// Colours used when the theme file is missing (the theme in use on 2026-10-21).
const FALLBACK: [(&str, &str); 13] = [
    ("bg", "#313138"),
    ("darker_bg", "#24242b"),
    ("lighter_bg", "#43434a"),
    ("dark_bg", "#2b2b31"),
    ("muted", "#6c6a6a"),
    ("fg", "#ebe3d0"),
    ("light_fg", "#f9f1de"),
    ("dark_fg", "#c1b9a7"),
    ("accent", "#7fa2bc"),
    ("green", "#bab792"),
    ("yellow", "#ddcea9"),
    ("red", "#a0726c"),
    ("selection", "#425869"),
];

fn is_hex(v: &str) -> bool {
    v.len() == 7 && v.starts_with('#') && v[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Reads `key = "#rrggbb"` lines. Stock Omarchy themes use long key names; they are aliased to the short ones.
fn palette(src: Option<&str>) -> (std::collections::HashMap<String, String>, bool) {
    let mut pal = std::collections::HashMap::new();
    let mut light = false;
    if let Some(text) = src {
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            if k == "mode" {
                light = v == "light";
            } else if is_hex(v) && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                pal.insert(k.to_owned(), v.to_owned());
            }
        }
    }
    for (short, long) in [
        ("bg", "background"),
        ("fg", "foreground"),
        ("dark_bg", "dark_background"),
        ("darker_bg", "darker_background"),
        ("lighter_bg", "lighter_background"),
        ("dark_fg", "dark_foreground"),
        ("light_fg", "light_foreground"),
        ("selection", "selection_background"),
    ] {
        if !pal.contains_key(short)
            && let Some(v) = pal.get(long).cloned()
        {
            pal.insert(short.to_owned(), v);
        }
    }
    for (k, v) in FALLBACK {
        pal.entry(k.to_owned()).or_insert_with(|| v.to_owned());
    }
    (pal, light)
}

/// `:root` variables with the names `design/tokens.css` uses. Text-safe status colours are mixed
/// toward the strongest foreground, which keeps contrast on dark and light themes alike.
pub fn theme_css(src: &Option<String>) -> String {
    let (p, light) = palette(src.as_deref());
    let g = |k: &str| p.get(k).map_or("#888888", String::as_str).to_owned();
    let mix = |c: &str| format!("color-mix(in srgb, {c} 70%, var(--text-strong))");
    format!(
        ":root {{\n  color-scheme: {scheme};\n  --bg: {bg};\n  --bg-deep: {deep};\n  --surface: {surface};\n  --surface-sunken: {sunken};\n  --border: {border};\n  --text: {text};\n  --text-strong: {strong};\n  --muted: {muted};\n  --accent: {accent};\n  --success: {green};\n  --warning: {yellow};\n  --error: {red};\n  --selection: {sel};\n  --success-text: {st};\n  --warning-text: {wt};\n  --error-text: {et};\n  --focus-ring: 0 0 0 2px {bg}, 0 0 0 4px {accent};\n  --radius-pane: 10px;\n  --radius-ctrl: 6px;\n  --gap: 10px;\n  --gap-outer: 12px;\n  --pane-min: 260px;\n  --font-ui: \"Inter\", \"Inter Display\", system-ui, sans-serif;\n  --font-mono: \"GeistMono Nerd Font\", \"GeistMono NF\", \"JetBrainsMono Nerd Font\", monospace;\n  --font-display: \"Departure Mono\", var(--font-mono);\n  --fs-base: 16px;\n  --fs-label: 13px;\n  --fs-small: 14px;\n  --fs-title: 18px;\n  --fs-display: 22px;\n  --lh-base: 1.45;\n  --dur-2: 160ms;\n  --ease-out: cubic-bezier(0.16, 1, 0.3, 1);\n}}\n@media (prefers-reduced-motion: reduce) {{ :root {{ --dur-2: 0ms; }} }}\n",
        scheme = if light { "light" } else { "dark" },
        bg = g("bg"),
        deep = g("darker_bg"),
        surface = g("lighter_bg"),
        sunken = g("dark_bg"),
        border = g("muted"),
        text = g("fg"),
        strong = g("light_fg"),
        muted = g("dark_fg"),
        accent = g("accent"),
        green = g("green"),
        yellow = g("yellow"),
        red = g("red"),
        sel = g("selection"),
        st = mix("var(--success)"),
        wt = mix("var(--warning)"),
        et = mix("var(--error)"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_css_uses_live_palette_and_aliases_long_names() {
        let src = Some("mode = \"light\"\nbackground = \"#fafafa\"\nforeground = \"#111111\"\naccent = \"#0055ff\"\n".to_owned());
        let css = theme_css(&src);
        assert!(css.contains("color-scheme: light"));
        assert!(css.contains("--bg: #fafafa;"), "{css}");
        assert!(css.contains("--text: #111111;"));
        assert!(css.contains("--accent: #0055ff;"));
        // Keys the file lacks fall back to the built-in theme instead of breaking the page.
        assert!(css.contains("--bg-deep: #24242b;"));
    }

    #[test]
    fn theme_css_ignores_values_that_are_not_hex() {
        let src = Some("bg = \"red; } body { display:none\"\n".to_owned());
        let css = theme_css(&src);
        assert!(css.contains("--bg: #313138;"));
        assert!(!css.contains("display:none"));
    }

    #[test]
    fn missing_theme_file_uses_fallback() {
        assert!(theme_css(&None).contains("--accent: #7fa2bc;"));
    }
}
