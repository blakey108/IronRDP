//! Local presentation state of projected RemoteApp windows ([MS-RDPERP]).
//!
//! Holds what a projected window needs beyond its geometry: its icons, its shape,
//! the server's sizing constraints and local move/size state, and the pixels of a
//! graphics pipeline surface mapped to it ([MS-RDPEGFX] 2.2.2.20 RDPGFX_MAP_SURFACE_TO_WINDOW_PDU).
//! Also queries the local system parameters reported to the server.
//!
//! [MS-RDPERP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdperp
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx

use core::cell::{Cell, RefCell};
use core::ptr;
use std::rc::Rc;

use ironrdp_pdu::geometry::ExclusiveRectangle;
use ironrdp_pdu::window::{WindowPoint, WindowState};
use ironrdp_rail::pdu::{
    ClientSystemParameter, FilterKeys, HighContrast, MinMaxInfoPdu, MoveSizeType, StickyKeys, SystemParameterRectangle,
    ToggleKeys,
};
use windows::Win32::Foundation::{E_OUTOFMEMORY, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CombineRgn, CreateCompatibleDC, CreateDIBSection, CreateRectRgn,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, HRGN, RGN_OR, SRCCOPY, SelectObject, SetWindowRgn,
    StretchBlt,
};
use windows::Win32::UI::Accessibility::{FILTERKEYS, HIGHCONTRASTW, STICKYKEYS, TOGGLEKEYS};
use windows::Win32::UI::Shell::{ABM_GETTASKBARPOS, APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, HTCAPTION, ICON_BIG, ICON_SMALL, MINMAXINFO, SC_MOVE, SC_SIZE, SM_SWAPBUTTON, SPI_GETCARETWIDTH,
    SPI_GETDRAGFULLWINDOWS, SPI_GETFILTERKEYS, SPI_GETHIGHCONTRAST, SPI_GETKEYBOARDCUES, SPI_GETKEYBOARDPREF,
    SPI_GETSTICKYKEYS, SPI_GETTOGGLEKEYS, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_ACTION,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SendMessageW, SystemParametersInfoW, WM_SETICON,
};
use windows_core::{BOOL, Error};

use crate::rail_icons::OwnedIcon;

/// Largest RemoteApp surface edge kept locally.
const MAX_SURFACE_DIMENSION: u16 = 16384;
/// Largest RemoteApp surface area kept locally, in pixels (256 MiB of BGRA).
const MAX_SURFACE_PIXELS: usize = 64 * 1024 * 1024;

/// Pixels of a graphics pipeline surface mapped to a RemoteApp window, in window coordinates.
pub(crate) struct WindowUpdate {
    pub(crate) window_id: u64,
    /// Presented width of the window content.
    pub(crate) width: u16,
    /// Presented height of the window content.
    pub(crate) height: u16,
    pub(crate) region: ExclusiveRectangle,
    /// Tightly packed RGBA rows for `region`.
    pub(crate) data: Vec<u8>,
}

