#![allow(clippy::print_stderr, clippy::print_stdout)] // allowed in this module only

use core::num::NonZeroU32;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use ironrdp::client::rail::RailInputEvent;
use ironrdp::client::rdp::{AutoReconnectDecision, RdpInputEvent, RdpInputSender, RdpOutputEvent};
use ironrdp_daemon::daemon::{Daemon, ResizeError};
use ironrdp_rail::pdu::{ActivatePdu, SystemCommand, SystemCommandPdu};
use raw_window_handle::{DisplayHandle, HasDisplayHandle as _};
use smallvec::SmallVec;
use tracing::{debug, error, info, trace, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, PhysicalSize};
use winit::event::{self, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Cursor, CursorIcon, CustomCursor, Window, WindowAttributes};

use crate::keymap::{is_modifier, map_key_code};
use crate::rail::{RailWindows, window_point_to_desktop};

type WindowSurface = (Arc<Window>, softbuffer::Surface<DisplayHandle<'static>, Arc<Window>>);

/// Events delivered from the viewer-hosted RPC server to the window.
pub enum ViewerEvent {
    FrameAvailable,
    Shutdown,
}

/// Where local window input is sent.
enum InputTarget {
    Direct(RdpInputSender),
    Rpc(Arc<Daemon>),
}

/// A viewer application driven by the RDP client's native output events.
pub struct App {
    inner: RpcApp,
}

/// A viewer application driven by a viewer-hosted RPC daemon.
pub struct RpcApp {
    input_target: InputTarget,
    frame_wakeup: Option<Arc<AtomicBool>>,
    context: softbuffer::Context<DisplayHandle<'static>>,
    initial_window_size: PhysicalSize<u32>,
    window: Option<WindowSurface>,
    buffer: Vec<u32>,
    buffer_size: (u16, u16),
    input_database: ironrdp::input::Database,
    last_size: Option<PhysicalSize<u32>>,
    resize_timeout: Option<Instant>,
    /// RemoteApp mode: server windows open as local windows and no desktop window is shown.
    remote_app: bool,
    rail_windows: RailWindows,
}

impl App {
    /// Creates the viewer; with `remote_app`, each RemoteApp window opens as a local window
    /// instead of a desktop window.
    pub fn new(
        event_loop: &EventLoop<RdpOutputEvent>,
        input_event_sender: &RdpInputSender,
        initial_window_size: PhysicalSize<u32>,
        remote_app: bool,
    ) -> anyhow::Result<Self> {
        let inner = RpcApp::new_inner(
            event_loop,
            InputTarget::Direct(input_event_sender.clone()),
            None,
            initial_window_size,
            remote_app,
        )?;
        Ok(Self { inner })
    }
}

impl RpcApp {
    pub fn new(
        event_loop: &EventLoop<ViewerEvent>,
        daemon: Arc<Daemon>,
        frame_wakeup: Arc<AtomicBool>,
        initial_window_size: PhysicalSize<u32>,
    ) -> anyhow::Result<Self> {
        // RemoteApp windows need the client's RAIL output, which the RPC daemon does not expose.
        Self::new_inner(
            event_loop,
            InputTarget::Rpc(daemon),
            Some(frame_wakeup),
            initial_window_size,
            false,
        )
    }

    fn new_inner<T: 'static>(
        event_loop: &EventLoop<T>,
        input_target: InputTarget,
        frame_wakeup: Option<Arc<AtomicBool>>,
        initial_window_size: PhysicalSize<u32>,
        remote_app: bool,
    ) -> anyhow::Result<Self> {
        // SAFETY: We drop the softbuffer context right before the event loop is stopped, thus making this safe.
        // FIXME: This is not a sufficient proof and the API is actually unsound as-is.
        let display_handle = unsafe {
            core::mem::transmute::<DisplayHandle<'_>, DisplayHandle<'static>>(
                event_loop.display_handle().context("get display handle")?,
            )
        };
        let context = softbuffer::Context::new(display_handle)
            .map_err(|e| anyhow::anyhow!("unable to initialize softbuffer context: {e}"))?;

        let input_database = ironrdp::input::Database::new();
        Ok(Self {
            input_target,
            frame_wakeup,
            context,
            initial_window_size,
            window: None,
            buffer: Vec::new(),
            buffer_size: (0, 0),
            input_database,
            last_size: None,
            resize_timeout: None,
            remote_app,
            rail_windows: RailWindows::default(),
        })
    }

    fn send_resize_event(&mut self) {
        let Some(size) = self.last_size else {
            return;
        };
        let Some((window, _)) = self.window.as_mut() else {
            return;
        };
        // The viewer window spans a single monitor, so the session gets one primary monitor at the
        // window DPI. A scale outside 100..=500 percent is clamped when the layout is encoded.
        #[expect(clippy::as_conversions, reason = "casting f64 to u32")]
        let scale_factor = (window.scale_factor() * 100.0).round() as u32;

        let width = u16::try_from(size.width).expect("reasonable width");
        let height = u16::try_from(size.height).expect("reasonable height");

        match &self.input_target {
            InputTarget::Direct(input_event_sender) => match input_event_sender.try_send(RdpInputEvent::Resize {
                width,
                height,
                scale_factor,
                // TODO: it should be possible to get the physical size here, however winit doesn't make it straightforward.
                // FreeRDP does it based on DPI reading grabbed via [`SDL_GetDisplayDPI`](https://wiki.libsdl.org/SDL2/SDL_GetDisplayDPI):
                // https://github.com/FreeRDP/FreeRDP/blob/ba8cf8cf2158018fb7abbedb51ab245f369be813/client/SDL/sdl_monitor.cpp#L250-L262
                // See also: https://github.com/rust-windowing/winit/issues/826
                physical_size: None,
            }) {
                Ok(()) => self.last_size = None,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.resize_timeout = Some(Instant::now() + Duration::from_millis(10));
                }
                Err(_) => {
                    self.last_size = None;
                    warn!("Unable to enqueue resize event because the RDP session is closed");
                }
            },
            InputTarget::Rpc(daemon) => match daemon.try_resize(width, height, scale_factor) {
                Ok(()) => self.last_size = None,
                Err(ResizeError::Full) => self.resize_timeout = Some(Instant::now() + Duration::from_millis(10)),
                Err(error) => {
                    self.last_size = None;
                    warn!(?error, "Unable to resize the RPC-backed RDP session");
                }
            },
        }
    }

    fn update_rpc_frame(&mut self) {
        let InputTarget::Rpc(daemon) = &self.input_target else {
            return;
        };
        let Some(frame) = daemon.current_frame() else {
            return;
        };
        let (Some(width), Some(height)) = (
            NonZeroU32::new(u32::from(frame.width)),
            NonZeroU32::new(u32::from(frame.height)),
        ) else {
            return;
        };
        self.update_frame(frame.pixels, width, height);
    }

    fn update_frame(&mut self, buffer: Vec<u32>, width: NonZeroU32, height: NonZeroU32) {
        let Some((window, surface)) = self.window.as_mut() else {
            return;
        };
        trace!(?width, ?height, "Received RPC-backed image");
        self.buffer_size = (
            u16::try_from(width.get()).expect("frame width fits in u16"),
            u16::try_from(height.get()).expect("frame height fits in u16"),
        );
        self.buffer = buffer;
        surface.resize(width, height).expect("surface resize");
        window.request_redraw();
    }

    fn draw(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let Some((_, surface)) = self.window.as_mut() else {
            return;
        };
        let mut sb_buffer = surface.buffer_mut().expect("surface buffer");
        sb_buffer.copy_from_slice(self.buffer.as_slice());
        sb_buffer.present().expect("buffer present");
    }
    fn on_about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(timeout) = self.resize_timeout {
            if let Some(timeout) = timeout.checked_duration_since(Instant::now()) {
                event_loop.set_control_flow(ControlFlow::wait_duration(timeout));
            } else {
                self.resize_timeout = None;
                self.send_resize_event();
                if let Some(retry_timeout) = self.resize_timeout {
                    event_loop.set_control_flow(ControlFlow::wait_duration(
                        retry_timeout.saturating_duration_since(Instant::now()),
                    ));
                } else {
                    event_loop.set_control_flow(ControlFlow::Wait);
                }
            }
        }
    }

    fn on_resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.remote_app || self.window.is_some() {
            return;
        }
        let window_attributes = WindowAttributes::default()
            .with_title("IronRDP")
            .with_inner_size(self.initial_window_size);
        match event_loop.create_window(window_attributes) {
            Ok(window) => {
                let window = Arc::new(window);
                if matches!(&self.input_target, InputTarget::Rpc(_)) {
                    window.set_cursor_visible(false);
                }
                let surface = softbuffer::Surface::new(&self.context, Arc::clone(&window)).expect("surface");
                self.window = Some((window, surface));
            }
            Err(error) => {
                error!(%error, "Failed to create window");
                event_loop.exit();
            }
        }
    }

    fn on_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some((window, _)) = self.window.as_mut().filter(|(window, _)| window.id() == window_id) else {
            if let Some((rail_window_id, _)) = self.rail_windows.find(window_id) {
                self.on_rail_window_event(rail_window_id, event);
            }
            return;
        };

        match event {
            WindowEvent::Resized(size) => {
                self.last_size = Some(size);
                self.resize_timeout = Some(Instant::now() + Duration::from_secs(1));
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // The window moved to a monitor with another DPI, or the monitor DPI changed.
                // Send the layout again with the matching desktop scale factor; winit usually
                // follows with a `Resized` event, which refreshes the size within the same debounce.
                self.last_size = Some(window.inner_size());
                self.resize_timeout = Some(Instant::now() + Duration::from_secs(1));
            }
            WindowEvent::CloseRequested => match &self.input_target {
                InputTarget::Direct(input_event_sender) => input_event_sender.request_graceful_close(),
                InputTarget::Rpc(daemon) => {
                    let _ = daemon.disconnect();
                    daemon.shutdown();
                    event_loop.exit();
                }
            },
            WindowEvent::DroppedFile(_) => {
                // TODO(#110): File upload
            }
            // WindowEvent::ReceivedCharacter(_) => {
            // Sadly, we can't use this winit event to send RDP unicode events because
            // of the several reasons:
            // 1. `ReceivedCharacter` event doesn't provide a way to distinguish between
            //    key press and key release, therefore the only way to use it is to send
            //    a key press + release events sequentially, which will not allow to
            //    handle long press and key repeat events.
            // 2. This event do not fire for non-printable keys (e.g. Control, Alt, etc.)
            // 3. This event fies BEFORE `KeyboardInput` event, so we can't make a
            //    reasonable workaround for `1` and `2` by collecting physical key press
            //    information first via `KeyboardInput` before processing `ReceivedCharacter`.
            //
            // However, all of these issues can be solved by updating `winit` to the
            // newer version.
            //
            // TODO(#376): Update winit
            // TODO(#376): Implement unicode input in native client
            // }
            input @ (WindowEvent::KeyboardInput { .. }
            | WindowEvent::ModifiersChanged(_)
            | WindowEvent::CursorMoved { .. }
            | WindowEvent::MouseWheel { .. }
            | WindowEvent::MouseInput { .. }) => {
                let win_size = window.inner_size();
                let buffer_size = self.buffer_size;
                // The desktop window scales the session frame to its client area.
                let to_session = |x: f64, y: f64| {
                    #[expect(clippy::as_conversions, reason = "casting f64 to u16")]
                    let x = (x / f64::from(win_size.width) * f64::from(buffer_size.0)) as u16;
                    #[expect(clippy::as_conversions, reason = "casting f64 to u16")]
                    let y = (y / f64::from(win_size.height) * f64::from(buffer_size.1)) as u16;
                    Some(ironrdp::input::MousePosition { x, y })
                };
                forward_input_event(&self.input_target, &mut self.input_database, &input, to_session);
            }
            WindowEvent::RedrawRequested => {
                self.draw();
            }
            WindowEvent::ActivationTokenDone { .. }
            | WindowEvent::Moved(_)
            | WindowEvent::Destroyed
            | WindowEvent::HoveredFile(_)
            | WindowEvent::HoveredFileCancelled
            | WindowEvent::Focused(_)
            | WindowEvent::Ime(_)
            | WindowEvent::CursorEntered { .. }
            | WindowEvent::CursorLeft { .. }
            | WindowEvent::PinchGesture { .. }
            | WindowEvent::PanGesture { .. }
            | WindowEvent::DoubleTapGesture { .. }
            | WindowEvent::RotationGesture { .. }
            | WindowEvent::TouchpadPressure { .. }
            | WindowEvent::AxisMotion { .. }
            | WindowEvent::Touch(_)
            | WindowEvent::ThemeChanged(_)
            | WindowEvent::Occluded(_) => {
                // ignore
            }
        }
    }

    fn handle_viewer_event(&mut self, event_loop: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::FrameAvailable => {
                if let Some(frame_wakeup) = &self.frame_wakeup {
                    frame_wakeup.store(false, Ordering::Release);
                }
                self.update_rpc_frame();
            }
            ViewerEvent::Shutdown => event_loop.exit(),
        }
    }

    /// Handles input and painting for a local RemoteApp window.
    fn on_rail_window_event(&mut self, rail_window_id: u32, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested | WindowEvent::Resized(_) => {
                self.rail_windows.draw(rail_window_id, &self.buffer, self.buffer_size);
            }
            WindowEvent::CloseRequested => {
                // The server owns the window's lifetime and answers with a Deleted Window order.
                self.send_rail_input(RailInputEvent::SystemCommand(SystemCommandPdu {
                    window_id: rail_window_id,
                    command: SystemCommand::Close,
                }));
            }
            WindowEvent::Focused(focused) => {
                self.send_rail_input(RailInputEvent::Activate(ActivatePdu {
                    window_id: rail_window_id,
                    enabled: focused,
                }));
                if !focused {
                    // Keys and buttons held when focus moves away would otherwise stay
                    // pressed on the server, because their release goes to another window.
                    release_all_input(&self.input_target, &mut self.input_database);
                }
            }
            input @ (WindowEvent::KeyboardInput { .. }
            | WindowEvent::ModifiersChanged(_)
            | WindowEvent::CursorMoved { .. }
            | WindowEvent::MouseWheel { .. }
            | WindowEvent::MouseInput { .. }) => {
                // A RemoteApp window shows its server rectangle 1:1 at the server position.
                let origin = self.rail_windows.model(rail_window_id).map(|model| (model.x, model.y));
                let to_session = |x: f64, y: f64| origin.and_then(|origin| window_point_to_desktop(origin, x, y));
                forward_input_event(&self.input_target, &mut self.input_database, &input, to_session);
            }
            _ => {}
        }
    }

    fn send_rail_input(&self, event: RailInputEvent) {
        if let InputTarget::Direct(input_event_sender) = &self.input_target
            && let Err(error) = input_event_sender.try_send_rail_input(event)
        {
            debug!(%error, "Unable to send RAIL input");
        }
    }

    /// Applies a server pointer shape to every local window and shows the pointer.
    fn set_cursor(&mut self, cursor: Cursor) {
        if let Some((window, _)) = &self.window {
            window.set_cursor(cursor.clone());
            window.set_cursor_visible(true);
        }
        self.rail_windows.set_cursor(cursor);
        self.rail_windows.set_cursor_visible(true);
    }

    fn handle_output_event(&mut self, event_loop: &ActiveEventLoop, event: RdpOutputEvent) {
        match event {
            RdpOutputEvent::Connected => info!("RDP session connected"),
            RdpOutputEvent::MonitorLayout(monitors) => {
                debug!(monitor_count = monitors.len(), "Received remote monitor layout");
            }
            RdpOutputEvent::MonitorLayoutRejected => {
                warn!("The server cannot accept the requested monitor layout");
            }
            RdpOutputEvent::LoginComplete => info!("RDP login complete"),
            RdpOutputEvent::PostLogonDisplayRedraw => info!("Requested post-logon display redraw"),
            RdpOutputEvent::MalformedBitmapDisplayRedraw => {
                warn!("Requested display redraw after discarding a malformed bitmap update");
            }
            RdpOutputEvent::Image { buffer, width, height } => {
                trace!(width = ?width, height = ?height, "Received image with size");
                self.buffer_size = (width.get(), height.get());
                self.buffer = buffer;
                if let Some((window, surface)) = self.window.as_mut() {
                    trace!(window_physical_size = ?window.inner_size(), "Drawing image to the window with size");
                    surface
                        .resize(NonZeroU32::from(width), NonZeroU32::from(height))
                        .expect("surface resize");
                    window.request_redraw();
                }
                self.rail_windows.request_redraw_all();
            }
            RdpOutputEvent::ConnectionFailure(error) => {
                error!(?error);
                eprintln!("Connection error: {}", error.report().with_locations());
                // TODO set proc_exit::sysexits::PROTOCOL_ERR.as_raw());
                event_loop.exit();
            }
            RdpOutputEvent::Terminated(result) => {
                let _exit_code = match result {
                    Ok(reason) => {
                        println!("Terminated gracefully: {reason}");
                        proc_exit::sysexits::OK
                    }
                    Err(error) => {
                        error!(?error);
                        eprintln!("Active session error: {}", error.report().with_locations());
                        proc_exit::sysexits::PROTOCOL_ERR
                    }
                };
                // TODO set exit_code.as_raw());
                event_loop.exit();
            }
            RdpOutputEvent::PointerHidden => {
                if let Some((window, _)) = &self.window {
                    window.set_cursor_visible(false);
                }
                self.rail_windows.set_cursor_visible(false);
            }
            RdpOutputEvent::PointerDefault => {
                self.set_cursor(Cursor::from(CursorIcon::default()));
            }
            RdpOutputEvent::PointerPosition { x, y } => {
                // A RemoteApp session has no single window the desktop position maps into.
                if let Some((window, _)) = &self.window
                    && let Err(error) = window.set_cursor_position(LogicalPosition::new(x, y))
                {
                    error!(?error, "Failed to set cursor position");
                }
            }
            RdpOutputEvent::PointerBitmap(pointer) => {
                debug!(width = ?pointer.width, height = ?pointer.height, "Received pointer bitmap");
                match CustomCursor::from_rgba(
                    pointer.bitmap_data.clone(),
                    pointer.width,
                    pointer.height,
                    pointer.hotspot_x,
                    pointer.hotspot_y,
                ) {
                    Ok(cursor) => self.set_cursor(Cursor::from(event_loop.create_custom_cursor(cursor))),
                    Err(error) => error!(?error, "Failed to set cursor bitmap"),
                }
            }
            RdpOutputEvent::DisplayResizeFallback(reason) => {
                warn!(
                    ?reason,
                    "Reconnecting because dynamic display resize could not complete"
                );
            }
            RdpOutputEvent::AutoReconnecting {
                attempt,
                maximum_attempts,
                response,
                ..
            } => {
                warn!(attempt, maximum_attempts, "Stopping unsupported automatic reconnect");
                let _ = response.send(AutoReconnectDecision::Stop);
            }
            RdpOutputEvent::AutoReconnected => {
                info!("RDP session automatically reconnected");
            }
            RdpOutputEvent::RailHandshake {
                handshake_ex_flags,
                initialization_message_count,
                queued_execute_count,
            } => {
                debug!(
                    ?handshake_ex_flags,
                    initialization_message_count, queued_execute_count, "RAIL static channel initialized"
                );
            }
            RdpOutputEvent::RailDesktopSynchronized { released_execute_count } => {
                debug!(
                    released_execute_count,
                    "RAIL queued input released after desktop synchronization"
                );
            }
            RdpOutputEvent::RailPostHandshakeQueueReleased { released_execute_count } => {
                debug!(
                    released_execute_count,
                    "RAIL queued input released after handshake fallback"
                );
            }
            RdpOutputEvent::RailExecuteResult(result) => {
                debug!(?result, "RAIL execute completed");
            }
            RdpOutputEvent::RailExecuteFailed { flags, reason, .. } => {
                warn!(flags, ?reason, "RAIL execute could not be processed");
            }
            RdpOutputEvent::RailApplicationId {
                window_id,
                application_id,
                process_id,
                process_image_name,
            } => {
                debug!(
                    window_id,
                    %application_id,
                    ?process_id,
                    ?process_image_name,
                    "RAIL application identity received"
                );
            }
            RdpOutputEvent::RailControl(control) => {
                debug!(?control, "RAIL control received");
            }
            RdpOutputEvent::WindowingOrders(orders) => {
                if self.remote_app {
                    self.rail_windows.apply_orders(event_loop, &self.context, orders);
                } else {
                    debug!("Ignoring windowing orders outside RemoteApp mode");
                }
            }
            RdpOutputEvent::WindowSurfaceUpdates(updates) => {
                for update in updates {
                    self.rail_windows.apply_surface_update(
                        update.window_id,
                        (update.width, update.height),
                        &update.region,
                        &update.data,
                    );
                }
            }
            // Only produced when the client is built with `.with_desktop_updates()`, which the
            // viewer does not opt into: it always presents full-frame `Image` snapshots instead.
            RdpOutputEvent::DesktopUpdate(_) => {}
        }
    }
}

