//! Decoder for RAIL Windowing Alternate Secondary Drawing Orders ([MS-RDPERP] 2.2.1.3).
//!
//! IronRDP validates these orders and hands them over as the raw slow-path `Orders` update
//! (`RdpOutputEvent::WindowingOrders`). Only the fields needed for window management are kept.

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub new: bool,
    pub owner: Option<u32>,
    /// `(style, extended_style)` as Win32 `WS_*` / `WS_EX_*` bits.
    pub style: Option<(u32, u32)>,
    /// 0 = hidden, 2 = minimized, 3 = maximized, 5 = shown.
    pub show: Option<u8>,
    pub title: Option<String>,
    pub offset: Option<(i32, i32)>,
    pub size: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowOrder {
    Window(WindowInfo),
    WindowDeleted(u32),
    Desktop {
        /// The server stopped monitoring the desktop: RemoteApp is no longer active.
        non_monitored: bool,
        active_window: Option<u32>,
        z_order: Option<Vec<u32>>,
    },
    /// Window icons and notification icons: not used.
    Ignored,
}

const WINDOW_TYPE: u32 = 0x0100_0000;
const NOTIFY_ICON_TYPE: u32 = 0x0200_0000;
const DESKTOP_TYPE: u32 = 0x0400_0000;
const TYPE_MASK: u32 = WINDOW_TYPE | NOTIFY_ICON_TYPE | DESKTOP_TYPE;
const STATE_NEW: u32 = 0x1000_0000;
const DELETED: u32 = 0x2000_0000;
const ICON: u32 = 0x4000_0000;
const CACHED_ICON: u32 = 0x8000_0000;

struct Cur<'a>(&'a [u8]);

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    fn unicode(&mut self) -> Option<String> {
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned())
    }
    fn rects(&mut self) -> Option<()> {
        let n = usize::from(self.u16()?);
        self.take(n * 8).map(|_| ())
    }
}

/// Decodes a slow-path `Orders` update (`updateType`, pad, `numberOrders`, pad, orders).
pub fn decode(update: &[u8]) -> Option<Vec<WindowOrder>> {
    let mut src = Cur(update);
    src.take(4)?;
    let count = src.u16()?;
    src.take(2)?;
    let mut orders = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        if src.u8()? != 0x2e {
            return None;
        }
        let size = usize::from(src.u16()?).checked_sub(3)?;
        orders.push(decode_order(Cur(src.take(size)?))?);
    }
    Some(orders)
}