impl core::fmt::Debug for WindowUpdate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WindowUpdate")
            .field("window_id", &self.window_id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("region", &self.region)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// How a server-initiated local move or resize is driven ([MS-RDPERP] 3.2.5.2.6 Local Move/Resize Features).
///
/// [MS-RDPERP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdperp
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalMoveSizeMode {
    /// Driven by the mouse button the user holds; ending it releases that button on the server.
    Mouse,
    /// Driven by the keyboard after a System menu Move or Size command.
    Keyboard,
    /// Started locally from the window frame or System menu; the server only learns
    /// the final rectangle.
    Local,
}

/// Returns the `WM_SYSCOMMAND` that starts the local loop for a server move/size type.
///
/// The sizing types share the `WMSZ_*` values, so `SC_SIZE | type` selects the edge.
pub(crate) fn local_move_size_command(move_size_type: MoveSizeType) -> (usize, LocalMoveSizeMode) {
    match move_size_type {
        MoveSizeType::Move => ((SC_MOVE | HTCAPTION) as usize, LocalMoveSizeMode::Mouse),
        MoveSizeType::KeyMove => (SC_MOVE as usize, LocalMoveSizeMode::Keyboard),
        MoveSizeType::KeySize => (SC_SIZE as usize, LocalMoveSizeMode::Keyboard),
        edge => ((SC_SIZE | edge as u32) as usize, LocalMoveSizeMode::Mouse),
    }
}

/// Applies a Server Min Max Info PDU ([MS-RDPERP] 2.2.2.7.1) to a `WM_GETMINMAXINFO` request.
///
/// [MS-RDPERP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdperp
pub(crate) fn apply_min_max_info(info: &mut MINMAXINFO, pdu: &MinMaxInfoPdu) {
    let [
        max_width,
        max_height,
        max_x,
        max_y,
        min_track_width,
        min_track_height,
        max_track_width,
        max_track_height,
    ] = pdu.values.map(i32::from);
    info.ptMaxSize = POINT {
        x: max_width,
        y: max_height,
    };
    info.ptMaxPosition = POINT { x: max_x, y: max_y };
    info.ptMinTrackSize = POINT {
        x: min_track_width,
        y: min_track_height,
    };
    info.ptMaxTrackSize = POINT {
        x: max_track_width,
        y: max_track_height,
    };
}

/// The latest shape fields a window's orders carried; orders update them incrementally.
#[derive(Debug, Clone, Default)]
pub(crate) struct WindowShape {
    window_rects: Option<Vec<ExclusiveRectangle>>,
    visibility_rects: Option<Vec<ExclusiveRectangle>>,
    visible_offset: Option<WindowPoint>,
}

impl WindowShape {
    /// Records the shape fields an order carries; absent fields keep their value.
    pub(crate) fn update(&mut self, state: &WindowState) {
        if let Some(rects) = &state.window_rects {
            self.window_rects = Some(rects.clone());
        }
        if let Some(rects) = &state.visibility_rects {
            self.visibility_rects = Some(rects.clone());
        }
        if let Some(offset) = state.visible_offset {
            self.visible_offset = Some(offset);
        }
    }

    /// Returns the window region in window coordinates, or `None` for an unshaped window.
    ///
    /// The window shape is relative to the window origin. Without one, the visibility
    /// region, relative to the visible offset, is translated to the window origin.
    /// A single rectangle covering the whole window is no shape at all.
    pub(crate) fn region_rectangles(&self, window_x: i32, window_y: i32, width: i32, height: i32) -> Option<Vec<RECT>> {
        let to_rect = |rect: &ExclusiveRectangle, dx: i32, dy: i32| RECT {
            left: i32::from(rect.left) + dx,
            top: i32::from(rect.top) + dy,
            right: i32::from(rect.right) + dx,
            bottom: i32::from(rect.bottom) + dy,
        };
        let rects: Vec<RECT> = if let Some(rects) = &self.window_rects {
            rects.iter().map(|rect| to_rect(rect, 0, 0)).collect()
        } else {
            let (Some(rects), Some(offset)) = (&self.visibility_rects, self.visible_offset) else {
                return None;
            };
            let dx = offset.x.saturating_sub(window_x);
            let dy = offset.y.saturating_sub(window_y);
            rects.iter().map(|rect| to_rect(rect, dx, dy)).collect()
        };
        let whole_window = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        match rects.as_slice() {
            [] => None,
            [rect] if covers(rect, &whole_window) => None,
            _ => Some(rects),
        }
    }
}

fn covers(outer: &RECT, inner: &RECT) -> bool {
    outer.left <= inner.left && outer.top <= inner.top && inner.right <= outer.right && inner.bottom <= outer.bottom
}

/// Sets or removes a window region built from rectangles in window coordinates.
pub(crate) fn apply_window_region(hwnd: HWND, rects: Option<&[RECT]>) {
    let Some(rects) = rects else {
        // SAFETY: removing the region of a window this thread owns.
        unsafe { SetWindowRgn(hwnd, None, true) };
        return;
    };
    // SAFETY: GDI region calls on regions this function owns until SetWindowRgn
    // takes ownership of the combined one.
    unsafe {
        let region = CreateRectRgn(0, 0, 0, 0);
        if region.is_invalid() {
            return;
        }
        for rect in rects {
            let part = CreateRectRgn(rect.left, rect.top, rect.right, rect.bottom);
            if part.is_invalid() {
                continue;
            }
            CombineRgn(Some(region), Some(region), Some(part), RGN_OR);
            delete_region(part);
        }
        if SetWindowRgn(hwnd, Some(region), true) == 0 {
            tracing::debug!("Unable to set a RAIL window region");
            delete_region(region);
        }
    }
}

fn delete_region(region: HRGN) {
    // SAFETY: the region is owned by the caller.
    let _ = unsafe { DeleteObject(HGDIOBJ(region.0)) };
}

/// Cloaks or uncloaks a window with DWM ([MS-RDPERP] 2.2.2.12.1 Window Cloak State Change PDU).
///
/// [MS-RDPERP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdperp
pub(crate) fn set_window_cloak(hwnd: HWND, cloaked: bool) {
    let value = BOOL::from(cloaked);
    // SAFETY: `value` is a BOOL of the size passed, alive for the call.
    if let Err(error) = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK,
            ptr::from_ref(&value).cast(),
            u32::try_from(size_of::<BOOL>()).unwrap_or(4),
        )
    } {
        tracing::debug!(?error, "Unable to cloak a RAIL window");
    }
}

