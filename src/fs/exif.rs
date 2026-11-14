//! The useful part of a photo's EXIF: camera, lens, exposure and date.
//!
//! Read through `kamadak-exif`, which parses TIFF/JPEG/HEIF containers without
//! decoding pixels. Everything is returned as display-ready strings because
//! the only consumers are the Properties dialog and the preview's caption;
//! turning a rational into "1/250 s" again in three places would be worse.
//!
//! A file without EXIF, or one whose EXIF is malformed, yields an empty list:
//! metadata is an enhancement, never a reason to refuse the file.

use std::{fs::File, io::BufReader, path::Path};

use exif::{In, Tag};

/// Camera, lens and exposure fields, in the order they are shown.
pub fn read(path: &Path) -> Vec<(String, String)> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut BufReader::new(file)) else {
        return Vec::new();
    };

    let primary = |tag| exif.get_field(tag, In::PRIMARY);
    let thumb = |tag| exif.get_field(tag, In::THUMBNAIL);
    let text = |field: &exif::Field| field.display_value().to_string();
    let unit = |field: &exif::Field| field.display_value().with_unit(&exif).to_string();

    let mut rows: Vec<(String, String)> = Vec::new();

    let make = primary(Tag::Make).map(text).unwrap_or_default();
    let model = primary(Tag::Model)
        .or_else(|| thumb(Tag::Model))
        .map(text)
        .unwrap_or_default();
    let camera = match (make.trim(), model.trim()) {
        ("", "") => String::new(),
        ("", model) => model.to_string(),
        (make, "") => make.to_string(),
        // "NIKON CORPORATION NIKON Z 6" reads as a stutter to anyone who is not
        // a firmware string, so an already-included maker is not repeated.
        (make, model) if model.to_lowercase().contains(&make.to_lowercase()) => model.to_string(),
        (make, model) => format!("{make} {model}"),
    };
    if !camera.is_empty() {
        rows.push(("Camera".into(), camera));
    }

    if let Some(field) = primary(Tag::LensModel).or_else(|| primary(Tag::LensMake)) {
        let lens = text(field);
        if !lens.trim().is_empty() {
            rows.push(("Lens".into(), lens));
        }
    }

    let mut exposure: Vec<String> = Vec::new();
    if let Some(field) = primary(Tag::ExposureTime) {
        exposure.push(unit(field));
    }
    if let Some(field) = primary(Tag::FNumber) {
        let aperture = field.display_value().to_string();
        if !aperture.is_empty() {
            exposure.push(format!("f/{aperture}"));
        }
    }
    if let Some(field) = primary(Tag::PhotographicSensitivity) {
        exposure.push(format!("ISO {}", text(field)));
    }
    if let Some(field) = primary(Tag::FocalLength) {
        exposure.push(unit(field));
    }
    if !exposure.is_empty() {
        rows.push(("Exposure".into(), exposure.join(" · ")));
    }

    if let Some(field) = primary(Tag::DateTimeOriginal) {
        // Seconds are noise in a file manager; "2026-11-14 10:07" is enough.
        let taken = text(field);
        rows.push(("Taken".into(), taken.chars().take(16).collect()));
    }

    if let (Some(w), Some(h)) = (primary(Tag::PixelXDimension), primary(Tag::PixelYDimension)) {
        if let (Some(w), Some(h)) = (w.value.get_uint(0), h.value.get_uint(0)) {
            rows.push(("Dimensions".into(), format!("{w} × {h}")));
        }
    }

    rows
}

/// A one-line summary for the preview header, or `None` without EXIF.
pub fn summary(path: &Path) -> Option<String> {
    let rows = read(path);
    let value = |label: &str| {
        rows.iter().find(|(name, _)| name == label).map(|(_, value)| value.clone())
    };
    let mut parts = Vec::new();
    for label in ["Camera", "Exposure", "Taken"] {
        if let Some(value) = value(label) {
            parts.push(value);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_exif_returns_nothing_instead_of_failing() {
        let path = std::env::temp_dir().join(format!(
            "cadrocfile-exif-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, b"not a photo").unwrap();
        assert!(read(&path).is_empty());
        assert!(summary(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        assert!(read(Path::new("/nonexistent/photo.jpg")).is_empty());
    }
}
