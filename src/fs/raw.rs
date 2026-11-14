//! Raw photo previews without LibRaw: find the JPEG the camera already made.
//!
//! Every raw format in common use stores one or more JPEG previews next to the
//! sensor data — that is what the camera's own screen shows, and what viewers
//! display before a raw developer runs. Nordend does the same, and so does
//! every thumbnailer worth using. Reading it costs a few hundred kilobytes and
//! one TIFF index walk, so a folder of 40 MB photos still fills at once.
//!
//! TIFF-based raws (CR2, NEF, ARW, ORF, DNG, RW2, SRW, PEF, 3FR…) are walked
//! through their IFDs, including SubIFDs where several cameras hide the large
//! preview. Fuji's RAF is not TIFF at all: its header carries the preview's
//! offset and length directly.
//!
//! Nothing here decodes or interprets sensor data, which keeps it safe: an
//! embedded JPEG is handed to the same memory-safe-ish image stack as any
//! other JPEG.

use std::{
    fs::File,
    os::unix::fs::FileExt,
    path::Path,
};

/// Extensions handled by the embedded-preview reader.
///
/// Listed explicitly rather than trusting the mime type: gio knows the common
/// ones (`image/x-canon-cr2`), but `.3fr`, `.iiq` and friends often arrive as
/// `application/octet-stream`, and HEIC-based formats (CR3, Hif) have no JPEG
/// in a TIFF index to find.
pub const EXTENSIONS: &[&str] = &[
    "3fr", "arw", "cr2", "crw", "dcr", "dng", "erf", "fff", "iiq", "kdc", "mef", "mos", "mrw",
    "nef", "nrw", "orf", "pef", "raf", "raw", "rw2", "rwl", "srw", "srf", "x3f",
];

/// Whether a path names a raw photo this reader understands.
pub fn is_raw(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| EXTENSIONS.contains(&e.as_str()))
}

/// The largest embedded JPEG preview in `path`, ready to be decoded.
///
/// `None` means the file has no readable preview — a broken download, an
/// unsupported variant, or a raw that stores only a thumbnail the camera
/// cropped to its screen.
pub fn embedded_preview(path: &Path) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    if size < 16 {
        return None;
    }

    let mut header = [0u8; 16];
    file.read_exact_at(&mut header, 0).ok()?;

    if header.starts_with(b"FUJIFILMCCD-RAW") {
        return fuji_preview(&file, size);
    }

    tiff_preview(&file, size, &header)
}

/// Fuji's RAF header: a 16-byte magic followed by offsets in big-endian.
///
/// The JPEG preview sits at byte 84 (offset) and 88 (length), which is what
/// dcraw and every reader since has used.
fn fuji_preview(file: &File, size: u64) -> Option<Vec<u8>> {
    let mut fields = [0u8; 8];
    file.read_exact_at(&mut fields, 84).ok()?;
    let offset = u32::from_be_bytes(fields[0..4].try_into().ok()?) as u64;
    let length = u32::from_be_bytes(fields[4..8].try_into().ok()?) as u64;
    read_jpeg(file, size, offset, length)
}

/// Confirms the range is inside the file and starts like a JPEG.
fn read_jpeg(file: &File, size: u64, offset: u64, length: u64) -> Option<Vec<u8>> {
    if length < 4 || offset.checked_add(length)? > size {
        return None;
    }
    // A preview that claims half a gigabyte is a corrupt index, not a photo.
    let length = length.min(64 * 1024 * 1024);
    let mut bytes = vec![0u8; length as usize];
    file.read_exact_at(&mut bytes, offset).ok()?;
    (bytes.starts_with(&[0xFF, 0xD8])).then_some(bytes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Little,
    Big,
}

impl Order {
    fn u16(self, bytes: &[u8]) -> u16 {
        let pair = [bytes[0], bytes[1]];
        match self {
            Order::Little => u16::from_le_bytes(pair),
            Order::Big => u16::from_be_bytes(pair),
        }
    }

    fn u32(self, bytes: &[u8]) -> u32 {
        let quad = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Order::Little => u32::from_le_bytes(quad),
            Order::Big => u32::from_be_bytes(quad),
        }
    }
}