/// The local presentation state of one projected RemoteApp window.
#[derive(Default)]
pub(crate) struct RailWindowShell {
    /// Sizing constraints from the latest Server Min Max Info PDU.
    pub(crate) min_max: Cell<Option<MinMaxInfoPdu>>,
    /// The server-initiated local move or resize in progress.
    pub(crate) local_move_size: Cell<Option<LocalMoveSizeMode>>,
    /// Set while the client activates the window on the server's behalf, so the
    /// activation is not echoed back.
    pub(crate) suppress_activation: Cell<bool>,
    pub(crate) shape: RefCell<WindowShape>,
    /// The region last set on the window, so unchanged shapes are not reapplied.
    pub(crate) applied_region: RefCell<Option<Vec<RECT>>>,
    /// Pixels of a graphics pipeline surface mapped to this window.
    pub(crate) surface: RefCell<Option<Rc<RefCell<RailWindowSurface>>>>,
    big_icon: RefCell<Option<Rc<OwnedIcon>>>,
    small_icon: RefCell<Option<Rc<OwnedIcon>>>,
}

impl RailWindowShell {
    /// Sets the window's taskbar (big) or title bar (small) icon, keeping it alive while set.
    pub(crate) fn set_icon(&self, hwnd: HWND, big: bool, icon: Rc<OwnedIcon>) {
        let kind = if big { ICON_BIG } else { ICON_SMALL };
        // SAFETY: the window belongs to this thread; the icon outlives its use because
        // this state keeps it until it is replaced or the window is destroyed.
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(kind as usize)),
                Some(LPARAM(icon.handle().0 as isize)),
            );
        }
        let slot = if big { &self.big_icon } else { &self.small_icon };
        // UNTESTED: the replaced icon is destroyed only after WM_SETICON installed the new
        // one, so the window never references a destroyed icon. Keep this order.
        slot.replace(Some(icon));
    }
}

/// A RemoteApp window's graphics pipeline surface: a 32 bpp DIB section in a memory DC.
pub(crate) struct RailWindowSurface {
    device_context: HDC,
    bitmap: HBITMAP,
    previous_bitmap: HGDIOBJ,
    pixels: *mut u8,
    width: u16,
    height: u16,
}

impl RailWindowSurface {
    fn new(width: u16, height: u16) -> windows_core::Result<Self> {
        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or_default(),
                biWidth: i32::from(width),
                biHeight: -i32::from(height),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = ptr::null_mut();
        // SAFETY: `bitmap_info` describes a top-down 32 bpp DIB.
        let bitmap = unsafe { CreateDIBSection(None, &bitmap_info, DIB_RGB_COLORS, &mut pixels, None, 0)? };
        // SAFETY: creating a memory DC has no preconditions.
        let device_context = unsafe { CreateCompatibleDC(None) };
        if pixels.is_null() || device_context.is_invalid() {
            // SAFETY: both handles are owned here.
            unsafe {
                if !device_context.is_invalid() {
                    let _ = DeleteDC(device_context);
                }
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
            }
            return Err(Error::from_hresult(E_OUTOFMEMORY));
        }
        // SAFETY: selecting the owned bitmap into the owned DC.
        let previous_bitmap = unsafe { SelectObject(device_context, HGDIOBJ(bitmap.0)) };
        Ok(Self {
            device_context,
            bitmap,
            previous_bitmap,
            pixels: pixels.cast(),
            width,
            height,
        })
    }

