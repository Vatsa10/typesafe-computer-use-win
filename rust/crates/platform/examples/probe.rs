//! Live checks against the real desktop: the monitor table, and that BitBlt produced pixels.

use platform::capture::{capture_monitor, capture_primary};
use platform::display::{cursor_position, monitor_at, monitors, primary_size, virtual_bounds};

fn main() {
    println!(
        "dpi aware declared: {}",
        platform::display::declare_dpi_aware()
    );
    let (w, h) = primary_size();
    println!("primary display: {w}x{h}");
    println!("virtual bounds: {:?}", virtual_bounds());

    let list = monitors();
    println!("\nmonitors(): {} found", list.len());
    for m in &list {
        println!(
            "  index {}: {}, {}, {}, {}{}  ({}x{})",
            m.index,
            m.left,
            m.top,
            m.right,
            m.bottom,
            if m.primary { ", primary" } else { "" },
            m.width(),
            m.height()
        );
    }

    println!("\nmonitor_at:");
    for m in &list {
        let (cx, cy) = (m.left + m.width() / 2, m.top + m.height() / 2);
        println!(
            "  centre of {} ({cx}, {cy}) -> {}",
            m.index,
            monitor_at(cx, cy)
        );
    }
    println!("  (-32000, -32000) -> {}", monitor_at(-32000, -32000));
    let (cx, cy) = cursor_position();
    println!("  cursor ({cx}, {cy}) -> {}", monitor_at(cx, cy));

    println!("\ncaptures:");
    for m in &list {
        match capture_monitor(m) {
            Some(shot) => println!(
                "  monitor {}: {}x{} origin {:?} bytes {} first 16 {:?}",
                m.index,
                shot.width,
                shot.height,
                shot.origin,
                shot.bgra.len(),
                &shot.bgra[..16.min(shot.bgra.len())]
            ),
            None => println!("  monitor {}: capture failed", m.index),
        }
    }
    match capture_primary() {
        Some(shot) => println!(
            "  primary: {}x{} origin {:?} first 16 {:?}",
            shot.width,
            shot.height,
            shot.origin,
            &shot.bgra[..16.min(shot.bgra.len())]
        ),
        None => println!("  primary: capture failed"),
    }
}
