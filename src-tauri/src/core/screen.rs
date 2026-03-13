use crate::core::protocol::ScreenInfo;

/// Get information about all connected displays on this machine.
pub fn get_screens() -> Vec<ScreenInfo> {
    #[cfg(target_os = "windows")]
    {
        get_screens_windows()
    }
    #[cfg(target_os = "macos")]
    {
        get_screens_macos()
    }
    #[cfg(target_os = "linux")]
    {
        crate::input::linux::get_screens_linux()
    }
}

#[cfg(target_os = "windows")]
fn get_screens_windows() -> Vec<ScreenInfo> {
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, MONITORINFOEXW, HDC, HMONITOR,
    };
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use std::mem;

    let mut screens: Vec<ScreenInfo> = Vec::new();

    unsafe extern "system" fn enum_callback(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let screens = &mut *(data.0 as *mut Vec<ScreenInfo>);
        let mut info: MONITORINFOEXW = mem::zeroed();
        info.monitorInfo.cbSize = mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _).as_bool() {
            let rc = info.monitorInfo.rcMonitor;
            let primary = (info.monitorInfo.dwFlags & 1) != 0; // MONITORINFOF_PRIMARY
            let name = String::from_utf16_lossy(
                &info.szDevice[..info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len())]
            );
            screens.push(ScreenInfo {
                id: name,
                x: rc.left,
                y: rc.top,
                width: rc.right - rc.left,
                height: rc.bottom - rc.top,
                primary,
            });
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_callback),
            LPARAM(&mut screens as *mut _ as isize),
        );
    }

    screens
}

#[cfg(target_os = "macos")]
fn get_screens_macos() -> Vec<ScreenInfo> {
    crate::input::macos::get_screens_macos()
}

/// Detect if the cursor is at a screen boundary edge.
/// Returns (screen_id, edge_direction, position_ratio 0.0..1.0).
/// Only fires on real desktop boundary edges, not internal edges between local monitors.
pub fn detect_edge(x: i32, y: i32, screens: &[ScreenInfo]) -> Option<(String, EdgeHit, f64)> {
    const MARGIN: i32 = 1;

    for screen in screens {
        let sx = screen.x;
        let sy = screen.y;
        let sw = screen.width;
        let sh = screen.height;

        // Skip screens with zero dimensions to avoid division by zero.
        if sw == 0 || sh == 0 {
            continue;
        }

        // Check if cursor is within this screen's bounds (with margin)
        if x >= sx - MARGIN && x <= sx + sw + MARGIN && y >= sy - MARGIN && y <= sy + sh + MARGIN {
            if x <= sx + MARGIN {
                if is_boundary_edge(screen, EdgeHit::Left, screens) {
                    let ratio = (y - sy) as f64 / sh as f64;
                    return Some((screen.id.clone(), EdgeHit::Left, ratio.clamp(0.0, 1.0)));
                }
            }
            if x >= sx + sw - MARGIN {
                if is_boundary_edge(screen, EdgeHit::Right, screens) {
                    let ratio = (y - sy) as f64 / sh as f64;
                    return Some((screen.id.clone(), EdgeHit::Right, ratio.clamp(0.0, 1.0)));
                }
            }
            if y <= sy + MARGIN {
                if is_boundary_edge(screen, EdgeHit::Top, screens) {
                    let ratio = (x - sx) as f64 / sw as f64;
                    return Some((screen.id.clone(), EdgeHit::Top, ratio.clamp(0.0, 1.0)));
                }
            }
            if y >= sy + sh - MARGIN {
                if is_boundary_edge(screen, EdgeHit::Bottom, screens) {
                    let ratio = (x - sx) as f64 / sw as f64;
                    return Some((screen.id.clone(), EdgeHit::Bottom, ratio.clamp(0.0, 1.0)));
                }
            }
        }
    }
    None
}

/// Check if a screen edge is a real desktop boundary (no adjacent local monitor on that side).
fn is_boundary_edge(screen: &ScreenInfo, edge: EdgeHit, all_screens: &[ScreenInfo]) -> bool {
    const OVERLAP_THRESHOLD: i32 = 10;

    for other in all_screens {
        if other.id == screen.id {
            continue;
        }

        match edge {
            EdgeHit::Right => {
                // Another monitor's left edge is adjacent to our right edge
                let adjacent = (other.x - (screen.x + screen.width)).abs() <= 2;
                let y_overlap = range_overlap(
                    screen.y, screen.y + screen.height,
                    other.y, other.y + other.height,
                );
                if adjacent && y_overlap >= OVERLAP_THRESHOLD {
                    return false;
                }
            }
            EdgeHit::Left => {
                // Another monitor's right edge is adjacent to our left edge
                let adjacent = (screen.x - (other.x + other.width)).abs() <= 2;
                let y_overlap = range_overlap(
                    screen.y, screen.y + screen.height,
                    other.y, other.y + other.height,
                );
                if adjacent && y_overlap >= OVERLAP_THRESHOLD {
                    return false;
                }
            }
            EdgeHit::Bottom => {
                // Another monitor's top edge is adjacent to our bottom edge
                let adjacent = (other.y - (screen.y + screen.height)).abs() <= 2;
                let x_overlap = range_overlap(
                    screen.x, screen.x + screen.width,
                    other.x, other.x + other.width,
                );
                if adjacent && x_overlap >= OVERLAP_THRESHOLD {
                    return false;
                }
            }
            EdgeHit::Top => {
                // Another monitor's bottom edge is adjacent to our top edge
                let adjacent = ((screen.y) - (other.y + other.height)).abs() <= 2;
                let x_overlap = range_overlap(
                    screen.x, screen.x + screen.width,
                    other.x, other.x + other.width,
                );
                if adjacent && x_overlap >= OVERLAP_THRESHOLD {
                    return false;
                }
            }
        }
    }
    true
}

/// Calculate the overlap between two 1D ranges [a1,a2) and [b1,b2).
fn range_overlap(a1: i32, a2: i32, b1: i32, b2: i32) -> i32 {
    let start = a1.max(b1);
    let end = a2.min(b2);
    (end - start).max(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeHit {
    Left,
    Right,
    Top,
    Bottom,
}
