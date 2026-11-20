//! Media sniffing and EXIF conversion, on JPEGs whose EXIF is generated in-test.

use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use exif::experimental::Writer;
use exif::{Field, In, Rational, Tag};
use knowa_extract::{Extraction, ExtractionError, WORKERS, run, sniff_media_type};
use knowa_model::{ErrorCode, Id, Value};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("knowa-extract-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ascii(tag: Tag, s: &str) -> Field {
    Field { tag, ifd_num: In::PRIMARY, value: exif::Value::Ascii(vec![s.as_bytes().to_vec()]) }
}

fn rationals(tag: Tag, parts: &[(u32, u32)]) -> Field {
    let v = parts.iter().map(|&(num, denom)| Rational { num, denom }).collect();
    Field { tag, ifd_num: In::PRIMARY, value: exif::Value::Rational(v) }
}

fn short(tag: Tag, n: u16) -> Field {
    Field { tag, ifd_num: In::PRIMARY, value: exif::Value::Short(vec![n]) }
}

fn long(tag: Tag, n: u32) -> Field {
    Field { tag, ifd_num: In::PRIMARY, value: exif::Value::Long(vec![n]) }
}

fn byte(tag: Tag, n: u8) -> Field {
    Field { tag, ifd_num: In::PRIMARY, value: exif::Value::Byte(vec![n]) }
}

/// SOI + APP1("Exif\0\0" + TIFF) + EOI.
fn jpeg_with_app1(tiff: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend(u16::try_from(2 + 6 + tiff.len()).unwrap().to_be_bytes());
    out.extend(b"Exif\0\0");
    out.extend(tiff);
    out.extend([0xFF, 0xD9]);
    out
}

fn jpeg_with_exif(fields: &[Field]) -> Vec<u8> {
    let mut writer = Writer::new();
    for f in fields {
        writer.push_field(f);
    }
    let mut tiff = Cursor::new(Vec::new());
    writer.write(&mut tiff, false).unwrap();
    jpeg_with_app1(&tiff.into_inner())
}

fn extract(bytes: &[u8], name: &str) -> (Id, Vec<Extraction>, Vec<ExtractionError>) {
    let s = Scratch::new();
    let path = s.0.join("blob");
    fs::write(&path, bytes).unwrap();
    let blob = Id::blob_of(bytes);
    let (ok, err) = run(&blob, &path, Some(name), None).unwrap();
    (blob, ok, err)
}

fn worker<'a>(ok: &'a [Extraction], name: &str) -> &'a Extraction {
    ok.iter().find(|e| e.worker == name).unwrap_or_else(|| panic!("{name} missing"))
}

fn value<'a>(e: &'a Extraction, p: &str) -> Option<&'a Value> {
    let mut hits = e.drafts.iter().filter(|d| d.p.as_str() == p);
    let first = hits.next().map(|d| &d.o);
    assert!(hits.next().is_none(), "{p} emitted twice");
    first
}

fn when(e: &Extraction) -> &knowa_model::When {
    match value(e, "knowa:when") {
        Some(Value::When(w)) => w,
        other => panic!("no knowa:when: {other:?}"),
    }
}

fn geo(e: &Extraction) -> (String, String, Option<String>) {
    match value(e, "knowa:where") {
        Some(Value::Geo(g)) => {
            (g.lat.as_str().into(), g.lon.as_str().into(), g.alt.as_ref().map(|a| a.as_str().into()))
        }
        other => panic!("no knowa:where: {other:?}"),
    }
}

fn tokyo_fields() -> Vec<Field> {
    vec![
        ascii(Tag::Make, "Canon "),
        ascii(Tag::Model, "EOS R5"),
        short(Tag::Orientation, 6),
        ascii(Tag::DateTimeOriginal, "2024:05:01 10:00:00"),
        ascii(Tag::SubSecTimeOriginal, "1234"),
        ascii(Tag::OffsetTimeOriginal, "+09:00"),
        long(Tag::PixelXDimension, 6000),
        long(Tag::PixelYDimension, 4000),
        ascii(Tag::GPSLatitudeRef, "N"),
        rationals(Tag::GPSLatitude, &[(35, 1), (40, 1), (5244, 100)]),
        ascii(Tag::GPSLongitudeRef, "E"),
        rationals(Tag::GPSLongitude, &[(139, 1), (45, 1), (36, 1)]),
        byte(Tag::GPSAltitudeRef, 0),
        rationals(Tag::GPSAltitude, &[(405, 10)]),
    ]
}

