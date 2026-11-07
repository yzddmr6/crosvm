// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright DroidVM contributors
// Additional permissions apply; see ADDITIONAL-PERMISSIONS in the repository root.
// iceland gvm: ported from DroidVM dvm-crosvm 849e05f (gpu_display/src/gpu_display_vnc.rs), CPU transport
// only: no Vulkan blit (vnc_blit.rs), no H.264 consumer, no dmabuf imports, no per-binding devices (input
// goes through the owner's event devices by kind: Tablet for the pointer, Keyboard for keys). See CREDITS.

//! A headless display backend that serves the guest's scanout over RFB (LibVNCServer, through
//! `vnc_server_bridge.c`). The virtio-gpu device or the simplefb bridge writes each frame into the
//! surface's CPU framebuffer (the copy path) and flips; the frame is offered to the bridge, which
//! diffs it in bands and marks only what changed. RFB pointer events become absolute tablet events
//! in guest pixels, RFB keys Linux key events.

use std::collections::VecDeque;
use std::os::raw::c_char;
use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::Mutex;

use base::AsRawDescriptor;
use base::Event;
use base::RawDescriptor;
use base::VolatileSlice;
use linux_input_sys::virtio_input_event;
use vm_control::gpu::DisplayParameters;

use crate::DisplayT;
use crate::EventDeviceKind;
use crate::GpuDisplayError;
use crate::GpuDisplayEvents;
use crate::GpuDisplayFramebuffer;
use crate::GpuDisplayResult;
use crate::GpuDisplaySurface;
use crate::SurfaceType;
use crate::SysDisplayT;

const VNC_INPUT_NONE: c_int = 0;
const VNC_INPUT_KEY: u8 = 1;
const VNC_INPUT_POINTER: u8 = 2;

#[repr(C)]
#[derive(Default, Clone)]
struct VncInputEvent {
    event_type: u8,
    down: u8,
    linux_keycode: u16,
    x: i32,
    y: i32,
    button_mask: u8,
}

extern "C" {
    fn vnc_server_create(
        width: c_int,
        height: c_int,
        host: *const c_char,
        port: c_int,
        password: *const c_char,
    ) -> *mut std::ffi::c_void;
    fn vnc_server_start(server: *mut std::ffi::c_void);
    fn vnc_server_has_input_events(server: *mut std::ffi::c_void) -> c_int;
    fn vnc_server_has_clients(server: *mut std::ffi::c_void) -> c_int;
    fn vnc_server_resize(server: *mut std::ffi::c_void, width: c_int, height: c_int) -> c_int;
    fn vnc_server_destroy(server: *mut std::ffi::c_void);
    fn vnc_server_set_input_event_fd(server: *mut std::ffi::c_void, fd: c_int);
    fn vnc_server_poll_input_event(server: *mut std::ffi::c_void, out: *mut VncInputEvent) -> c_int;
    fn vnc_server_set_cursor(
        server: *mut std::ffi::c_void,
        argb: *const u8,
        width: c_int,
        height: c_int,
        hot_x: c_int,
        hot_y: c_int,
    );
    fn vnc_server_set_cursor_pos(server: *mut std::ffi::c_void, x: c_int, y: c_int);
    #[allow(clippy::too_many_arguments)]
    fn vnc_server_offer_frame(
        server: *mut std::ffi::c_void,
        clean: *const u8,
        clean_size: u32,
        cursor_argb: *const u8,
        cw: c_int,
        ch: c_int,
        cx: c_int,
        cy: c_int,
        visible: c_int,
        full: c_int,
        gpu_blit_ctx: *mut std::ffi::c_void,
        gpu_import_id: i64,
    );
}

struct VncServerHandle {
    ptr: *mut std::ffi::c_void,
}

// SAFETY: the bridge serialises its own state (input ring under a mutex, LibVNCServer's own client
// threads); the handle is only an opaque pointer that outlives every user through the Arc.
unsafe impl Send for VncServerHandle {}
unsafe impl Sync for VncServerHandle {}

impl VncServerHandle {
    fn has_clients(&self) -> bool {
        // SAFETY: ptr is a live server handle owned by this VncServerHandle.
        !self.ptr.is_null() && unsafe { vnc_server_has_clients(self.ptr) != 0 }
    }
}

impl Drop for VncServerHandle {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: the last reference; nothing can offer a frame any more.
            unsafe { vnc_server_destroy(self.ptr) };
        }
    }
}

/// The guest's hardware cursor, as last reported by virtio-gpu.
#[derive(Default)]
struct CursorState {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    visible: bool,
}