impl ApplicationHandler<ViewerEvent> for RpcApp {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.on_about_to_wait(event_loop);
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.on_resumed(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: winit::window::WindowId, event: WindowEvent) {
        self.on_window_event(event_loop, window_id, event);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: ViewerEvent) {
        self.handle_viewer_event(event_loop, event);
    }
}

impl ApplicationHandler<RdpOutputEvent> for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.on_about_to_wait(event_loop);
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.on_resumed(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: winit::window::WindowId, event: WindowEvent) {
        self.inner.on_window_event(event_loop, window_id, event);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: RdpOutputEvent) {
        self.inner.handle_output_event(event_loop, event);
    }
}

fn apply_and_send_fast_path_events(
    input_target: &InputTarget,
    input_database: &mut ironrdp::input::Database,
    operations: impl IntoIterator<Item = ironrdp::input::Operation>,
) {
    match input_target {
        InputTarget::Direct(input_event_sender) => {
            let Ok(permit) = input_event_sender.try_reserve() else {
                return;
            };
            let input_events = input_database.apply(operations);
            if !input_events.is_empty() {
                permit.send(RdpInputEvent::FastPath(input_events));
            }
        }
        InputTarget::Rpc(daemon) => {
            let _ = daemon.input_operations(operations);
        }
    }
}

