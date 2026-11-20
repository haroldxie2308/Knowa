//! exif@1: capture time, position, camera and image geometry from EXIF.
//!
//! All-or-nothing per blob: a field that is present but cannot be converted
//! faithfully fails the whole extractor with a message (reported, never dropped).
//! Blank values, which EXIF defines as "unknown", count as absent.
//!
//! Conversions:
//! - `knowa:when`: DateTimeOriginal `YYYY:MM:DD HH:MM:SS` → `YYYY-MM-DDTHH:MM:SS`;
//!   SubSecTimeOriginal → `.fff` (digits past the third truncated, fewer
//!   zero-padded, which preserves the value); OffsetTimeOriginal → `±HH:MM`. Without
//!   an offset the EDTF has none, i.e. local time with an unknown zone.
//! - `knowa:where`: GPS degree/minute/second rationals summed exactly, signed by
//!   the N/S/E/W reference, formatted per `decimal` (≤ 7 fraction digits, half away
//!   from zero). `alt` from GPSAltitude, negative when GPSAltitudeRef is 1.
//! - `exif:make`, `exif:model`: ASCII trimmed of the space padding writers use.
//! - `exif:width`/`exif:height`: PixelX/YDimension, else ImageWidth/ImageLength.
//! - `exif:orientation`: the raw value.

use std::fs::File;
use std::io::{self, BufReader};

use exif::{Exif, In, Reader, Tag};
use knowa_model::{Dec, Draft, Error, Geo, Id, Result, Term, Value, When, vocab};

use crate::decimal::Ratio;
use crate::{Input, Outcome};

const EXIF_MEDIA_TYPES: &[&str] = &["image/jpeg", "image/heic", "image/heif", "image/png", "image/tiff", "image/webp"];

const MAKE: &str = "exif:make";
const MODEL: &str = "exif:model";
const WIDTH: &str = "exif:width";
const HEIGHT: &str = "exif:height";
const ORIENTATION: &str = "exif:orientation";

/// A content problem in the EXIF data, reported as the extractor's error.
type Content<T> = std::result::Result<T, String>;

pub(crate) fn run(input: &Input) -> Result<Outcome> {
    if !EXIF_MEDIA_TYPES.contains(&input.media_type) {
        return Ok(Ok(Vec::new()));
    }
    let file = File::open(input.path).map_err(|e| Error::internal(format!("io: {}: {e}", input.path.display())))?;
    let exif = match Reader::new().read_from_container(&mut BufReader::new(file)) {
        Ok(exif) => exif,
        // The image simply has no EXIF block: zero drafts, not an error.
        Err(exif::Error::NotFound(_)) => return Ok(Ok(Vec::new())),
        // A truncated file is corrupt content; any other I/O failure is the
        // environment's and aborts the run.
        Err(exif::Error::Io(e)) if e.kind() != io::ErrorKind::UnexpectedEof => {
            return Err(Error::internal(format!("io: {}: {e}", input.path.display())));
        }
        Err(e) => return Ok(Err(format!("corrupt EXIF: {e}"))),
    };
    Ok(drafts(input.blob, &exif))
}

fn drafts(blob: &Id, exif: &Exif) -> Outcome {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if let Some(when) = when(exif)? {
        fields.push((vocab::WHEN, Value::When(when)));
    }
    if let Some(geo) = place(exif)? {
        fields.push((vocab::WHERE, Value::Geo(geo)));
    }
    for (p, tag) in [(MAKE, Tag::Make), (MODEL, Tag::Model)] {
        if let Some(t) = text(exif, tag)? {
            fields.push((p, Value::text(t)));
        }
    }
    // JPEG/HEIF store the primary image size as PixelX/YDimension in the Exif IFD;
    // TIFF files carry it only as ImageWidth/ImageLength in IFD0. Same quantity.
    for (p, primary, tiff) in [(WIDTH, Tag::PixelXDimension, Tag::ImageWidth), (HEIGHT, Tag::PixelYDimension, Tag::ImageLength)] {
        let size = match uint(exif, primary)? {
            Some(n) => Some(n),
            None => uint(exif, tiff)?,
        };
        if let Some(n) = size {
            fields.push((p, Value::Int(n)));
        }
    }
    if let Some(n) = uint(exif, Tag::Orientation)? {
        fields.push((ORIENTATION, Value::Int(n)));
    }
    fields
        .into_iter()
        .map(|(p, o)| {
            let p = Term::parse(p).map_err(|e| format!("predicate {p}: {}", e.message))?;
            Ok(Draft::new(blob.clone(), p, o))
        })
        .collect()
}

fn ascii(exif: &Exif, tag: Tag) -> Content<Option<&[u8]>> {
    match exif.get_field(tag, In::PRIMARY) {
        None => Ok(None),
        Some(f) => match &f.value {
            exif::Value::Ascii(strings) => Ok(strings.first().map(Vec::as_slice)),
            other => Err(format!("{tag} has type {other:?}, expected ASCII")),
        },
    }
}