struct SharedFramebuffer {
    /// The guest scanout with NO cursor composited into it (the bridge restores from it when the
    /// pointer moves), BGRX = DRM XRGB8888 little endian, packed to the width.
    data: Vec<u8>,
    server: Arc<VncServerHandle>,
    cursor: CursorState,
    frames: u64,
}

impl SharedFramebuffer {
    fn offer_frame(&mut self, full: bool) {
        let c = &self.cursor;
        let has_img = !c.pixels.is_empty() && c.width > 0 && c.height > 0;
        // SAFETY: both buffers outlive the call; the bridge only reads them.
        unsafe {
            vnc_server_offer_frame(
                self.server.ptr,
                self.data.as_ptr(),
                self.data.len() as u32,
                if has_img { c.pixels.as_ptr() } else { std::ptr::null() },
                c.width as c_int,
                c.height as c_int,
                c.x as c_int,
                c.y as c_int,
                (c.visible && has_img) as c_int,
                full as c_int,
                std::ptr::null_mut(),
                0,
            )
        }
        if full {
            self.frames += 1;
            // iceland gvm: frame statistics for the tests (V9-2): first frame, then every 300th
            if self.frames == 1 || self.frames % 300 == 0 {
                let (mean, nonzero) = frame_stats(&self.data);
                eprintln!(
                    "GVM-VNC-FRAME {} clients={} mean={:.3} nonzero={:.3}",
                    self.frames,
                    self.server.has_clients() as u8,
                    mean,
                    nonzero
                );
            }
        }
    }
}

/// Mean brightness and the share of non-black pixels on a 64x64 grid of a packed BGRX frame.
fn frame_stats(data: &[u8]) -> (f64, f64) {
    let px = data.len() / 4;
    if px == 0 {
        return (0.0, 0.0);
    }
    let step = (px / 4096).max(1);
    let (mut sum, mut nz, mut n) = (0u64, 0u64, 0u64);
    let mut i = 0;
    while i < px {
        let p = &data[i * 4..i * 4 + 3];
        let s = p[0] as u64 + p[1] as u64 + p[2] as u64;
        sum += s;
        if s > 24 {
            nz += 1;
        }
        n += 1;
        i += step;
    }
    (sum as f64 / (n as f64 * 765.0), nz as f64 / n as f64)
}

struct VncSurface {
    width: u32,
    height: u32,
    shared_fb: Arc<Mutex<SharedFramebuffer>>,
    local_buffer: Vec<u8>,
}

impl GpuDisplaySurface for VncSurface {
    fn framebuffer(&mut self) -> Option<GpuDisplayFramebuffer> {
        let stride = self.width * 4;
        Some(GpuDisplayFramebuffer::new(
            VolatileSlice::new(self.local_buffer.as_mut_slice()),
            stride,
            4,
        ))
    }

    fn flip(&mut self) {
        // No "skip when nobody is connected": the flip is driven by the guest's flush, a frame
        // dropped here is never offered again (DroidVM measured a black client after a resize).
        if let Ok(mut fb) = self.shared_fb.lock() {
            let n = fb.data.len().min(self.local_buffer.len());
            fb.data[..n].copy_from_slice(&self.local_buffer[..n]);
            fb.offer_frame(true);
        }
    }
}

/// virtio-gpu's cursor plane: composited into the outgoing frame by the bridge, and published as
/// an RFB cursor for clients with the Cursor pseudo-encoding. The upstream gpu device gives no
/// hotspot, so the image is drawn with its top-left corner at the reported position (as the
/// Wayland backend does) and the RFB cursor gets hotspot 0,0.
struct VncCursorSurface {
    width: u32,
    height: u32,
    server: Arc<VncServerHandle>,
    shared_fb: Arc<Mutex<SharedFramebuffer>>,
    pixels: Vec<u8>,
}

impl GpuDisplaySurface for VncCursorSurface {
    fn framebuffer(&mut self) -> Option<GpuDisplayFramebuffer> {
        Some(GpuDisplayFramebuffer::new(
            VolatileSlice::new(self.pixels.as_mut_slice()),
            self.width * 4,
            4,
        ))
    }

    fn flip(&mut self) {
        if let Ok(mut fb) = self.shared_fb.lock() {
            fb.cursor.pixels.clear();
            fb.cursor.pixels.extend_from_slice(&self.pixels);
            fb.cursor.width = self.width;
            fb.cursor.height = self.height;
            fb.cursor.visible = true;
            fb.offer_frame(false);
        }
        // SAFETY: `pixels` is width*height*4 bytes and outlives the call.
        unsafe {
            vnc_server_set_cursor(
                self.server.ptr,
                self.pixels.as_ptr(),
                self.width as c_int,
                self.height as c_int,
                0,
                0,
            )
        }
    }