/// Walks a TIFF-based raw's IFD tree and returns the biggest embedded JPEG.
fn tiff_preview(file: &File, size: u64, header: &[u8; 16]) -> Option<Vec<u8>> {
    let order = match &header[0..2] {
        b"II" => Order::Little,
        b"MM" => Order::Big,
        _ => return None,
    };
    // 42 is ordinary TIFF. Olympus uses 0x4f52 / 0x5352 ("RO" / "RS") and
    // Panasonic's RW2 begins "IIU\0", which is 0x55 read as a little-endian
    // short at the same place. All three keep the classic IFD layout.
    let magic = order.u16(&header[2..4]);
    if !matches!(magic, 42 | 0x4f52 | 0x5352 | 0x55) {
        return None;
    }

    let mut walker = Walker { file, size, order, best: None, seen: Vec::new() };
    let first = order.u32(&header[4..8]) as u64;
    walker.ifd(first, 0);

    let (offset, length) = walker.best?;
    read_jpeg(file, size, offset, length)
}

struct Walker<'a> {
    file: &'a File,
    size: u64,
    order: Order,
    /// Offset and length of the largest preview found so far.
    best: Option<(u64, u64)>,
    seen: Vec<u64>,
}

impl Walker<'_> {
    fn read(&self, offset: u64, length: usize) -> Option<Vec<u8>> {
        if offset.checked_add(length as u64)? > self.size {
            return None;
        }
        let mut buffer = vec![0u8; length];
        self.file.read_exact_at(&mut buffer, offset).ok()?;
        Some(buffer)
    }

    fn ifd(&mut self, offset: u64, depth: u8) {
        if depth > 8 || offset < 8 || self.seen.contains(&offset) {
            return;
        }
        self.seen.push(offset);

        let Some(head) = self.read(offset, 2) else { return };
        let count = self.order.u16(&head) as usize;
        // A real IFD has tens of entries; a huge count means the offset is not
        // an IFD at all. Zero is legal (and is where a camera leaves IFD0 when
        // only the preview in IFD1 matters), so it must still chain onwards.
        if count > 512 {
            return;
        }
        let Some(table) = self.read(offset + 2, count * 12) else { return };

        let (mut jpeg_offset, mut jpeg_length) = (None, None);
        for entry in table.chunks_exact(12) {
            let tag = self.order.u16(&entry[0..2]);
            let kind = self.order.u16(&entry[2..4]);
            let count = self.order.u32(&entry[4..8]) as u64;
            let value = &entry[8..12];

            match tag {
                // JPEGInterchangeFormat / Length: where the preview lives.
                0x0201 => {
                    jpeg_offset = Some(match kind {
                        3 => self.order.u16(value) as u64,
                        _ => self.order.u32(value) as u64,
                    });
                }
                0x0202 => {
                    jpeg_length = Some(match kind {
                        3 => self.order.u16(value) as u64,
                        _ => self.order.u32(value) as u64,
                    });
                }
                // SubIFDs: several cameras keep the big preview in one of them.
                0x014a => self.sub_ifds(kind, count, value, depth),
                // The EXIF IFD occasionally holds a second, smaller preview.
                0x8769 => {
                    if let Some(offset) = self.nested_offset(kind, count, value) {
                        self.ifd(offset, depth + 1);
                    }
                }
                _ => {}
            }
        }

        if let (Some(offset), Some(length)) = (jpeg_offset, jpeg_length)
            && length > self.best.map_or(0, |(_, l)| l)
            && offset + length <= self.size
            && length <= 64 * 1024 * 1024
        {
            self.best = Some((offset, length));
        }

        // IFD1 is where TIFF puts the preview for formats that do not use
        // SubIFDs (Canon CR2's full-size preview, for one).
        if let Some(next) = self.read(offset + 2 + count as u64 * 12, 4) {
            let next = self.order.u32(&next) as u64;
            if next != 0 {
                self.ifd(next, depth + 1);
            }
        }
    }

    fn sub_ifds(&mut self, kind: u16, count: u64, value: &[u8], depth: u8) {
        if !matches!(kind, 4 | 13) || count == 0 {
            return;
        }
        // A single SubIFD is stored inline; an array lives elsewhere.
        if count == 1 {
            if let Some(offset) = self.nested_offset(kind, count, value) {
                self.ifd(offset, depth + 1);
            }
            return;
        }
        let count = count.min(16) as usize;
        let Some(array) = self.read(self.order.u32(value) as u64, count * 4) else { return };
        for index in 0..count {
            let offset = self.order.u32(&array[index * 4..index * 4 + 4]) as u64;
            self.ifd(offset, depth + 1);
        }
    }

    /// A LONG/IFD value that may be inline (one value) or an offset (many).
    fn nested_offset(&self, kind: u16, count: u64, value: &[u8]) -> Option<u64> {
        if !matches!(kind, 4 | 13) || count == 0 {
            return None;
        }
        if count == 1 {
            return Some(self.order.u32(value) as u64);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cadrocfile-raw-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0xFF, 0xD9];

    /// A minimal little-endian TIFF whose IFD0 has no preview and whose IFD1
    /// points at one — the layout Canon uses for CR2.
    fn tiff_with_preview_in_ifd1() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II\x2a\x00");
        bytes.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at 8

        // IFD0: no entries, next IFD at 8 + 2 + 0 + 4 = 14.
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&14u32.to_le_bytes());

        // IFD1: two entries, preview after the table.
        let jpeg_offset = 14 + 2 + 2 * 12 + 4;
        bytes.extend_from_slice(&2u16.to_le_bytes());
        let mut entry = |tag: u16, value: u32| {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&4u16.to_le_bytes()); // LONG
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
        };
        entry(0x0201, jpeg_offset as u32);
        entry(0x0202, JPEG.len() as u32);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(JPEG);
        bytes
    }

    #[test]
    fn finds_a_preview_in_the_second_ifd() {
        let dir = temp("ifd1");
        let path = dir.join("IMG_0001.CR2");
        fs::write(&path, tiff_with_preview_in_ifd1()).unwrap();
        assert_eq!(embedded_preview(&path).as_deref(), Some(JPEG));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_a_preview_in_a_sub_ifd() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"MM\x00\x2a");
        bytes.extend_from_slice(&8u32.to_be_bytes()); // IFD0 at 8

        // IFD0: one entry (SubIFDs, one value) then next=0.
        let sub_ifd = 8 + 2 + 12 + 4;
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&0x014au16.to_be_bytes());
        bytes.extend_from_slice(&4u16.to_be_bytes()); // LONG
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&(sub_ifd as u32).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());

        // SubIFD: preview entry pair.
        let jpeg_offset = sub_ifd + 2 + 2 * 12 + 4;
        bytes.extend_from_slice(&2u16.to_be_bytes());
        bytes.extend_from_slice(&0x0201u16.to_be_bytes());
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&(jpeg_offset as u32).to_be_bytes());
        bytes.extend_from_slice(&0x0202u16.to_be_bytes());
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&(JPEG.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(JPEG);

        let dir = temp("subifd");
        let path = dir.join("IMG_0002.NEF");
        fs::write(&path, bytes).unwrap();
        assert_eq!(embedded_preview(&path).as_deref(), Some(JPEG));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_a_fuji_preview_from_the_header() {
        let mut bytes = vec![0u8; 120];
        bytes[..16].copy_from_slice(b"FUJIFILMCCD-RAW ");
        bytes[84..88].copy_from_slice(&120u32.to_be_bytes());
        bytes[88..92].copy_from_slice(&(JPEG.len() as u32).to_be_bytes());
        bytes.extend_from_slice(JPEG);

        let dir = temp("raf");
        let path = dir.join("DSCF0001.RAF");
        fs::write(&path, bytes).unwrap();
        assert_eq!(embedded_preview(&path).as_deref(), Some(JPEG));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_index_is_refused_instead_of_panicking() {
        let dir = temp("truncated");
        let path = dir.join("broken.DNG");
        fs::write(&path, b"II\x2a\x00\xff\xff\xff\xff").unwrap();
        assert!(embedded_preview(&path).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recognises_raw_extensions_case_insensitively() {
        assert!(is_raw(Path::new("/x/IMG_1.ORF")));
        assert!(is_raw(Path::new("/x/IMG_1.dng")));
        assert!(!is_raw(Path::new("/x/IMG_1.JPG")));
    }
}