    /// Returns the presented width and height.
    pub(crate) fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Returns the surface for `update`, recreating it when the presented size changed.
    pub(crate) fn for_update(current: Option<&Rc<RefCell<Self>>>, update: &WindowUpdate) -> Option<Rc<RefCell<Self>>> {
        if let Some(current) = current
            && current.borrow().width == update.width
            && current.borrow().height == update.height
        {
            return Some(Rc::clone(current));
        }
        if update.width == 0
            || update.height == 0
            || MAX_SURFACE_DIMENSION < update.width
            || MAX_SURFACE_DIMENSION < update.height
            || MAX_SURFACE_PIXELS < usize::from(update.width) * usize::from(update.height)
        {
            tracing::debug!(
                window_id = update.window_id,
                width = update.width,
                height = update.height,
                "Ignoring an oversized RAIL window surface"
            );
            return None;
        }
        match Self::new(update.width, update.height) {
            Ok(surface) => Some(Rc::new(RefCell::new(surface))),
            Err(error) => {
                tracing::debug!(
                    ?error,
                    window_id = update.window_id,
                    "Unable to allocate a RAIL window surface"
                );
                None
            }
        }
    }

    /// Writes an RGBA update region into the BGRA surface.
    pub(crate) fn apply(&mut self, update: &WindowUpdate) {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        // SAFETY: the DIB section holds `width * height * 4` bytes for the surface's lifetime.
        let pixels = unsafe { core::slice::from_raw_parts_mut(self.pixels, width * height * 4) };
        write_rgba_update(pixels, (width, height), &update.region, &update.data);
    }

    /// Stretches the `source` rectangle of the surface onto `destination` in `device_context`.
    pub(crate) fn paint(&self, device_context: HDC, destination: RECT, source: RECT) -> bool {
        // SAFETY: both DCs are valid for the duration of the call.
        unsafe {
            StretchBlt(
                device_context,
                destination.left,
                destination.top,
                destination.right - destination.left,
                destination.bottom - destination.top,
                Some(self.device_context),
                source.left,
                source.top,
                source.right - source.left,
                source.bottom - source.top,
                SRCCOPY,
            )
        }
        .as_bool()
    }
}

impl Drop for RailWindowSurface {
    fn drop(&mut self) {
        // SAFETY: restores and frees the handles this surface created.
        unsafe {
            SelectObject(self.device_context, self.previous_bitmap);
            let _ = DeleteDC(self.device_context);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
        }
    }
}

/// Writes a server-provided RGBA region into a BGRA canvas, clipped to the canvas.
///
/// The region and data come from the server: an inverted, empty or out-of-range
/// region writes nothing, and data shorter than the clipped rows writes nothing.
fn write_rgba_update(canvas: &mut [u8], (width, height): (usize, usize), region: &ExclusiveRectangle, data: &[u8]) {
    let left = usize::from(region.left).min(width);
    let right = usize::from(region.right).min(width);
    let top = usize::from(region.top).min(height);
    let bottom = usize::from(region.bottom).min(height);
    let source_width = usize::from(region.right.saturating_sub(region.left));
    if right <= left || bottom <= top || data.len() < source_width * (bottom - top) * 4 {
        return;
    }
    copy_rgba_region_as_bgra(canvas, width, data, source_width, (left, top, right, bottom));
}

/// Copies tightly packed RGBA rows into a BGRA canvas `canvas_width` pixels wide.
fn copy_rgba_region_as_bgra(
    canvas: &mut [u8],
    canvas_width: usize,
    source: &[u8],
    source_width: usize,
    (left, top, right, bottom): (usize, usize, usize, usize),
) {
    for (row, y) in (top..bottom).enumerate() {
        let source_row = &source[row * source_width * 4..][..(right - left) * 4];
        let target_row = &mut canvas[(y * canvas_width + left) * 4..][..(right - left) * 4];
        for (target, pixel) in target_row.chunks_exact_mut(4).zip(source_row.chunks_exact(4)) {
            target.copy_from_slice(&[pixel[2], pixel[1], pixel[0], 0xff]);
        }
    }
}

