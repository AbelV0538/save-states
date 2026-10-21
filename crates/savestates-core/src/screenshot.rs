//! Thumbnails: a screenshot by default, or an image the user picks.
//! Both are center-cropped to 16:9 and stored as small JPEGs so the grid stays uniform.

use std::path::Path;

use anyhow::{Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbImage, imageops};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::paths::write_atomic;

pub const THUMB_W: u32 = 480;
pub const THUMB_H: u32 = 270;

/// Screenshot the monitor under the mouse cursor (where the user is working).
pub fn capture_thumbnail(dest: &Path) -> Result<()> {
    let mut pt = POINT::default();
    let _ = unsafe { GetCursorPos(&mut pt) };
    let monitor = xcap::Monitor::from_point(pt.x, pt.y)
        .ok()
        .or_else(|| {
            let all = xcap::Monitor::all().ok()?;
            let primary = all.iter().position(|m| m.is_primary().unwrap_or(false)).unwrap_or(0);
            all.into_iter().nth(primary)
        })
        .context("no monitor to capture")?;
    let shot = monitor.capture_image().context("screen capture failed")?;
    write_thumbnail(DynamicImage::ImageRgba8(shot), dest)
}

/// Use an arbitrary image file as a state's thumbnail.
pub fn import_thumbnail(src: &Path, dest: &Path) -> Result<()> {
    let img = image::open(src).with_context(|| format!("could not open {}", src.display()))?;
    write_thumbnail(img, dest)
}

fn write_thumbnail(img: DynamicImage, dest: &Path) -> Result<()> {
    let thumb = to_thumbnail(&img.to_rgb8());
    let mut jpeg = Vec::with_capacity(48 * 1024);
    JpegEncoder::new_with_quality(&mut jpeg, 82)
        .write_image(thumb.as_raw(), thumb.width(), thumb.height(), ExtendedColorType::Rgb8)
        .context("encoding thumbnail")?;
    write_atomic(dest, &jpeg).with_context(|| format!("writing {}", dest.display()))
}

fn to_thumbnail(img: &RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    let (cw, ch) = crop_16_9(w, h);
    let cropped = imageops::crop_imm(img, (w - cw) / 2, (h - ch) / 2, cw, ch).to_image();
    imageops::thumbnail(&cropped, THUMB_W, THUMB_H)
}

/// Largest 16:9 region that fits in `w x h`.
fn crop_16_9(w: u32, h: u32) -> (u32, u32) {
    let (w64, h64) = (w as u64, h as u64);
    if w64 * 9 > h64 * 16 { (((h64 * 16) / 9).max(1) as u32, h) } else { (w, ((w64 * 9) / 16).max(1) as u32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_to_16_9() {
        assert_eq!(crop_16_9(2560, 1600), (2560, 1440)); // 16:10 loses height
        assert_eq!(crop_16_9(3440, 1440), (2560, 1440)); // ultrawide loses width
        assert_eq!(crop_16_9(1920, 1080), (1920, 1080));
        assert_eq!(crop_16_9(1080, 1920), (1080, 607)); // portrait
    }

    #[test]
    fn thumbnail_has_fixed_size() {
        let img = RgbImage::from_pixel(1000, 1000, image::Rgb([10, 20, 30]));
        let t = to_thumbnail(&img);
        assert_eq!(t.dimensions(), (THUMB_W, THUMB_H));
    }
}