/// Forwards keyboard, modifier, pointer and wheel input from a local window.
///
/// `to_session` maps a position in the window to session desktop coordinates, or rejects it.
fn forward_input_event(
    input_target: &InputTarget,
    input_database: &mut ironrdp::input::Database,
    event: &WindowEvent,
    to_session: impl FnOnce(f64, f64) -> Option<ironrdp::input::MousePosition>,
) {
    match event {
        WindowEvent::KeyboardInput { event, .. } => {
            if let Some(operations) = keyboard_operations(event) {
                apply_and_send_fast_path_events(input_target, input_database, operations);
            }
        }
        WindowEvent::ModifiersChanged(modifiers) => {
            apply_and_send_fast_path_events(input_target, input_database, modifiers_operations(modifiers));
        }
        WindowEvent::CursorMoved { position, .. } => {
            if let Some(position) = to_session(position.x, position.y) {
                apply_and_send_fast_path_events(
                    input_target,
                    input_database,
                    core::iter::once(ironrdp::input::Operation::MouseMove(position)),
                );
            }
        }
        WindowEvent::MouseWheel { delta, .. } => {
            apply_and_send_fast_path_events(input_target, input_database, wheel_operations(*delta));
        }
        WindowEvent::MouseInput { state, button, .. } => {
            if let Some(operation) = mouse_button_operation(*state, *button) {
                apply_and_send_fast_path_events(input_target, input_database, core::iter::once(operation));
            }
        }
        _ => {}
    }
}

