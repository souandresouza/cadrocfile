//! The user's accent colour.
//!
//! One colour drives selection, focus rings and the capacity meters. Rather
//! than shipping a second stylesheet or a second `CssProvider` and hoping
//! `@define-color` resolution goes our way across providers, the two accent
//! definitions are substituted into the stylesheet text before it is parsed.
//! That is unambiguous: what GTK sees is a stylesheet that only ever named one
//! accent.
use crate::tr;

/// Shipped default — the red the app has used since the identity settled.
pub const DEFAULT: &str = "#E04434";

/// Presets offered in Settings, chosen to stay legible against both the light
/// and dark libadwaita surfaces at the alpha values the stylesheet uses.
pub const PRESETS: &[(&str, &str)] = &[
    ("Red", "#E04434"),
    ("Orange", "#E8760F"),
    ("Amber", "#C79000"),
    ("Green", "#2E9E4F"),
    ("Teal", "#0F9B8E"),
    ("Blue", "#3584E4"),
    ("Purple", "#8B5CF6"),
    ("Pink", "#D63384"),
];

/// Parses `#RRGGBB` (or `RRGGBB`) into components.
pub fn parse(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// Normalises any accepted spelling to `#RRGGBB`, or the default if it is not
/// a colour at all — a hand-edited config should not leave the app unstyled.
pub fn normalise(hex: &str) -> String {
    match parse(hex) {
        Some((r, g, b)) => format!("#{r:02X}{g:02X}{b:02X}"),
        None => DEFAULT.to_string(),
    }
}

/// A lighter partner for the accent, used where the flat colour would be too
/// heavy — the bright end of the capacity ring, hover states.
///
/// Derived rather than configured: asking someone to pick two colours that
/// look related is asking them to do the computer's job.
pub fn brighten(hex: &str) -> String {
    let (r, g, b) = parse(hex).unwrap_or((224, 68, 52));
    // A third of the way to white keeps the hue and lifts the value enough to
    // read as a highlight against the base colour.
    let lift = |c: u8| c as u16 + ((255 - c as u16) / 3);
    format!("#{:02X}{:02X}{:02X}", lift(r), lift(g), lift(b))
}

/// The stylesheet with the accent definitions replaced by `hex`, plus one rule
/// per preset swatch.
///
/// The swatches are generated here rather than painted with a `CssProvider` per
/// button, which is what `gtk_widget_get_style_context` was for and which GTK
/// deprecated in 4.10. Emitting them as ordinary classes keeps every colour the
/// app draws in one stylesheet.
pub fn apply(css: &str, hex: &str) -> String {
    let accent = normalise(hex);
    let bright = brighten(&accent);
    let mut out = css.replace(
        "@define-color fm_accent #E04434;\n@define-color fm_accent_bright #E9705F;",
        &format!("@define-color fm_accent {accent};\n@define-color fm_accent_bright {bright};"),
    );

    out.push_str("\n/* Generated: one class per preset swatch. */\n");
    for (index, (_, preset)) in PRESETS.iter().enumerate() {
        out.push_str(&format!(
            ".accent-swatch-{index} {{ background-image: none; background-color: {preset}; }}\n"
        ));
    }
    out
}

/// The CSS class carrying a preset's colour.
pub fn swatch_class(index: usize) -> String {
    format!("accent-swatch-{index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_parsed_with_or_without_the_hash() {
        assert_eq!(parse("#E04434"), Some((224, 68, 52)));
        assert_eq!(parse("e04434"), Some((224, 68, 52)));
        assert_eq!(parse("#fff"), None, "shorthand is not accepted");
        assert_eq!(parse("#GGGGGG"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn a_nonsense_colour_falls_back_to_the_default() {
        assert_eq!(normalise("not a colour"), DEFAULT);
        assert_eq!(normalise("#3584e4"), "#3584E4", "case is normalised");
    }

    #[test]
    fn the_bright_partner_is_lighter_but_keeps_the_hue() {
        for (_, hex) in PRESETS {
            let (r, g, b) = parse(hex).unwrap();
            let (br, bg, bb) = parse(&brighten(hex)).unwrap();
            assert!(br >= r && bg >= g && bb >= b, "{hex} must not darken");
            assert!(
                br as u16 + bg as u16 + bb as u16 > r as u16 + g as u16 + b as u16,
                "{hex} must actually lighten"
            );
            // Ordering of the channels is what carries the hue.
            let order = |x: u8, y: u8| x.cmp(&y);
            assert_eq!(order(r, g), order(br, bg), "{hex} hue shifted");
            assert_eq!(order(g, b), order(bg, bb), "{hex} hue shifted");
        }
    }

    /// The substitution has to actually match the stylesheet it ships with,
    /// which a stray edit to either file would silently break.
    #[test]
    fn the_stylesheet_placeholder_is_substituted() {
        let css = include_str!("style.css");
        assert!(css.contains("@define-color fm_accent #E04434;"), "stylesheet drifted");

        let out = apply(css, "#3584E4");
        assert!(out.contains("@define-color fm_accent #3584E4;"), "accent not applied");
        assert!(
            out.contains(&format!("@define-color fm_accent_bright {};", brighten("#3584E4"))),
            "bright not applied"
        );
        // The default's own swatch rule legitimately mentions it, so check the
        // definitions rather than the whole file.
        assert!(
            !out.contains("@define-color fm_accent #E04434;"),
            "the default definition should have been replaced"
        );
        for (index, (_, preset)) in PRESETS.iter().enumerate() {
            assert!(
                out.contains(&format!(".accent-swatch-{index} {{")),
                "missing swatch rule for {preset}"
            );
        }
    }

    #[test]
    fn every_preset_is_a_valid_colour() {
        for (name, hex) in PRESETS {
            assert!(parse(hex).is_some(), "{name} has an invalid hex: {hex}");
        }
    }
}