/// Queries the local system parameters reported to the server ([MS-RDPERP] 2.2.2.4.1
/// Client System Parameters Update PDU).
///
/// Values that cannot be read are omitted; the RAIL channel only sends the ones the
/// server advertised support for.
///
/// [MS-RDPERP]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdperp
pub(crate) fn local_system_parameters() -> Vec<ClientSystemParameter> {
    let mut parameters = Vec::new();
    if let Some(value) = get_bool(SPI_GETDRAGFULLWINDOWS) {
        parameters.push(ClientSystemParameter::FullWindowDrag(value));
    }
    if let Some(value) = get_bool(SPI_GETKEYBOARDCUES) {
        parameters.push(ClientSystemParameter::KeyboardCues(value));
    }
    if let Some(value) = get_bool(SPI_GETKEYBOARDPREF) {
        parameters.push(ClientSystemParameter::KeyboardPreference(value));
    }
    // SAFETY: GetSystemMetrics has no preconditions.
    parameters.push(ClientSystemParameter::MouseButtonSwap(
        unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0,
    ));
    let mut work_area = RECT::default();
    if get(SPI_GETWORKAREA, 0, &mut work_area)
        && let Some(work_area) = session_rectangle(work_area)
    {
        parameters.push(ClientSystemParameter::WorkArea(work_area));
    }
    let mut taskbar = APPBARDATA {
        cbSize: u32::try_from(size_of::<APPBARDATA>()).unwrap_or_default(),
        ..Default::default()
    };
    // SAFETY: `taskbar` is an initialized APPBARDATA with its size set.
    if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut taskbar) } != 0
        && let Some(taskbar) = session_rectangle(taskbar.rc)
    {
        parameters.push(ClientSystemParameter::TaskbarPosition(taskbar));
    }
    let mut high_contrast = HIGHCONTRASTW {
        cbSize: u32::try_from(size_of::<HIGHCONTRASTW>()).unwrap_or_default(),
        ..Default::default()
    };
    if get(SPI_GETHIGHCONTRAST, high_contrast.cbSize, &mut high_contrast) {
        // SAFETY: on success the scheme is null or a NUL-terminated string owned by the system.
        let color_scheme = if high_contrast.lpszDefaultScheme.is_null() {
            String::new()
        } else {
            unsafe { high_contrast.lpszDefaultScheme.to_string() }.unwrap_or_default()
        };
        parameters.push(ClientSystemParameter::HighContrast(HighContrast {
            flags: high_contrast.dwFlags.0 & 0x0000_001f,
            color_scheme,
        }));
    }
    let mut caret_width = 0u32;
    if get(SPI_GETCARETWIDTH, 0, &mut caret_width) && caret_width != 0 {
        parameters.push(ClientSystemParameter::CaretWidth(caret_width));
    }
    let mut sticky_keys = STICKYKEYS {
        cbSize: u32::try_from(size_of::<STICKYKEYS>()).unwrap_or_default(),
        ..Default::default()
    };
    if get(SPI_GETSTICKYKEYS, sticky_keys.cbSize, &mut sticky_keys) {
        parameters.push(ClientSystemParameter::StickyKeys(StickyKeys {
            flags: sticky_keys.dwFlags.0 & 0xffff_01ff,
        }));
    }
    let mut toggle_keys = TOGGLEKEYS {
        cbSize: u32::try_from(size_of::<TOGGLEKEYS>()).unwrap_or_default(),
        ..Default::default()
    };
    if get(SPI_GETTOGGLEKEYS, toggle_keys.cbSize, &mut toggle_keys) {
        parameters.push(ClientSystemParameter::ToggleKeys(ToggleKeys {
            flags: toggle_keys.dwFlags & 0x0000_001f,
        }));
    }
    let mut filter_keys = FILTERKEYS {
        cbSize: u32::try_from(size_of::<FILTERKEYS>()).unwrap_or_default(),
        ..Default::default()
    };
    if get(SPI_GETFILTERKEYS, filter_keys.cbSize, &mut filter_keys) {
        parameters.push(ClientSystemParameter::FilterKeys(FilterKeys {
            flags: filter_keys.dwFlags & 0x0000_007f,
            wait_time: filter_keys.iWaitMSec,
            delay_time: filter_keys.iDelayMSec,
            repeat_time: filter_keys.iRepeatMSec,
            bounce_time: filter_keys.iBounceMSec,
        }));
    }
    parameters
}