/// Releases every key and mouse button the session still holds pressed.
fn release_all_input(input_target: &InputTarget, input_database: &mut ironrdp::input::Database) {
    match input_target {
        InputTarget::Direct(input_event_sender) => {
            let Ok(permit) = input_event_sender.try_reserve() else {
                return;
            };
            let input_events = input_database.release_all();
            if !input_events.is_empty() {
                permit.send(RdpInputEvent::FastPath(input_events));
            }
        }
        // The daemon tracks its own input state, and RemoteApp windows are not shown in RPC mode.
        InputTarget::Rpc(_) => {}
    }
}

/// Maps a key event to fast-path operations; modifiers are reported by `ModifiersChanged` instead.
fn keyboard_operations(event: &event::KeyEvent) -> Option<SmallVec<[ironrdp::input::Operation; 2]>> {
    let key_code = match event.physical_key {
        PhysicalKey::Code(key_code) => key_code,
        PhysicalKey::Unidentified(native_key_code) => {
            warn!(?native_key_code, "Unsupported physical key; ignored");
            return None;
        }
    };

    // `ModifiersChanged` is authoritative for these keys.
    if is_modifier(key_code) {
        return None;
    }

    let Some((scancode, release_only)) = map_key_code(key_code) else {
        warn!(?key_code, "Unsupported physical key; ignored");
        return None;
    };

    Some(match event.state {
        event::ElementState::Pressed => {
            smallvec::smallvec![ironrdp::input::Operation::KeyPressed(scancode)]
        }
        event::ElementState::Released if release_only => smallvec::smallvec![
            ironrdp::input::Operation::KeyPressed(scancode),
            ironrdp::input::Operation::KeyReleased(scancode),
        ],
        event::ElementState::Released => {
            smallvec::smallvec![ironrdp::input::Operation::KeyReleased(scancode)]
        }
    })
}

