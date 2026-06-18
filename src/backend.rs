//! DRM/KMS backend: ZenOS owns the screen directly, no weston/X11.
//!
//! Multi-output: one DRM device (`Gpu`) drives one `Surface` per connected
//! connector (each its own CRTC + DrmCompositor + Output, positioned
//! left-to-right in the global Space). Connectors are (re)scanned at boot and on
//! udev "change" events, so monitors can be hot-plugged/unplugged.
//!
//! Rendering is event-driven (see `run`): the loop blocks on events and the
//! per-output flip chain is kept alive by VBlanks, so the frame rate tracks each
//! monitor's refresh with no idle busy-spin.

use std::time::Duration;

use smithay::backend::drm::{DrmEvent, DrmNode};
use smithay::backend::input::{
    Axis, AxisSource, ButtonState, Event, InputEvent, KeyState, KeyboardKeyEvent,
    PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::input::keyboard::{keysyms, FilterResult, Keysym};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::input::Libinput;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode as CalloopMode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
use smithay::utils::{Point, Rectangle, Size, SERIAL_COUNTER};
use smithay::wayland::socket::ListeningSocketSource;

use std::sync::Arc;

use crate::state::{ClientState, MoveGrab, ResizeGrab};

use crate::state::ZenState;

use crate::config::*;

use smithay::desktop::WindowSurfaceType;

use crate::drm::{open_device, scan_connectors};
use crate::layout::{dock_icon_pos, power_btn_rect, power_menu_item_rect};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // --- Step 1: session -----------------------------------------------------
    let (session, session_notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    tracing::info!("seat: {seat_name}");

    let mut event_loop: EventLoop<ZenState> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    // Wayland display (kept local; dispatched each loop tick so existing calloop
    // sources keep their &mut ZenState signature, no CalloopData wrapper needed).
    let mut display: Display<ZenState> = Display::new()?;
    let dh = display.handle();

    let mut state = ZenState::new(dh, event_loop.get_signal(), session, seat_name.clone());

    // Wayland client socket. Clients connecting here get a ClientState.
    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    handle.insert_source(socket, move |stream, _, data: &mut ZenState| {
        if let Err(e) = data
            .display_handle
            .insert_client(stream, Arc::new(ClientState::default()))
        {
            tracing::warn!("failed to accept client: {e}");
        }
    })?;
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);
    tracing::info!("WAYLAND_DISPLAY={socket_name}");

    // Wake the event loop when a client sends a request, so the loop can block
    // on events (no busy-poll) yet still service clients promptly. The actual
    // dispatch_clients + flush happen in the main loop; this source only needs
    // to make `dispatch` return. Level-triggered: readiness is cleared when the
    // main loop reads the fd via dispatch_clients on the next iteration.
    let display_fd = display.backend().poll_fd().try_clone_to_owned()?;
    handle.insert_source(
        Generic::new(display_fd, Interest::READ, CalloopMode::Level),
        |_, _, _: &mut ZenState| Ok(PostAction::Continue),
    )?;

    // --- Step 2: udev (GPU discovery) ---------------------------------------
    let udev_backend = UdevBackend::new(&seat_name)?;

    // $ZENOS_GPU forces a specific DRM node (e.g. /dev/dri/card0). On hybrid
    // laptops the external HDMI/DP is often wired to the dGPU, while the iGPU
    // only sees it in degraded modes; pick the GPU the display hangs off.
    let primary = std::env::var("ZENOS_GPU")
        .ok()
        .and_then(|p| DrmNode::from_path(p).ok())
        .or_else(|| {
            primary_gpu(&seat_name)
                .ok()
                .flatten()
                .and_then(|p| DrmNode::from_path(p).ok())
        })
        .or_else(|| {
            all_gpus(&seat_name)
                .ok()?
                .into_iter()
                .find_map(|p| DrmNode::from_path(p).ok())
        })
        .ok_or("no GPU found via udev")?;
    tracing::info!("primary GPU: {:?}", primary);

    let dev_path = primary
        .dev_path()
        .ok_or("primary GPU has no device path")?;

    // --- Event sources -------------------------------------------------------
    handle.insert_source(session_notifier, move |event, _, data| match event {
        SessionEvent::PauseSession => {
            tracing::info!("session paused");
            if let Some(gpu) = &mut data.gpu {
                gpu.drm.pause();
            }
        }
        SessionEvent::ActivateSession => {
            tracing::info!("session resumed");
            // TODO(milestone-7): drm.activate() + reset CRTC + redraw.
        }
    })?;

    // Pump until the session is active before opening the GPU (SET_MASTER).
    let mut tries = 0;
    while !state.session.is_active() && tries < 200 {
        event_loop.dispatch(Some(Duration::from_millis(10)), &mut state)?;
        tries += 1;
    }
    if !state.session.is_active() {
        return Err("session never became active (no DRM master)".into());
    }
    tracing::info!("session active after {tries} dispatch(es)");

    // Open the DRM device, then scan connectors -> one Surface per output.
    let (mut gpu, drm_notifier) = open_device(&mut state.session, primary, &dev_path)?;
    scan_connectors(&mut gpu, &mut state.space, &state.display_handle)?;
    if gpu.surfaces.is_empty() {
        return Err("no connected output".into());
    }
    state.gpu = Some(gpu);

    // DRM VBlank: per-output heartbeat. Ack the finished flip on that CRTC,
    // release clients to draw their next frame (throttled to the monitor
    // refresh), then render again so any damage shows and the chain keeps going.
    handle.insert_source(drm_notifier, move |event, _, data| match event {
        DrmEvent::VBlank(crtc) => {
            if let Some(gpu) = &mut data.gpu {
                if let Some(surface) = gpu.surfaces.get_mut(&crtc) {
                    let _ = surface.compositor.frame_submitted();
                    surface.pending_flip = false;
                }
            }
            data.send_frame_callbacks();
            data.render();
        }
        DrmEvent::Error(e) => tracing::error!("DRM error: {e:?}"),
    })?;

    // udev "change" = a monitor was (un)plugged: rescan connectors and add/remove
    // outputs, then redraw.
    handle.insert_source(udev_backend, move |event, _, data| match event {
        UdevEvent::Changed { device_id } => {
            tracing::info!("udev change {device_id}, rescanning outputs");
            let dh = data.display_handle.clone();
            if let Some(mut gpu) = data.gpu.take() {
                if let Err(e) = scan_connectors(&mut gpu, &mut data.space, &dh) {
                    tracing::error!("rescan failed: {e}");
                }
                data.gpu = Some(gpu);
            }
            data.dirty = true;
            data.scene_dirty = true;
            data.render();
        }
        UdevEvent::Added { device_id, .. } => tracing::debug!("udev add {device_id}"),
        UdevEvent::Removed { device_id } => tracing::debug!("udev remove {device_id}"),
    })?;

    // Input: libinput on the session seat. Esc quits cleanly.
    let mut libinput =
        Libinput::new_with_udev(LibinputSessionInterface::from(state.session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|_| "libinput udev_assign_seat failed")?;
    let libinput_backend = LibinputInputBackend::new(libinput);
    handle.insert_source(libinput_backend, move |event, _, data| {
        // Any input event means something will visibly change next render.
        data.dirty = true;
        match event {
        InputEvent::Keyboard { event } => {
            let keyboard = data.seat.get_keyboard().unwrap();
            let serial = SERIAL_COUNTER.next_serial();
            let time = event.time_msec();
            let code = event.key_code();
            let key_state = event.state();
            // Forward to the focused client, unless it's a compositor shortcut.
            keyboard.input::<(), _>(data, code, key_state, serial, time, |data, _mods, sym| {
                if key_state == KeyState::Pressed {
                    let keysym = sym.modified_sym();
                    if keysym == Keysym::new(keysyms::KEY_Escape) || code == RAW_KEY_ESC.into() {
                        tracing::info!("Esc pressed, exiting");
                        data.running = false;
                        return FilterResult::Intercept(());
                    } else if keysym == Keysym::new(keysyms::KEY_F1) || code == RAW_KEY_F1.into() {
                        tracing::info!("F1 pressed, launching foot");
                        let _ = std::process::Command::new("foot").spawn();
                        return FilterResult::Intercept(());
                    } else if keysym == Keysym::new(keysyms::KEY_F2) || code == RAW_KEY_F2.into() {
                        tracing::info!("F2 pressed, screenshot (code={code:?}, sym={keysym:?})");
                        data.screenshot = true;
                        return FilterResult::Intercept(());
                    }
                }
                FilterResult::Forward
            });
        }
        InputEvent::PointerMotion { event } => {
            // Clamp to the union of all outputs (multi-monitor desktop spans
            // left-to-right).
            let (mut maxw, mut maxh) = (0i32, 0i32);
            if let Some(gpu) = &data.gpu {
                for s in gpu.surfaces.values() {
                    maxw = maxw.max(s.location.0 + s.size.0);
                    maxh = maxh.max(s.location.1 + s.size.1);
                }
            }
            let mut loc = data.pointer_location;
            loc.x = (loc.x + event.delta_x()).clamp(0.0, maxw as f64);
            loc.y = (loc.y + event.delta_y()).clamp(0.0, maxh as f64);
            data.pointer_location = loc;

            if let Some(grab) = &data.resize_grab {
                let dx = (loc.x - grab.start_ptr.x) as i32;
                let dy = (loc.y - grab.start_ptr.y) as i32;
                let (sw, sh) = grab.start_size;
                let (sx, sy) = (grab.start_loc.x, grab.start_loc.y);
                let (gl, gr, gt, gb) = (grab.left, grab.right, grab.top, grab.bottom);
                let last = grab.last_size;
                let window = grab.window.clone();
                let mut nw = sw;
                let mut nh = sh;
                if gr {
                    nw = sw + dx;
                }
                if gl {
                    nw = sw - dx;
                }
                if gb {
                    nh = sh + dy;
                }
                if gt {
                    nh = sh - dy;
                }
                nw = nw.max(WIN_MIN_W);
                nh = nh.max(WIN_MIN_H);
                // Skip when the target size is unchanged (most motions, since the
                // client snaps to a grid): avoids a configure storm and jitter.
                if (nw, nh) != last {
                    // When dragging the left/top edge the origin moves with it, but
                    // stops once the window hits its minimum size.
                    let nx = if gl { sx + (sw - nw) } else { sx };
                    let ny = if gt { sy + (sh - nh) } else { sy };
                    if let Some(t) = window.toplevel() {
                        t.with_pending_state(|s| s.size = Some((nw, nh).into()));
                        t.send_configure();
                    }
                    data.space.map_element(window, (nx, ny), false);
                    data.dirty = true;
                    data.scene_dirty = true;
                    if let Some(g) = &mut data.resize_grab {
                        g.last_size = (nw, nh);
                    }
                }
            } else if data.move_grab.is_some() {
                // Don't reposition here: libinput motion fires ~1000Hz, far above
                // the refresh. Just flag the scene dirty; render() applies the
                // move once per frame from the latest pointer position (coalesced,
                // lowest latency). See ZenState::apply_move_grab.
                data.scene_dirty = true;
            } else {
                // Resolve the surface under the pointer, popups + subsurfaces
                // included (WindowSurfaceType::ALL). Popup focus is what lets an
                // active PopupPointerGrab route motion into menu items; it only
                // gates by client, so we must hand it the real popup surface.
                let focus = {
                    let mut found = None;
                    for window in data.space.elements().rev() {
                        let win_loc = data.space.element_location(window).unwrap_or_default();
                        if let Some((surf, sloc)) =
                            window.surface_under(loc - win_loc.to_f64(), WindowSurfaceType::ALL)
                        {
                            found = Some((surf, (sloc + win_loc).to_f64()));
                            break;
                        }
                    }
                    found
                };
                let pointer = data.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time_msec();
                pointer.motion(
                    data,
                    focus,
                    &MotionEvent {
                        location: loc,
                        serial,
                        time,
                    },
                );
                pointer.frame(data);
            }
        }
        InputEvent::PointerButton { event } => {
            let serial = SERIAL_COUNTER.next_serial();
            let time = event.time_msec();
            let button = event.button_code();
            let button_state = event.state();
            if button_state == ButtonState::Pressed {
                let loc = data.pointer_location;
                // A press can focus/raise/move/resize/maximize a window — all of
                // which change the composed scene, so recompose this frame.
                data.scene_dirty = true;

                // Power menu (top-left bar button + dropdown). Hit-tested first,
                // in output-local coords, and consumes the click.
                if button == BTN_LEFT {
                    let (ox, oy) = data
                        .gpu
                        .as_ref()
                        .and_then(|g| {
                            g.surfaces.values().find_map(|s| {
                                let (sx, sy) = s.location;
                                let (sw, sh) = s.size;
                                let (lx, ly) = (loc.x as i32, loc.y as i32);
                                (lx >= sx && lx < sx + sw && ly >= sy && ly < sy + sh)
                                    .then_some(s.location)
                            })
                        })
                        .unwrap_or((0, 0));
                    let lx = loc.x as i32 - ox;
                    let ly = loc.y as i32 - oy;
                    let (bx, by, bw, bh) = power_btn_rect();
                    if lx >= bx && lx < bx + bw && ly >= by && ly < by + bh {
                        data.power_menu_open = !data.power_menu_open;
                        data.dirty = true;
                        return;
                    }
                    if data.power_menu_open {
                        for i in 0..POWER_ITEMS.len() as i32 {
                            let (ix, iy, iw, ih) = power_menu_item_rect(i);
                            if lx >= ix && lx < ix + iw && ly >= iy && ly < iy + ih {
                                let cmd = if i == 0 { "reboot" } else { "poweroff" };
                                tracing::info!("power menu: systemctl {cmd}");
                                let _ = std::process::Command::new("systemctl").arg(cmd).spawn();
                                break;
                            }
                        }
                        // Any click while open dismisses it (and is consumed).
                        data.power_menu_open = false;
                        data.dirty = true;
                        return;
                    }
                }

                // Dock launch hit-test (icons live in each output's dock).
                if button == BTN_LEFT {
                    let mut launch = None;
                    if let Some(gpu) = &data.gpu {
                        'outer: for s in gpu.surfaces.values() {
                            let (sw, sh) = s.size;
                            let (ox, oy) = s.location;
                            for (i, app) in DOCK_APPS.iter().enumerate() {
                                let (ix, iy) = dock_icon_pos(sw, sh, i, DOCK_APPS.len());
                                let r = Rectangle::new(
                                    Point::from((ox + ix, oy + iy)),
                                    Size::from((ICON_SIZE, ICON_SIZE)),
                                );
                                if r.to_f64().contains(loc) {
                                    launch = Some(app.exec);
                                    break 'outer;
                                }
                            }
                        }
                    }
                    if let Some(exec) = launch {
                        tracing::info!("dock launch: {exec}");
                        let _ = std::process::Command::new(exec).spawn();
                        let pointer = data.seat.get_pointer().unwrap();
                        pointer.button(
                            data,
                            &ButtonEvent {
                                button,
                                state: button_state,
                                serial,
                                time,
                            },
                        );
                        pointer.frame(data);
                        return;
                    }
                }

                // SSD titlebars live above the surface, outside the Space, so
                // hit-test them manually before the normal surface focus path.
                let deco = {
                    let mut found = None;
                    // Topmost first, so overlapping titlebars hit-test the front one.
                    for window in data.space.elements().rev() {
                        let wl = data.space.element_location(window).unwrap_or_default();
                        let gw = window.geometry().size.w;
                        if gw <= 0 {
                            continue;
                        }
                        let tb = Rectangle::new(
                            Point::from((wl.x, wl.y - TITLEBAR_H)),
                            Size::from((gw, TITLEBAR_H)),
                        );
                        if tb.to_f64().contains(loc) {
                            found = Some((window.clone(), wl));
                            break;
                        }
                    }
                    found
                };
                if let Some((window, wl)) = deco {
                    if let Some(s) = window.toplevel().map(|t| t.wl_surface().clone()) {
                        let keyboard = data.seat.get_keyboard().unwrap();
                        keyboard.set_focus(data, Some(s), serial);
                    }
                    // Bring the clicked window to the front of the stack.
                    data.space.raise_element(&window, true);
                    // First traffic light = close.
                    let close = Rectangle::new(
                        Point::from((wl.x + LIGHT_MARGIN, wl.y - TITLEBAR_H + (TITLEBAR_H - LIGHT_DIA) / 2)),
                        Size::from((LIGHT_DIA, LIGHT_DIA)),
                    );
                    // Third traffic light = maximize / restore.
                    let max_btn = Rectangle::new(
                        Point::from((
                            wl.x + LIGHT_MARGIN + 2 * LIGHT_SPACING,
                            wl.y - TITLEBAR_H + (TITLEBAR_H - LIGHT_DIA) / 2,
                        )),
                        Size::from((LIGHT_DIA, LIGHT_DIA)),
                    );
                    if button == BTN_LEFT && close.to_f64().contains(loc) {
                        if let Some(t) = window.toplevel() {
                            t.send_close();
                        }
                    } else if button == BTN_LEFT && max_btn.to_f64().contains(loc) {
                        // Toggle maximize: restore saved geometry, or fill the
                        // output below the bar + titlebar and remember the old one.
                        if let Some(t) = window.toplevel() {
                            let surf = t.wl_surface().clone();
                            // Owned copy so the outputs() iterator borrow ends here,
                            // before the &mut data.space below.
                            let out_geo = data
                                .space
                                .outputs()
                                .next()
                                .and_then(|o| data.space.output_geometry(o));
                            if let Some(((rx, ry), (rw, rh))) = data.maximized.remove(&surf) {
                                t.with_pending_state(|s| {
                                    s.size = Some((rw, rh).into());
                                    // Clear Maximized so the client restores its
                                    // own decorations / margins.
                                    s.states.unset(XdgState::Maximized);
                                });
                                t.send_configure();
                                data.space.map_element(window.clone(), (rx, ry), false);
                            } else if let Some(geo) = out_geo {
                                let cur = window.geometry().size;
                                data.maximized.insert(surf, ((wl.x, wl.y), (cur.w, cur.h)));
                                let mw = geo.size.w;
                                // Reserve the top bar + SSD titlebar above and the
                                // floating dock (height + its bottom margin) below.
                                let mh =
                                    geo.size.h - BAR_H - TITLEBAR_H - DOCK_H - DOCK_MARGIN;
                                t.with_pending_state(|s| {
                                    s.size = Some((mw, mh).into());
                                    // Maximized tells the client to drop its CSD
                                    // shadows/insets and fill the size exactly.
                                    s.states.set(XdgState::Maximized);
                                });
                                t.send_configure();
                                data.space.map_element(
                                    window.clone(),
                                    (geo.loc.x, geo.loc.y + BAR_H + TITLEBAR_H),
                                    false,
                                );
                            }
                        }
                        data.dirty = true;
                    } else if button == BTN_LEFT {
                        // Drag the titlebar to move (no modifier, macOS-style).
                        data.move_grab = Some(MoveGrab {
                            window,
                            start_ptr: loc,
                            start_win: wl,
                        });
                    }
                    let pointer = data.seat.get_pointer().unwrap();
                    pointer.button(
                        data,
                        &ButtonEvent {
                            button,
                            state: button_state,
                            serial,
                            time,
                        },
                    );
                    pointer.frame(data);
                    return;
                }

                // Click on window content = focus only. Moving is done by
                // dragging the titlebar (handled above), no modifier needed.
                let under = data.space.element_under(loc).map(|(w, _)| w.clone());
                if let Some(window) = under {
                    if let Some(s) = window.toplevel().map(|t| t.wl_surface().clone()) {
                        let keyboard = data.seat.get_keyboard().unwrap();
                        keyboard.set_focus(data, Some(s), serial);
                    }
                    // Bring the clicked window to the front of the stack.
                    data.space.raise_element(&window, true);
                    // Click within the edge band starts an interactive resize.
                    if button == BTN_LEFT {
                        let wl_loc = data.space.element_location(&window).unwrap_or_default();
                        let gs = window.geometry().size;
                        let rx = loc.x as i32 - wl_loc.x;
                        let ry = loc.y as i32 - wl_loc.y;
                        let left = rx <= RESIZE_BORDER;
                        let right = rx >= gs.w - RESIZE_BORDER;
                        let top = ry <= RESIZE_BORDER;
                        let bottom = ry >= gs.h - RESIZE_BORDER;
                        if left || right || top || bottom {
                            data.resize_grab = Some(ResizeGrab {
                                window: window.clone(),
                                start_ptr: loc,
                                start_loc: wl_loc,
                                start_size: (gs.w, gs.h),
                                last_size: (gs.w, gs.h),
                                left,
                                right,
                                top,
                                bottom,
                            });
                        }
                    }
                }
            } else {
                data.move_grab = None;
                data.resize_grab = None;
            }
            let pointer = data.seat.get_pointer().unwrap();
            pointer.button(
                data,
                &ButtonEvent {
                    button,
                    state: button_state,
                    serial,
                    time,
                },
            );
            pointer.frame(data);
        }
        InputEvent::PointerAxis { event } => {
            // Forward wheel + touchpad scroll to the focused client. Continuous
            // (value) + discrete (v120) amounts, per smithay's pointer-axis API;
            // a finger source with a 0 amount means scroll stopped.
            let source = event.source();
            let amount = |axis| {
                event
                    .amount(axis)
                    .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 3.0 / 120.0)
            };
            let (hv, vv) = (amount(Axis::Horizontal), amount(Axis::Vertical));
            let mut frame = AxisFrame::new(event.time_msec()).source(source);
            if hv != 0.0 {
                frame = frame.value(Axis::Horizontal, hv);
                if let Some(d) = event.amount_v120(Axis::Horizontal) {
                    frame = frame.v120(Axis::Horizontal, d as i32);
                }
            }
            if vv != 0.0 {
                frame = frame.value(Axis::Vertical, vv);
                if let Some(d) = event.amount_v120(Axis::Vertical) {
                    frame = frame.v120(Axis::Vertical, d as i32);
                }
            }
            if source == AxisSource::Finger {
                if event.amount(Axis::Horizontal) == Some(0.0) {
                    frame = frame.stop(Axis::Horizontal);
                }
                if event.amount(Axis::Vertical) == Some(0.0) {
                    frame = frame.stop(Axis::Vertical);
                }
            }
            let pointer = data.seat.get_pointer().unwrap();
            pointer.axis(data, frame);
            pointer.frame(data);
        }
        _ => {}
        }
    })?;

    // 1Hz tick so the clock redraws even when otherwise idle (minute rollover).
    handle.insert_source(
        Timer::from_duration(Duration::from_secs(1)),
        |_, _, data: &mut ZenState| {
            data.dirty = true; // clock may have ticked over
            data.scene_dirty = true;
            data.render();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        },
    )?;

    // --- Safety auto-exit ----------------------------------------------------
    let timeout = std::env::var("ZENOS_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10);
    let deadline =
        (timeout > 0).then(|| std::time::Instant::now() + Duration::from_secs(timeout));
    if deadline.is_some() {
        tracing::info!("auto-exit in {timeout}s (set ZENOS_TIMEOUT to change, 0 to disable)");
    }

    // Kick the first frame on every output to start their flip chains.
    state.render();

    // --- Dispatch loop -------------------------------------------------------
    // Event-driven: block until something happens (input, client request, or a
    // VBlank), then service it and try to render. Rendering itself is gated by
    // pending_flip + damage, and the VBlank handler keeps the flip chain going,
    // so the effective frame rate tracks the monitor's refresh (60/120/165Hz)
    // with no busy-spin when idle. Only poll on a timer when an auto-exit
    // deadline is set, so it can still fire with no events.
    let tick = deadline.map(|_| Duration::from_millis(200));
    tracing::info!("ZenOS compositor running");
    while state.running {
        event_loop.dispatch(tick, &mut state)?;

        display.dispatch_clients(&mut state)?;
        state.space.refresh();
        state.render();
        display.flush_clients()?;

        if let Some(d) = deadline {
            if std::time::Instant::now() >= d {
                tracing::info!("auto-exit timeout reached, shutting down");
                state.running = false;
            }
        }
    }

    tracing::info!("releasing GPU + session");
    state.gpu = None; // drop DrmDevice -> release DRM master -> restore TTY
    Ok(())
}