#[test]
fn photo_with_full_exif() {
    let bytes = jpeg_with_exif(&tokyo_fields());
    let (blob, ok, err) = extract(&bytes, "IMG_0001.JPG");
    assert!(err.is_empty(), "{err:?}");
    assert_eq!(ok.iter().map(|e| e.worker).collect::<Vec<_>>(), WORKERS);
    assert!(ok.iter().flat_map(|e| &e.drafts).all(|d| d.s == blob));

    let media = worker(&ok, "media@1");
    assert_eq!(value(media, "knowa:mediaType"), Some(&Value::text("image/jpeg")));
    assert_eq!(value(media, "knowa:size"), Some(&Value::Int(bytes.len() as i64)));
    assert!(matches!(value(media, "knowa:type"), Some(Value::Term(t)) if t.as_str() == "knowa:Photo"));

    let exif = worker(&ok, "exif@1");
    let w = when(exif);
    // Sub-seconds past the third digit are truncated; the offset is kept.
    assert_eq!(w.raw, "2024-05-01T10:00:00.123+09:00");
    assert!(!w.local_unknown);
    // 35°40'52.44" = 35.68123333…; 139°45'36" = 139.76 exactly.
    assert_eq!(geo(exif), ("35.6812333".into(), "139.76".into(), Some("40.5".into())));
    assert_eq!(value(exif, "exif:make"), Some(&Value::text("Canon")));
    assert_eq!(value(exif, "exif:model"), Some(&Value::text("EOS R5")));
    assert_eq!(value(exif, "exif:width"), Some(&Value::Int(6000)));
    assert_eq!(value(exif, "exif:height"), Some(&Value::Int(4000)));
    assert_eq!(value(exif, "exif:orientation"), Some(&Value::Int(6)));
}

#[test]
fn no_offset_means_local_unknown_and_refs_sign_coordinates() {
    let bytes = jpeg_with_exif(&[
        ascii(Tag::DateTimeOriginal, "2019:12:31 23:59:59"),
        ascii(Tag::GPSLatitudeRef, "S"),
        rationals(Tag::GPSLatitude, &[(33, 1), (52, 1), (0, 1)]),
        ascii(Tag::GPSLongitudeRef, "W"),
        rationals(Tag::GPSLongitude, &[(151, 1), (12, 1), (30, 1)]),
        byte(Tag::GPSAltitudeRef, 1),
        rationals(Tag::GPSAltitude, &[(3, 2)]),
    ]);
    let (_, ok, err) = extract(&bytes, "photo.jpg");
    assert!(err.is_empty(), "{err:?}");
    let exif = worker(&ok, "exif@1");
    let w = when(exif);
    assert_eq!(w.raw, "2019-12-31T23:59:59");
    assert!(w.local_unknown);
    assert_eq!(geo(exif), ("-33.8666667".into(), "-151.2083333".into(), Some("-1.5".into())));
    assert_eq!(value(exif, "exif:make"), None);
}

#[test]
fn images_without_exif_yield_no_exif_drafts() {
    let (_, ok, err) = extract(&[0xFF, 0xD8, 0xFF, 0xD9], "bare.jpg");
    assert!(err.is_empty(), "{err:?}");
    assert!(worker(&ok, "exif@1").drafts.is_empty());
    assert_eq!(value(worker(&ok, "media@1"), "knowa:mediaType"), Some(&Value::text("image/jpeg")));

    let (_, ok, err) = extract("plain words\n".as_bytes(), "notes.txt");
    assert!(err.is_empty());
    let media = worker(&ok, "media@1");
    assert_eq!(value(media, "knowa:mediaType"), Some(&Value::text("text/plain")));
    assert!(matches!(value(media, "knowa:type"), Some(Value::Term(t)) if t.as_str() == "knowa:File"));
    assert!(worker(&ok, "exif@1").drafts.is_empty());
}

