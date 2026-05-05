use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle, Circle};
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::pixelcolor::BinaryColor;

use crate::libs::network::status::NetworkStatus;

// PUBLIC API

/// Draw network connection status icon
/// Returns the width of the drawn icon for layout calculations
pub fn draw_network_status<D>(
    display: &mut D,
    x: i32,
    y: i32,
    status: &NetworkStatus,
) -> u32
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    if status.ethernet_connected {
        draw_ethernet(display, x, y);
        12
    } else if status.wifi_connected {
        match status.wifi_signal_strength {
            rssi if rssi > -67 => {
                draw_wifi_strong(display, x, y);
            }
            rssi if rssi > -80 => {
                draw_wifi_medium(display, x, y);
            }
            _ => {
                draw_wifi_weak(display, x, y);
            }
        }
        11 // width of the Wi-Fi bitmap below
    } else {
        draw_no_connection(display, x, y+1);
        11 // width of the X bitmap
    }
}

// WIFI BITMAPS – EXACT STYLE FROM YOUR IMAGE

const WIFI_STRONG_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 1, 1, 1, 1, 1, 1, 1, 0, 0],
    [0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0],
    [1, 0, 0, 1, 1, 1, 1, 1, 0, 0, 1],
    [0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
];

/// Medium = same style, without the outermost arc
const WIFI_MEDIUM_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0],
    [0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
];

/// Weak = inner arc + dot, same pixel style
const WIFI_WEAK_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
];


// NO CONNECTION BITMAP – X SYMBOL

const NO_CONNECTION_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 
    [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],  
    [0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0], 
    [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0], 
    [0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0],  
    [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],  
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],  
];

// Generic helper to draw any of the bitmaps above
fn draw_wifi_bitmap<D>(
    display: &mut D,
    x: i32,
    y: i32,
    bitmap: &[[u8; 11]; 7],
) where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    // We draw horizontal runs instead of single pixels to keep it efficient.
    let style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    for (yy, row) in bitmap.iter().enumerate() {
        let mut run_start: Option<usize> = None;

        for (xx, &pix) in row.iter().enumerate() {
            let on = pix != 0;

            match (run_start, on) {
                (None, true) => {
                    // starting a new run
                    run_start = Some(xx);
                }
                (Some(start), false) => {
                    // end of a run: draw it
                    let x0 = x + start as i32;
                    let x1 = x + (xx as i32) - 1;
                    let y0 = y + yy as i32;

                    let _ = Line::new(Point::new(x0, y0), Point::new(x1, y0))
                        .into_styled(style)
                        .draw(display);
                    run_start = None;
                }
                _ => {}
            }
        }

        // If the row ended while we were still in a run, draw the last one
        if let Some(start) = run_start {
            let x0 = x + start as i32;
            let x1 = x + (row.len() as i32) - 1;
            let y0 = y + yy as i32;

            let _ = Line::new(Point::new(x0, y0), Point::new(x1, y0))
                .into_styled(style)
                .draw(display);
        }
    }
}

// Public Wi-Fi drawing functions

fn draw_wifi_strong<D>(display: &mut D, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    draw_wifi_bitmap(display, x, y+1, &WIFI_STRONG_BITMAP);
}

fn draw_wifi_medium<D>(display: &mut D, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    draw_wifi_bitmap(display, x, y+1, &WIFI_MEDIUM_BITMAP);
}

fn draw_wifi_weak<D>(display: &mut D, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    draw_wifi_bitmap(display, x, y+1, &WIFI_WEAK_BITMAP);
}

fn draw_no_connection<D>(display: &mut D, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    draw_wifi_bitmap(display, x, y+1, &NO_CONNECTION_BITMAP);
}

// ETHERNET BITMAP – MATCHING WIFI DIMENSIONS
// grid: 11 (width) x 7 (height)

const ETHERNET_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 
    [0, 0, 1, 1, 1, 1, 1, 1, 1, 0, 0], 
    [0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0], 
    [0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0],
    [0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0], 
    [0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0], 
];

fn draw_ethernet<D>(display: &mut D, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    draw_ethernet_bitmap(display, x, y+1, &ETHERNET_BITMAP);
}

// LORAWAN RADIO TOWER BITMAP – 11x7 matching other icon dimensions
const LORAWAN_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
    [0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0],
    [0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0],
    [0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0],
    [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0],
];

/// Draw LoRaWAN radio tower icon
pub fn draw_lorawan<D>(display: &mut D, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    draw_wifi_bitmap(display, x, y, &LORAWAN_BITMAP);
}

fn draw_ethernet_bitmap<D>(
    display: &mut D,
    x: i32,
    y: i32,
    bitmap: &[[u8; 11]; 7],
) where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    let style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    for (yy, row) in bitmap.iter().enumerate() {
        let mut run_start: Option<usize> = None;

        for (xx, &pix) in row.iter().enumerate() {
            let on = pix != 0;

            match (run_start, on) {
                (None, true) => {
                    run_start = Some(xx);
                }
                (Some(start), false) => {
                    let x0 = x + start as i32;
                    let x1 = x + (xx as i32) - 1;
                    let y0 = y + yy as i32;

                    let _ = Line::new(Point::new(x0, y0), Point::new(x1, y0))
                        .into_styled(style)
                        .draw(display);
                    run_start = None;
                }
                _ => {}
            }
        }

        if let Some(start) = run_start {
            let x0 = x + start as i32;
            let x1 = x + (row.len() as i32) - 1;
            let y0 = y + yy as i32;

            let _ = Line::new(Point::new(x0, y0), Point::new(x1, y0))
                .into_styled(style)
                .draw(display);
        }
    }
}

// MUTE ICON BITMAP – 11x7 matching other icon dimensions
// Speaker with X overlay, user-designed pixel art
const MUTE_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 1, 1, 0, 0, 1, 0, 0, 0, 1],
    [1, 1, 1, 1, 0, 0, 0, 1, 0, 1, 0],
    [1, 1, 1, 1, 0, 0, 0, 0, 1, 0, 0],
    [1, 1, 1, 1, 0, 0, 0, 1, 0, 1, 0],
    [0, 0, 1, 1, 0, 0, 1, 0, 0, 0, 1],
    [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
];

/// Draw mute icon (speaker with X)
/// Returns the width of the drawn icon for layout calculations
pub fn draw_mute<D>(display: &mut D, x: i32, y: i32) -> u32
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    draw_wifi_bitmap(display, x, y, &MUTE_BITMAP);
    11
}