fn decode_order(mut src: Cur<'_>) -> Option<WindowOrder> {
    let flags = src.u32()?;
    match flags & TYPE_MASK {
        WINDOW_TYPE => {
            let id = src.u32()?;
            if flags & DELETED != 0 {
                return Some(WindowOrder::WindowDeleted(id));
            }
            if flags & (ICON | CACHED_ICON) != 0 {
                return Some(WindowOrder::Ignored);
            }
            let mut w = WindowInfo { id, new: flags & STATE_NEW != 0, ..Default::default() };
            // Field order follows [MS-RDPERP] 2.2.1.3.1.2.1.
            if flags & 0x0002 != 0 {
                w.owner = Some(src.u32()?);
            }
            if flags & 0x0008 != 0 {
                w.style = Some((src.u32()?, src.u32()?));
            }
            if flags & 0x0010 != 0 {
                w.show = Some(src.u8()?);
            }
            if flags & 0x0004 != 0 {
                w.title = Some(src.unicode()?);
            }
            if flags & 0x4000 != 0 {
                src.take(8)?; // ClientOffset
            }
            if flags & 0x0001_0000 != 0 {
                src.take(8)?; // ClientAreaSize
            }
            if flags & 0x0080 != 0 {
                src.take(8)?; // HorizontalResizeMargins
            }
            if flags & 0x0800_0000 != 0 {
                src.take(8)?; // VerticalResizeMargins
            }
            if flags & 0x0002_0000 != 0 {
                src.u8()?; // RPContent
            }
            if flags & 0x0004_0000 != 0 {
                src.u32()?; // RootParentHandle
            }
            if flags & 0x0800 != 0 {
                w.offset = Some((src.i32()?, src.i32()?));
            }
            if flags & 0x8000 != 0 {
                src.take(8)?; // ClientDelta
            }
            if flags & 0x0400 != 0 {
                w.size = Some((src.u32()?, src.u32()?));
            }
            if flags & 0x0100 != 0 {
                src.rects()?; // WndRects
            }
            if flags & 0x1000 != 0 {
                src.take(8)?; // VisibleOffset
            }
            if flags & 0x0200 != 0 {
                src.rects()?; // VisibilityRects
            }
            // Remaining optional fields (overlay description, taskbar, z-order, appbar) are not used.
            Some(WindowOrder::Window(w))
        }
        DESKTOP_TYPE => {
            if flags & 1 != 0 {
                return Some(WindowOrder::Desktop { non_monitored: true, active_window: None, z_order: None });
            }
            let active_window = if flags & 0x20 != 0 { Some(src.u32()?) } else { None };
            let z_order = if flags & 0x10 != 0 {
                let n = src.u8()?;
                Some((0..n).map(|_| src.u32()).collect::<Option<Vec<_>>>()?)
            } else {
                None
            };
            Some(WindowOrder::Desktop { non_monitored: false, active_window, z_order })
        }
        _ => Some(WindowOrder::Ignored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(fields: u32, body: &[u8]) -> Vec<u8> {
        let mut o = vec![0x2e];
        o.extend_from_slice(&u16::try_from(3 + 4 + body.len()).unwrap().to_le_bytes());
        o.extend_from_slice(&fields.to_le_bytes());
        o.extend_from_slice(body);
        o
    }

    fn update(orders: &[Vec<u8>]) -> Vec<u8> {
        let mut u = vec![0, 0, 0, 0];
        u.extend_from_slice(&u16::try_from(orders.len()).unwrap().to_le_bytes());
        u.extend_from_slice(&[0, 0]);
        orders.iter().for_each(|o| u.extend_from_slice(o));
        u
    }

    #[test]
    fn decodes_new_window_and_delete() {
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_le_bytes()); // WindowId
        body.extend_from_slice(&1u32.to_le_bytes()); // OwnerWindowId
        body.extend_from_slice(&0x80C0_0000u32.to_le_bytes()); // Style
        body.extend_from_slice(&0x80u32.to_le_bytes()); // ExtendedStyle
        body.push(5); // ShowState
        body.extend_from_slice(&4u16.to_le_bytes()); // Title "Hi"
        body.extend_from_slice(&[b'H', 0, b'i', 0]);
        body.extend_from_slice(&(-10i32).to_le_bytes()); // WindowOffset x
        body.extend_from_slice(&20i32.to_le_bytes()); // y
        body.extend_from_slice(&300u32.to_le_bytes()); // WindowSize w
        body.extend_from_slice(&200u32.to_le_bytes()); // h
        body.extend_from_slice(&1u16.to_le_bytes()); // 1 visibility rect
        body.extend_from_slice(&[0; 8]);
        let create = order(WINDOW_TYPE | STATE_NEW | 0x0002 | 0x0008 | 0x0010 | 0x0004 | 0x0800 | 0x0400 | 0x0200, &body);
        let delete = order(WINDOW_TYPE | DELETED, &7u32.to_le_bytes());
        let desktop = order(DESKTOP_TYPE | 0x20 | 0x10, &[7, 0, 0, 0, 1, 7, 0, 0, 0]);

        let orders = decode(&update(&[create, delete, desktop])).unwrap();
        assert_eq!(
            orders[0],
            WindowOrder::Window(WindowInfo {
                id: 7,
                new: true,
                owner: Some(1),
                style: Some((0x80C0_0000, 0x80)),
                show: Some(5),
                title: Some("Hi".into()),
                offset: Some((-10, 20)),
                size: Some((300, 200)),
            })
        );
        assert_eq!(orders[1], WindowOrder::WindowDeleted(7));
        assert_eq!(
            orders[2],
            WindowOrder::Desktop { non_monitored: false, active_window: Some(7), z_order: Some(vec![7]) }
        );
    }

    #[test]
    fn rejects_truncated_order() {
        let truncated = order(WINDOW_TYPE | 0x0800, &7u32.to_le_bytes());
        assert!(decode(&update(&[truncated])).is_none());
    }
}