/// Maps the modifier state to the left modifier keys' operations.
fn modifiers_operations(modifiers: &event::Modifiers) -> SmallVec<[ironrdp::input::Operation; 4]> {
    const SHIFT_LEFT: ironrdp::input::Scancode = ironrdp::input::Scancode::from_u8(false, 0x2A);
    const CONTROL_LEFT: ironrdp::input::Scancode = ironrdp::input::Scancode::from_u8(false, 0x1D);
    const ALT_LEFT: ironrdp::input::Scancode = ironrdp::input::Scancode::from_u8(false, 0x38);
    const LOGO_LEFT: ironrdp::input::Scancode = ironrdp::input::Scancode::from_u8(true, 0x5B);

    let mut operations = SmallVec::<[ironrdp::input::Operation; 4]>::new();

    let mut add_operation = |pressed: bool, scancode: ironrdp::input::Scancode| {
        let operation = if pressed {
            ironrdp::input::Operation::KeyPressed(scancode)
        } else {
            ironrdp::input::Operation::KeyReleased(scancode)
        };
        operations.push(operation);
    };

    // NOTE: https://docs.rs/winit/0.30.12/src/winit/keyboard.rs.html#1737-1744
    //
    // We can’t use state.lshift_state(), state.lcontrol_state(), etc, because on some platforms such as
    // Linux, the modifiers change is hidden.
    //
    // > The exact modifier key is not used to represent modifiers state in the
    // > first place due to a fact that modifiers state could be changed without any
    // > key being pressed and on some platforms like Wayland/X11 which key resulted
    // > in modifiers change is hidden, also, not that it really matters.
    add_operation(modifiers.state().shift_key(), SHIFT_LEFT);
    add_operation(modifiers.state().control_key(), CONTROL_LEFT);
    add_operation(modifiers.state().alt_key(), ALT_LEFT);
    add_operation(modifiers.state().super_key(), LOGO_LEFT);

    operations
}