/// Converts a local screen rectangle into session coordinates.
///
/// Projected RemoteApp windows use local screen coordinates as session
/// coordinates, so only rectangles a TS_RECTANGLE_16 can carry are kept.
pub(crate) fn session_rectangle(rect: RECT) -> Option<SystemParameterRectangle> {
    Some(SystemParameterRectangle {
        left: u16::try_from(rect.left).ok()?,
        top: u16::try_from(rect.top).ok()?,
        right: u16::try_from(rect.right).ok()?,
        bottom: u16::try_from(rect.bottom).ok()?,
    })
}

fn get<T>(action: SYSTEM_PARAMETERS_INFO_ACTION, size: u32, value: &mut T) -> bool {
    // SAFETY: `value` is the structure `action` writes, with `size` bytes when the action needs it.
    unsafe {
        SystemParametersInfoW(
            action,
            size,
            Some(ptr::from_mut(value).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok()
}

fn get_bool(action: SYSTEM_PARAMETERS_INFO_ACTION) -> Option<bool> {
    let mut value = BOOL::default();
    get(action, 0, &mut value).then(|| value.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    // oracle: spec WinUser.h SC_MOVE = 0xF010, SC_SIZE = 0xF000, HTCAPTION = 2, WMSZ_BOTTOMRIGHT = 8
    #[test]
    fn server_move_size_types_start_the_matching_local_loop() {
        assert_eq!(
            local_move_size_command(MoveSizeType::Move),
            (0xf012, LocalMoveSizeMode::Mouse)
        );
        assert_eq!(
            local_move_size_command(MoveSizeType::BottomRight),
            (0xf008, LocalMoveSizeMode::Mouse)
        );
        assert_eq!(
            local_move_size_command(MoveSizeType::KeySize),
            (0xf000, LocalMoveSizeMode::Keyboard)
        );
    }

    // oracle: spec MS-RDPERP 2.2.2.7.1 field order (MaxWidth, MaxHeight, MaxPosX, MaxPosY, MinTrack*, MaxTrack*)
    #[test]
    fn min_max_info_fills_every_constraint() {
        let mut info = MINMAXINFO::default();
        apply_min_max_info(
            &mut info,
            &MinMaxInfoPdu {
                window_id: 1,
                values: [1920, 1040, -8, -8, 136, 39, 1936, 1056],
            },
        );

        assert_eq!((info.ptMaxSize.x, info.ptMaxSize.y), (1920, 1040));
        assert_eq!((info.ptMaxPosition.x, info.ptMaxPosition.y), (-8, -8));
        assert_eq!((info.ptMinTrackSize.x, info.ptMinTrackSize.y), (136, 39));
        assert_eq!((info.ptMaxTrackSize.x, info.ptMaxTrackSize.y), (1936, 1056));
    }

    // oracle: spec MS-RDPERP 2.2.1.3.1.2.1 WindowRects are relative to the window origin
    #[test]
    fn window_rects_take_precedence_over_visibility() {
        let mut shape = WindowShape::default();
        shape.update(&WindowState {
            window_rects: Some(vec![rect(0, 0, 10, 10), rect(10, 5, 20, 10)]),
            visibility_rects: Some(vec![rect(0, 0, 1, 1)]),
            visible_offset: Some(WindowPoint { x: 0, y: 0 }),
            ..WindowState::default()
        });

        assert_eq!(
            shape.region_rectangles(100, 100, 20, 10),
            Some(vec![
                RECT {
                    left: 0,
                    top: 0,
                    right: 10,
                    bottom: 10
                },
                RECT {
                    left: 10,
                    top: 5,
                    right: 20,
                    bottom: 10
                },
            ])
        );
    }

    // oracle: spec MS-RDPERP 2.2.1.3.1.2.1 VisibilityRects are relative to VisibleOffset
    #[test]
    fn visibility_region_is_translated_into_window_coordinates() {
        let mut shape = WindowShape::default();
        shape.update(&WindowState {
            visibility_rects: Some(vec![rect(0, 0, 5, 5), rect(5, 0, 10, 3)]),
            visible_offset: Some(WindowPoint { x: 110, y: 104 }),
            ..WindowState::default()
        });

        assert_eq!(
            shape.region_rectangles(100, 100, 20, 20),
            Some(vec![
                RECT {
                    left: 10,
                    top: 4,
                    right: 15,
                    bottom: 9
                },
                RECT {
                    left: 15,
                    top: 4,
                    right: 20,
                    bottom: 7
                },
            ])
        );
    }

    // oracle: hand-derived: one rectangle equal to the window bounds clips nothing
    #[test]
    fn a_rectangle_covering_the_window_is_no_shape() {
        let mut shape = WindowShape::default();
        shape.update(&WindowState {
            window_rects: Some(vec![rect(0, 0, 20, 10)]),
            ..WindowState::default()
        });

        assert_eq!(shape.region_rectangles(0, 0, 20, 10), None);
    }

    // oracle: hand-derived: absent fields in a later order keep the earlier shape
    #[test]
    fn orders_without_shape_fields_keep_the_shape() {
        let mut shape = WindowShape::default();
        shape.update(&WindowState {
            window_rects: Some(vec![rect(0, 0, 5, 5)]),
            ..WindowState::default()
        });
        shape.update(&WindowState::default());

        assert_eq!(
            shape.region_rectangles(0, 0, 20, 10),
            Some(vec![RECT {
                left: 0,
                top: 0,
                right: 5,
                bottom: 5
            }])
        );
    }

    // oracle: hand-derived RGBA to BGRA byte swap with an opaque alpha
    #[test]
    fn surface_regions_are_converted_to_opaque_bgra() {
        let mut canvas = vec![0u8; 3 * 2 * 4];
        let source = [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];

        copy_rgba_region_as_bgra(&mut canvas, 3, &source, 2, (1, 0, 3, 2));

        assert_eq!(
            canvas,
            [
                0, 0, 0, 0, 3, 2, 1, 0xff, 6, 5, 4, 0xff, //
                0, 0, 0, 0, 9, 8, 7, 0xff, 12, 11, 10, 0xff,
            ]
        );
    }

    // oracle: hand-derived: well-formed region written at its offset
    #[test]
    fn update_regions_are_written_at_their_offset() {
        let mut canvas = vec![0u8; 2 * 2 * 4];

        write_rgba_update(&mut canvas, (2, 2), &rect(1, 1, 2, 2), &[1, 2, 3, 0]);

        assert_eq!(canvas, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 1, 0xff]);
    }

    // oracle: hand-derived: rows keep the region's own stride while columns and rows are clipped
    #[test]
    fn update_regions_past_the_surface_are_clipped() {
        let mut canvas = vec![0u8; 2 * 2 * 4];
        // A 2x2 region at (1, 1): only its top-left pixel lands on the 2x2 surface.
        let data = [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];

        write_rgba_update(&mut canvas, (2, 2), &rect(1, 1, 3, 3), &data);

        assert_eq!(canvas, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 1, 0xff]);
    }

    // oracle: hand-derived: malformed server regions or data must not write
    #[test]
    fn malformed_update_regions_write_nothing() {
        let cases: [(&str, ExclusiveRectangle, usize); 5] = [
            ("inverted", rect(2, 0, 1, 2), 16),
            ("empty", rect(1, 1, 1, 2), 16),
            ("left of nothing", rect(2, 0, 4, 2), 32),
            ("below the surface", rect(0, 2, 2, 4), 32),
            ("truncated data", rect(0, 0, 2, 2), 12),
        ];

        for (name, region, data_len) in cases {
            let mut canvas = vec![0u8; 2 * 2 * 4];
            write_rgba_update(&mut canvas, (2, 2), &region, &vec![0x7f; data_len]);
            assert!(canvas.iter().all(|byte| *byte == 0), "{name}");
        }
    }

    // oracle: hand-derived: TS_RECTANGLE_16 carries unsigned 16-bit edges
    #[test]
    fn negative_screen_rectangles_are_not_reported() {
        assert_eq!(
            session_rectangle(RECT {
                left: -1920,
                top: 0,
                right: 0,
                bottom: 1080
            }),
            None
        );
        assert_eq!(
            session_rectangle(RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040
            }),
            Some(SystemParameterRectangle {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040
            })
        );
    }
}