    fn set_position(&mut self, x: u32, y: u32) {
        if let Ok(mut fb) = self.shared_fb.lock() {
            fb.cursor.x = x as i32;
            fb.cursor.y = y as i32;
            fb.offer_frame(false);
        }
        // SAFETY: server handle is valid for this surface's lifetime.
        unsafe { vnc_server_set_cursor_pos(self.server.ptr, x as c_int, y as c_int) }
    }
}

pub struct DisplayVnc {
    // Field order is drop order: the server (its client threads write to `event`) goes first.
    shared_fb: Option<Arc<Mutex<SharedFramebuffer>>>,
    server: Arc<VncServerHandle>,
    width: u32,
    height: u32,
    input_queue: VecDeque<VncInputEvent>,
    prev_button_mask: u8,
    event: Event,
}

impl DisplayVnc {
    /// `host` is an IPv4 / IPv6 literal or "" / "0.0.0.0" / "::" for every address (the bridge
    /// refuses names). `width` x `height` is the first screen size; a surface of another size
    /// resizes the RFB framebuffer.
    pub fn new_tcp(
        host: &str,
        port: u16,
        width: u32,
        height: u32,
        password: Option<&str>,
    ) -> GpuDisplayResult<DisplayVnc> {
        let event = Event::new().map_err(|_| GpuDisplayError::CreateEvent)?;
        let c_host = std::ffi::CString::new(host).map_err(|_| GpuDisplayError::Allocate)?;
        let c_password = match password {
            Some(p) if !p.is_empty() => {
                Some(std::ffi::CString::new(p).map_err(|_| GpuDisplayError::Allocate)?)
            }
            _ => None,
        };
        // SAFETY: the strings outlive the call; the bridge copies what it keeps.
        let server_ptr = unsafe {
            vnc_server_create(
                width as c_int,
                height as c_int,
                c_host.as_ptr(),
                port as c_int,
                c_password.as_ref().map(|p| p.as_ptr()).unwrap_or(std::ptr::null()),
            )
        };
        if server_ptr.is_null() {
            base::error!("GVM-VNC server failed to start on {}:{}", host, port);
            return Err(GpuDisplayError::Allocate);
        }
        // SAFETY: the server is live; the event outlives it (DisplayVnc drops `server` before
        // `event`, and surfaces holding the server are dropped before the display).
        unsafe {
            vnc_server_set_input_event_fd(server_ptr, event.as_raw_descriptor());
            vnc_server_start(server_ptr);
        }
        eprintln!(
            "GVM-VNC listening on {}:{} ({}x{}, {})",
            if host.is_empty() { "*" } else { host },
            port,
            width,
            height,
            if c_password.is_some() { "VNC password" } else { "no password" }
        );
        Ok(DisplayVnc {
            event,
            width,
            height,
            server: Arc::new(VncServerHandle { ptr: server_ptr }),
            shared_fb: None,
            input_queue: VecDeque::new(),
            prev_button_mask: 0,
        })
    }

    fn drain_c_events(&mut self) {
        loop {
            let mut ev = VncInputEvent::default();
            // SAFETY: the server is live; `ev` is a valid out pointer.
            let t = unsafe { vnc_server_poll_input_event(self.server.ptr, &mut ev) };
            if t == VNC_INPUT_NONE {
                break;
            }
            self.input_queue.push_back(ev);
        }
        let _ = self.event.wait_timeout(std::time::Duration::ZERO);
    }

    /// Absolute pointer (like QEMU's usb-tablet): position on every event, button transitions
    /// from the RFB mask (bit0 left, bit1 middle, bit2 right, bit3 / bit4 wheel up / down,
    /// bit5 / bit6 wheel left / right). Coordinates are guest pixels: the gvm tablet device is
    /// sized to the display.
    fn pointer_events(&mut self, ev: &VncInputEvent) -> Vec<virtio_input_event> {
        const REL_HWHEEL: u16 = 0x06;
        let cur = ev.button_mask;
        let changed = cur ^ self.prev_button_mask;
        self.prev_button_mask = cur;
        let mut events = vec![
            virtio_input_event::absolute_x(ev.x.clamp(0, self.width.saturating_sub(1) as i32)),
            virtio_input_event::absolute_y(ev.y.clamp(0, self.height.saturating_sub(1) as i32)),
        ];
        if changed & 0x01 != 0 {
            events.push(virtio_input_event::left_click(cur & 0x01 != 0));
        }
        if changed & 0x02 != 0 {
            events.push(virtio_input_event::middle_click(cur & 0x02 != 0));
        }
        if changed & 0x04 != 0 {
            events.push(virtio_input_event::right_click(cur & 0x04 != 0));
        }
        if changed & 0x08 != 0 && cur & 0x08 != 0 {
            events.push(virtio_input_event::wheel(1));
        }
        if changed & 0x10 != 0 && cur & 0x10 != 0 {
            events.push(virtio_input_event::wheel(-1));
        }
        if changed & 0x20 != 0 && cur & 0x20 != 0 {
            events.push(virtio_input_event::relative(REL_HWHEEL, -1));
        }
        if changed & 0x40 != 0 && cur & 0x40 != 0 {
            events.push(virtio_input_event::relative(REL_HWHEEL, 1));
        }
        events
    }