fn exif_error(bytes: &[u8]) -> String {
    let (_, ok, err) = extract(bytes, "broken.jpg");
    assert!(ok.iter().all(|e| e.worker != "exif@1"), "exif@1 must not also succeed");
    assert!(ok.iter().any(|e| e.worker == "media@1"), "media@1 still runs");
    assert_eq!(err.len(), 1, "{err:?}");
    assert_eq!(err[0].worker, "exif@1");
    err[0].message.clone()
}

#[test]
fn corrupt_exif_is_reported_not_dropped() {
    // TIFF header pointing at an IFD that is cut off.
    exif_error(&jpeg_with_app1(b"MM\0\x2a\0\0\0\x08\0\x05"));

    let msg = exif_error(&jpeg_with_exif(&[
        ascii(Tag::GPSLatitudeRef, "N"),
        rationals(Tag::GPSLatitude, &[(35, 1), (40, 0), (0, 1)]),
        ascii(Tag::GPSLongitudeRef, "E"),
        rationals(Tag::GPSLongitude, &[(139, 1), (0, 1), (0, 1)]),
    ]));
    assert!(msg.contains("zero denominator"), "{msg}");

    let msg = exif_error(&jpeg_with_exif(&[
        rationals(Tag::GPSLatitude, &[(35, 1), (0, 1), (0, 1)]),
        ascii(Tag::GPSLongitudeRef, "E"),
        rationals(Tag::GPSLongitude, &[(139, 1), (0, 1), (0, 1)]),
    ]));
    assert!(msg.contains("GPSLatitudeRef"), "{msg}");

    let msg = exif_error(&jpeg_with_exif(&[ascii(Tag::DateTimeOriginal, "2024:13:01 10:00:00")]));
    assert!(msg.contains("DateTimeOriginal"), "{msg}");
}

#[test]
fn blank_values_count_as_absent() {
    let bytes = jpeg_with_exif(&[ascii(Tag::DateTimeOriginal, "    :  :     :  :  "), ascii(Tag::Make, "   ")]);
    let (_, ok, err) = extract(&bytes, "blank.jpg");
    assert!(err.is_empty(), "{err:?}");
    assert!(worker(&ok, "exif@1").drafts.is_empty());
}

#[test]
fn selection_and_missing_files() {
    let s = Scratch::new();
    let path = s.0.join("photo.jpg");
    let bytes = jpeg_with_exif(&tokyo_fields());
    fs::write(&path, &bytes).unwrap();
    let blob = Id::blob_of(&bytes);

    let (ok, err) = run(&blob, &path, None, Some(&["exif@1"])).unwrap();
    assert!(err.is_empty());
    assert_eq!(ok.iter().map(|e| e.worker).collect::<Vec<_>>(), ["exif@1"]);

    assert_eq!(run(&blob, &path, None, Some(&["ocr@1"])).unwrap_err().code, ErrorCode::Invalid);
    assert_eq!(run(&blob, &s.0.join("gone"), None, None).unwrap_err().code, ErrorCode::NotFound);
    let entity = Id::new_entity().unwrap();
    assert_eq!(run(&entity, &path, None, None).unwrap_err().code, ErrorCode::Invalid);
}

#[test]
fn sniffing_prefers_content_then_extension_then_text() {
    assert_eq!(sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0], Some("wrong.txt")), "image/jpeg");
    assert_eq!(sniff_media_type(b"# Title\n", Some("README.MD")), "text/markdown");
    assert_eq!(sniff_media_type(b"# Title\n", None), "text/plain");
    assert_eq!(sniff_media_type(&[0x01, 0x00, 0x02], None), "application/octet-stream");
    // A head that ends inside a multi-byte character is still text.
    let cut = &"naïve café".as_bytes()[..11];
    assert!(std::str::from_utf8(cut).is_err());
    assert_eq!(sniff_media_type(cut, None), "text/plain");
    // Invalid UTF-8 in the middle is not.
    assert_eq!(sniff_media_type(&[b'a', 0xC3, b'b'], None), "application/octet-stream");
}
