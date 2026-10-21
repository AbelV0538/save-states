//! The app icon, drawn in code so there is no asset to ship: a rounded blue tile
//! with three stacked "save slot" bars.

pub const SIZE: u32 = 32;

pub fn rgba() -> Vec<u8> {
    let s = SIZE as i32;
    let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
    let bars = [(8, 12), (14, 18), (20, 24)];
    for y in 0..s {
        for x in 0..s {
            let i = ((y * s + x) * 4) as usize;
            if !inside_rounded(x, y, s, 7) {
                continue;
            }
            let on_bar = (7..s - 7).contains(&x) && bars.iter().any(|&(a, b)| (a..b).contains(&y));
            let [r, g, b] = if on_bar { [255, 255, 255] } else { [37, 99, 235] };
            px[i..i + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    px
}

fn inside_rounded(x: i32, y: i32, size: i32, radius: i32) -> bool {
    let cx = x.clamp(radius, size - 1 - radius);
    let cy = y.clamp(radius, size - 1 - radius);
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= radius * radius
}

pub fn window_icon() -> eframe::egui::IconData {
    eframe::egui::IconData { rgba: rgba(), width: SIZE, height: SIZE }
}

pub fn tray_icon() -> Option<tray_icon::Icon> {
    tray_icon::Icon::from_rgba(rgba(), SIZE, SIZE).ok()
}