    fn next_report(&mut self) -> Option<GpuDisplayEvents> {
        let ev = self.input_queue.pop_front()?;
        match ev.event_type {
            VNC_INPUT_KEY => Some(GpuDisplayEvents {
                events: vec![virtio_input_event::key(ev.linux_keycode, ev.down != 0, false)],
                device_type: EventDeviceKind::Keyboard,
            }),
            VNC_INPUT_POINTER => Some(GpuDisplayEvents {
                events: self.pointer_events(&ev),
                device_type: EventDeviceKind::Tablet,
            }),
            _ => None,
        }
    }
}

impl DisplayT for DisplayVnc {
    fn pending_events(&self) -> bool {
        // SAFETY: the server is live.
        !self.input_queue.is_empty() || unsafe { vnc_server_has_input_events(self.server.ptr) != 0 }
    }

    fn next_event(&mut self) -> GpuDisplayResult<u64> {
        self.drain_c_events();
        Ok(0)
    }

    fn handle_next_event(
        &mut self,
        _surface: &mut Box<dyn GpuDisplaySurface>,
    ) -> Option<GpuDisplayEvents> {
        self.next_report()
    }

    fn create_surface(
        &mut self,
        parent_surface_id: Option<u32>,
        _surface_id: u32,
        _scanout_id: Option<u32>,
        display_params: &DisplayParameters,
        surf_type: SurfaceType,
    ) -> GpuDisplayResult<Box<dyn GpuDisplaySurface>> {
        if parent_surface_id.is_some() {
            if !matches!(surf_type, SurfaceType::Cursor) {
                return Err(GpuDisplayError::Unsupported);
            }
            let (width, height) = (64u32, 64u32); // virtio-gpu's cursor plane
            let shared_fb = self.shared_fb.clone().ok_or(GpuDisplayError::Unsupported)?;
            return Ok(Box::new(VncCursorSurface {
                width,
                height,
                server: self.server.clone(),
                shared_fb,
                pixels: vec![0u8; (width as usize) * (height as usize) * 4],
            }));
        }
        let (req_w, req_h) = display_params.get_virtual_display_size();
        let width = if req_w != 0 { req_w } else { self.width };
        let height = if req_h != 0 { req_h } else { self.height };
        if width != self.width || height != self.height {
            // SAFETY: the server is live.
            if unsafe { vnc_server_resize(self.server.ptr, width as c_int, height as c_int) } != 0 {
                base::error!("GVM-VNC resize to {}x{} failed", width, height);
                return Err(GpuDisplayError::Allocate);
            }
            eprintln!("GVM-VNC resized to {}x{}", width, height);
            self.width = width;
            self.height = height;
        }
        let buf = (width as usize) * (height as usize) * 4;
        let shared_fb = Arc::new(Mutex::new(SharedFramebuffer {
            data: vec![0u8; buf],
            server: self.server.clone(),
            cursor: CursorState::default(),
            frames: 0,
        }));
        self.shared_fb = Some(shared_fb.clone());
        Ok(Box::new(VncSurface {
            width,
            height,
            shared_fb,
            local_buffer: vec![0u8; buf],
        }))
    }
}

impl SysDisplayT for DisplayVnc {}

impl AsRawDescriptor for DisplayVnc {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.event.as_raw_descriptor()
    }
}

#[cfg(test)]
mod tests {
    use super::frame_stats;

    #[test]
    fn stats_black_and_white() {
        assert_eq!(frame_stats(&[0u8; 4 * 100]), (0.0, 0.0));
        let (m, nz) = frame_stats(&[255u8; 4 * 100]);
        assert!((m - 1.0).abs() < 1e-9 && (nz - 1.0).abs() < 1e-9);
    }
}