fn when(exif: &Exif) -> Content<Option<When>> {
    let Some(raw) = ascii(exif, Tag::DateTimeOriginal)? else {
        return Ok(None);
    };
    let mut dt = match exif::DateTime::from_ascii(raw) {
        Ok(dt) => dt,
        Err(exif::Error::BlankValue(_)) => return Ok(None),
        Err(e) => return Err(format!("DateTimeOriginal {:?}: {e}", String::from_utf8_lossy(raw))),
    };
    if let Some(sub) = ascii(exif, Tag::SubSecTimeOriginal)? {
        dt.parse_subsec(sub).map_err(|e| format!("SubSecTimeOriginal {:?}: {e}", String::from_utf8_lossy(sub)))?;
    }
    if let Some(off) = ascii(exif, Tag::OffsetTimeOriginal)? {
        match dt.parse_offset(off) {
            Ok(()) | Err(exif::Error::BlankValue(_)) => {}
            Err(e) => return Err(format!("OffsetTimeOriginal {:?}: {e}", String::from_utf8_lossy(off))),
        }
    }
    let mut edtf = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
    );
    if let Some(ns) = dt.nanosecond {
        edtf.push_str(&format!(".{:03}", ns / 1_000_000));
    }
    if let Some(minutes) = dt.offset {
        let sign = if minutes < 0 { '-' } else { '+' };
        let m = minutes.unsigned_abs();
        edtf.push_str(&format!("{sign}{:02}:{:02}", m / 60, m % 60));
    }
    When::parse(&edtf).map(Some).map_err(|e| format!("DateTimeOriginal as EDTF {edtf:?}: {}", e.message))
}

fn rational(r: &exif::Rational, tag: Tag) -> Content<Ratio> {
    Ratio::new(r.num.into(), r.denom.into()).ok_or_else(|| format!("{tag} has a zero denominator"))
}

fn overflow(tag: Tag) -> String {
    format!("{tag} is out of range")
}

fn rationals(exif: &Exif, tag: Tag) -> Content<Option<&[exif::Rational]>> {
    match exif.get_field(tag, In::PRIMARY) {
        None => Ok(None),
        Some(f) => match &f.value {
            exif::Value::Rational(v) => Ok(Some(v)),
            other => Err(format!("{tag} has type {other:?}, expected RATIONAL")),
        },
    }
}

/// Degrees + minutes/60 + seconds/3600, exactly, as a signed decimal string.
fn coordinate(exif: &Exif, tag: Tag, parts: &[exif::Rational], ref_tag: Tag, pos: u8, neg: u8) -> Content<Dec> {
    let [d, m, s] = parts else {
        return Err(format!("{tag} has {} components, expected 3 (degrees, minutes, seconds)", parts.len()));
    };
    let (d, m, s) = (rational(d, tag)?, rational(m, tag)?, rational(s, tag)?);
    let total = m
        .div_int(60)
        .and_then(|m| d.add(m))
        .and_then(|dm| s.div_int(3600).and_then(|s| dm.add(s)))
        .ok_or_else(|| overflow(tag))?;
    let negative = match ascii(exif, ref_tag)? {
        Some([c]) if *c == pos => false,
        Some([c]) if *c == neg => true,
        Some(other) => {
            return Err(format!(
                "{ref_tag} is {:?}, expected {:?} or {:?}",
                String::from_utf8_lossy(other),
                char::from(pos),
                char::from(neg)
            ));
        }
        None => return Err(format!("{ref_tag} is missing, so the sign of {tag} is unknown")),
    };
    let text = total.to_decimal(negative).ok_or_else(|| overflow(tag))?;
    Dec::parse(&text).map_err(|e| format!("{tag} as decimal {text:?}: {}", e.message))
}

fn place(exif: &Exif) -> Content<Option<Geo>> {
    let (lat, lon) = match (rationals(exif, Tag::GPSLatitude)?, rationals(exif, Tag::GPSLongitude)?) {
        (None, None) => return Ok(None),
        (Some(lat), Some(lon)) => (lat, lon),
        _ => return Err("GPS has only one of GPSLatitude / GPSLongitude".to_string()),
    };
    let lat = coordinate(exif, Tag::GPSLatitude, lat, Tag::GPSLatitudeRef, b'N', b'S')?;
    let lon = coordinate(exif, Tag::GPSLongitude, lon, Tag::GPSLongitudeRef, b'E', b'W')?;
    Ok(Some(Geo { lat, lon, alt: altitude(exif)?, r: None }))
}

fn altitude(exif: &Exif) -> Content<Option<Dec>> {
    let Some(parts) = rationals(exif, Tag::GPSAltitude)? else {
        return Ok(None);
    };
    let [a] = parts else {
        return Err(format!("GPSAltitude has {} components, expected 1", parts.len()));
    };
    let below = match exif.get_field(Tag::GPSAltitudeRef, In::PRIMARY) {
        // EXIF 2.32 §4.6.6: GPSAltitudeRef defaults to 0 (above sea level).
        None => false,
        Some(f) => match f.value.get_uint(0) {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(format!("GPSAltitudeRef is {:?}, expected 0 or 1", f.value)),
        },
    };
    let text = rational(a, Tag::GPSAltitude)?.to_decimal(below).ok_or_else(|| overflow(Tag::GPSAltitude))?;
    Dec::parse(&text).map(Some).map_err(|e| format!("GPSAltitude as decimal {text:?}: {}", e.message))
}

fn text(exif: &Exif, tag: Tag) -> Content<Option<String>> {
    let Some(raw) = ascii(exif, tag)? else {
        return Ok(None);
    };
    let s = std::str::from_utf8(raw).map_err(|_| format!("{tag} is not valid UTF-8"))?;
    let s = s.trim();
    Ok((!s.is_empty()).then(|| s.to_string()))
}

fn uint(exif: &Exif, tag: Tag) -> Content<Option<i64>> {
    match exif.get_field(tag, In::PRIMARY) {
        None => Ok(None),
        Some(f) => f
            .value
            .get_uint(0)
            .map(|n| Some(i64::from(n)))
            .ok_or_else(|| format!("{tag} has value {:?}, expected an unsigned integer", f.value)),
    }
}