/// Maps a wheel event to fast-path wheel rotations.
fn wheel_operations(delta: event::MouseScrollDelta) -> SmallVec<[ironrdp::input::Operation; 2]> {
    let mut operations = SmallVec::new();
    let (horizontal, vertical) = match delta {
        // Line deltas are scaled to roughly match a wheel notch.
        event::MouseScrollDelta::LineDelta(delta_x, delta_y) => (f64::from(delta_x) * 100., f64::from(delta_y) * 100.),
        event::MouseScrollDelta::PixelDelta(delta) => (delta.x, delta.y),
    };
    for (is_vertical, delta) in [(false, horizontal), (true, vertical)] {
        if 0.001 < delta.abs() {
            operations.push(ironrdp::input::Operation::WheelRotations(
                ironrdp::input::WheelRotations {
                    is_vertical,
                    #[expect(clippy::as_conversions, reason = "casting f64 to i16")]
                    rotation_units: delta as i16,
                },
            ));
        }
    }
    operations
}

/// Maps a mouse button event to a fast-path operation.
fn mouse_button_operation(state: event::ElementState, button: event::MouseButton) -> Option<ironrdp::input::Operation> {
    let mouse_button = match button {
        event::MouseButton::Left => ironrdp::input::MouseButton::Left,
        event::MouseButton::Right => ironrdp::input::MouseButton::Right,
        event::MouseButton::Middle => ironrdp::input::MouseButton::Middle,
        event::MouseButton::Back => ironrdp::input::MouseButton::X1,
        event::MouseButton::Forward => ironrdp::input::MouseButton::X2,
        event::MouseButton::Other(native_button) => ironrdp::input::MouseButton::from_native_button(native_button)?,
    };

    Some(match state {
        event::ElementState::Pressed => ironrdp::input::Operation::MouseButtonPressed(mouse_button),
        event::ElementState::Released => ironrdp::input::Operation::MouseButtonReleased(mouse_button),
    })
}
