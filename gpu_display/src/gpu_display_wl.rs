// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Crate for displaying simple surfaces and GPU buffers over wayland.

extern crate base;

#[path = "dwl.rs"]
#[allow(dead_code)]
mod dwl;

use std::cell::Cell;
use std::cmp::max;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ffi::CString;
use std::mem::zeroed;
use std::panic::catch_unwind;
use std::path::Path;
use std::process::abort;
use std::ptr::null;

use anyhow::bail;
use base::error;
use base::round_up_to_page_size;
use base::AsRawDescriptor;
use base::MemoryMapping;
use base::MemoryMappingBuilder;
use base::RawDescriptor;
use base::SharedMemory;
use base::VolatileMemory;
use dwl::*;
use linux_input_sys::virtio_input_event;
use sync::Waitable;
use vm_control::gpu::DisplayParameters;

use crate::DisplayExternalResourceImport;
use crate::DisplayT;
use crate::EventDeviceKind;
use crate::FlipToExtraInfo;
use crate::GpuDisplayError;
use crate::GpuDisplayEvents;
use crate::GpuDisplayFramebuffer;
use crate::GpuDisplayResult;
use crate::GpuDisplaySurface;
use crate::SemaphoreTimepoint;
use crate::SurfaceType;
use crate::SysDisplayT;

const BUFFER_COUNT: usize = 3;
const BYTES_PER_PIXEL: u32 = 4;

struct DwlContext(*mut dwl_context);
impl Drop for DwlContext {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY:
            // Safe given that we checked the pointer for non-null and it should always be of the
            // correct type.
            unsafe {
                dwl_context_destroy(&mut self.0);
            }
        }
    }
}

impl AsRawDescriptor for DwlContext {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        // SAFETY:
        // Safe given that the context pointer is valid.
        unsafe { dwl_context_fd(self.0) }
    }
}

struct DwlDmabuf(*mut dwl_dmabuf);

impl Drop for DwlDmabuf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY:
            // Safe given that we checked the pointer for non-null and it should always be of the
            // correct type.
            unsafe {
                dwl_dmabuf_destroy(&mut self.0);
            }
        }
    }
}

struct DwlSurface(*mut dwl_surface);
impl Drop for DwlSurface {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY:
            // Safe given that we checked the pointer for non-null and it should always be of the
            // correct type.
            unsafe {
                dwl_surface_destroy(&mut self.0);
            }
        }
    }
}

struct WaylandSurface {
    surface: DwlSurface,
    row_size: u32,
    buffer_size: usize,
    buffer_index: Cell<usize>,
    buffer_mem: MemoryMapping,
}

impl WaylandSurface {
    fn surface(&self) -> *mut dwl_surface {
        self.surface.0
    }
}

impl GpuDisplaySurface for WaylandSurface {
    fn surface_descriptor(&self) -> u64 {
        // SAFETY:
        // Safe if the surface is valid.
        let pointer = unsafe { dwl_surface_descriptor(self.surface.0) };
        pointer as u64
    }

    fn framebuffer(&mut self) -> Option<GpuDisplayFramebuffer> {
        let buffer_index = (self.buffer_index.get() + 1) % BUFFER_COUNT;
        let framebuffer = self
            .buffer_mem
            .get_slice(buffer_index * self.buffer_size, self.buffer_size)
            .ok()?;

        Some(GpuDisplayFramebuffer::new(
            framebuffer,
            self.row_size,
            BYTES_PER_PIXEL,
        ))
    }

    fn next_buffer_in_use(&self) -> bool {
        let next_buffer_index = (self.buffer_index.get() + 1) % BUFFER_COUNT;
        // SAFETY:
        // Safe because only a valid surface and buffer index is used.
        unsafe { dwl_surface_buffer_in_use(self.surface(), next_buffer_index) }
    }

    fn close_requested(&self) -> bool {
        // SAFETY:
        // Safe because only a valid surface is used.
        unsafe { dwl_surface_close_requested(self.surface()) }
    }

    fn flip(&mut self) {
        self.buffer_index
            .set((self.buffer_index.get() + 1) % BUFFER_COUNT);

        // SAFETY:
        // Safe because only a valid surface and buffer index is used.
        unsafe {
            dwl_surface_flip(self.surface(), self.buffer_index.get());
        }
    }

    fn flip_to(
        &mut self,
        import_id: u32,
        _acquire_timepoint: Option<SemaphoreTimepoint>,
        _release_timepoint: Option<SemaphoreTimepoint>,
        _extra_info: Option<FlipToExtraInfo>,
    ) -> anyhow::Result<Waitable> {
        // SAFETY:
        // Safe because only a valid surface and import_id is used.
        unsafe { dwl_surface_flip_to(self.surface(), import_id) };
        Ok(Waitable::signaled())
    }

    fn commit(&mut self) -> GpuDisplayResult<()> {
        // SAFETY:
        // Safe because only a valid surface is used.
        unsafe {
            dwl_surface_commit(self.surface());
        }

        Ok(())
    }

    fn set_position(&mut self, x: u32, y: u32) {
        // SAFETY:
        // Safe because only a valid surface is used.
        unsafe {
            dwl_surface_set_position(self.surface(), x, y);
        }
    }
}

/// A connection to the compositor and associated collection of state.
///
/// The user of `GpuDisplay` can use `AsRawDescriptor` to poll on the compositor connection's file
/// descriptor. When the connection is readable, `dispatch_events` can be called to process it.
pub struct DisplayWl {
    dmabufs: HashMap<u32, DwlDmabuf>,
    ctx: DwlContext,
    current_event: Option<dwl_event>,
    mt_tracking_id: u16,
    // iceland gvm: the window's input mode (CROSVM_GVM_INPUT_MODE) and the last pointer position
    gvm_input: crate::gvm_input::Mapper,
    gvm_pointer: (i32, i32),
    gvm_t0: std::time::Instant,
}

/// iceland gvm: turns the mode mapper's output into one report for one guest device.
fn gvm_report(outs: Vec<crate::gvm_input::Out>) -> Option<GpuDisplayEvents> {
    use crate::gvm_input::Out;
    const REL_HWHEEL: u16 = 0x06;
    if outs.is_empty() {
        return None;
    }
    let device_type = match outs[0] {
        Out::MtDown { .. } | Out::MtMove { .. } | Out::MtUp { .. } => EventDeviceKind::Touchscreen,
        Out::RelMove { .. } | Out::RelButton { .. } | Out::RelWheel { .. } | Out::RelSyn => EventDeviceKind::Mouse,
        _ => EventDeviceKind::Tablet,
    };
    let mut events = Vec::new();
    for o in outs {
        match o {
            Out::MtDown { slot, id, x, y } => events.extend([
                virtio_input_event::multitouch_slot(slot),
                virtio_input_event::multitouch_tracking_id(id),
                virtio_input_event::multitouch_absolute_x(max(0, x)),
                virtio_input_event::multitouch_absolute_y(max(0, y)),
                virtio_input_event::touch(true),
            ]),
            Out::MtMove { slot, x, y } => events.extend([
                virtio_input_event::multitouch_slot(slot),
                virtio_input_event::multitouch_absolute_x(max(0, x)),
                virtio_input_event::multitouch_absolute_y(max(0, y)),
            ]),
            Out::MtUp { slot, last } => {
                events.push(virtio_input_event::multitouch_slot(slot));
                events.push(virtio_input_event::multitouch_tracking_id(-1));
                if last {
                    events.push(virtio_input_event::touch(false));
                }
            }
            Out::AbsMove { x, y } => {
                events.push(virtio_input_event::absolute_x(max(0, x)));
                events.push(virtio_input_event::absolute_y(max(0, y)));
            }
            Out::AbsButton { code, down } | Out::RelButton { code, down } => {
                events.push(virtio_input_event::key(code, down, false))
            }
            Out::AbsWheel { v, h } => {
                if v != 0 {
                    events.push(virtio_input_event::wheel(v));
                }
                if h != 0 {
                    events.push(virtio_input_event::relative(REL_HWHEEL, h));
                }
            }
            Out::RelWheel { v } => events.push(virtio_input_event::wheel(v)),
            Out::RelMove { dx, dy } => {
                events.push(virtio_input_event::relative_x(dx));
                events.push(virtio_input_event::relative_y(dy));
            }
            Out::AbsSyn | Out::RelSyn => events.push(virtio_input_event::syn()),
        }
    }
    Some(GpuDisplayEvents { events, device_type })
}

/// Error logging callback used by wrapped C implementation.
///
/// # Safety
///
/// safe because it must be passed a valid pointer to null-terminated c-string.
#[allow(clippy::unnecessary_cast)]
unsafe extern "C" fn error_callback(message: *const ::std::os::raw::c_char) {
    catch_unwind(|| {
        assert!(!message.is_null());
        // SAFETY: trivially safe
        let msg = unsafe {
            std::str::from_utf8(std::slice::from_raw_parts(
                message as *const u8,
                libc::strlen(message),
            ))
            .unwrap()
        };
        error!("{}", msg);
    })
    .unwrap_or_else(|_| abort())
}

impl DisplayWl {
    /// Opens a fresh connection to the compositor.
    pub fn new(wayland_path: Option<&Path>) -> GpuDisplayResult<DisplayWl> {
        // SAFETY:
        // The dwl_context_new call should always be safe to call, and we check its result.
        let ctx = DwlContext(unsafe { dwl_context_new(Some(error_callback)) });
        if ctx.0.is_null() {
            return Err(GpuDisplayError::Allocate);
        }

        // The dwl_context_setup call is always safe to call given that the supplied context is
        // valid. and we check its result.
        let cstr_path = match wayland_path.map(|p| p.as_os_str().to_str()) {
            Some(Some(s)) => match CString::new(s) {
                Ok(cstr) => Some(cstr),
                Err(_) => return Err(GpuDisplayError::InvalidPath),
            },
            Some(None) => return Err(GpuDisplayError::InvalidPath),
            None => None,
        };
        // This grabs a pointer to cstr_path without moving the CString into the .map closure
        // accidentally, which triggeres a really hard to catch use after free in
        // dwl_context_setup.
        let cstr_path_ptr = cstr_path
            .as_ref()
            .map(|s: &CString| CStr::as_ptr(s))
            .unwrap_or(null());
        // SAFETY: args are valid and the return value is checked.
        let setup_success = unsafe { dwl_context_setup(ctx.0, cstr_path_ptr) };
        if !setup_success {
            return Err(GpuDisplayError::Connect);
        }

        let mode = crate::gvm_input::Mode::from_env();
        if mode != crate::gvm_input::Mode::Touch {
            base::warn!("GVM-INPUT mode {:?} (CROSVM_GVM_INPUT_MODE)", mode);
        }
        Ok(DisplayWl {
            dmabufs: HashMap::new(),
            ctx,
            current_event: None,
            mt_tracking_id: 0u16,
            gvm_input: crate::gvm_input::Mapper::new(mode),
            gvm_pointer: (0, 0),
            gvm_t0: std::time::Instant::now(),
        })
    }

    fn ctx(&self) -> *mut dwl_context {
        self.ctx.0
    }

    fn pop_event(&self) -> dwl_event {
        // SAFETY:
        // Safe because dwl_next_events from a context's circular buffer.
        unsafe {
            let mut ev = zeroed();
            dwl_context_next_event(self.ctx(), &mut ev);
            ev
        }
    }

    fn next_tracking_id(&mut self) -> i32 {
        let cur_id: i32 = self.mt_tracking_id as i32;
        self.mt_tracking_id = self.mt_tracking_id.wrapping_add(1);
        cur_id
    }

    fn current_tracking_id(&self) -> i32 {
        self.mt_tracking_id as i32
    }
}

impl DisplayT for DisplayWl {
    fn pending_events(&self) -> bool {
        // SAFETY:
        // Safe because the function just queries the values of two variables in a context.
        unsafe { dwl_context_pending_events(self.ctx()) }
    }

    fn next_event(&mut self) -> GpuDisplayResult<u64> {
        let ev = self.pop_event();
        let descriptor = ev.surface_descriptor as u64;
        self.current_event = Some(ev);
        Ok(descriptor)
    }

    fn handle_next_event(
        &mut self,
        _surface: &mut Box<dyn GpuDisplaySurface>,
    ) -> Option<GpuDisplayEvents> {
        // Should not panic since the common layer only calls this when an event occurs.
        let event = self.current_event.take().unwrap();

        match event.event_type {
            DWL_EVENT_TYPE_KEYBOARD_ENTER => None,
            DWL_EVENT_TYPE_KEYBOARD_LEAVE => None,
            DWL_EVENT_TYPE_KEYBOARD_KEY => {
                let linux_keycode = event.params[0] as u16;
                let pressed = event.params[1] == DWL_KEYBOARD_KEY_STATE_PRESSED;
                let events = vec![virtio_input_event::key(linux_keycode, pressed, false)];
                Some(GpuDisplayEvents {
                    events,
                    device_type: EventDeviceKind::Keyboard,
                })
            }
            // iceland gvm (0.2-6): display_wl.c sends the raw pointer and real fingers; the input mode
            // (gvm_input.rs) decides what reaches which guest device (touch screen, tablet, mouse)
            DWL_EVENT_TYPE_POINTER_MOVE => {
                self.gvm_pointer = (event.params[0], event.params[1]);
                gvm_report(self.gvm_input.pointer_move(event.params[0], event.params[1]))
            }
            DWL_EVENT_TYPE_POINTER_BUTTON => {
                let (x, y) = self.gvm_pointer;
                gvm_report(self.gvm_input.pointer_button(event.params[0] as u16, event.params[1] != 0, x, y))
            }
            DWL_EVENT_TYPE_POINTER_AXIS => {
                gvm_report(self.gvm_input.pointer_axis(event.params[0] as u32, event.params[1] as f64 / 256.0))
            }
            DWL_EVENT_TYPE_FINGER_DOWN => {
                let now = self.gvm_t0.elapsed().as_millis() as u64;
                gvm_report(self.gvm_input.finger_down(event.params[0], event.params[1], event.params[2], now))
            }
            DWL_EVENT_TYPE_FINGER_MOTION => {
                let now = self.gvm_t0.elapsed().as_millis() as u64;
                gvm_report(self.gvm_input.finger_motion(event.params[0], event.params[1], event.params[2], now))
            }
            DWL_EVENT_TYPE_FINGER_UP => {
                let now = self.gvm_t0.elapsed().as_millis() as u64;
                gvm_report(self.gvm_input.finger_up(event.params[0], now))
            }
            DWL_EVENT_TYPE_FINGER_CANCEL => gvm_report(self.gvm_input.finger_cancel()),
            // upstream single-touch emulation (no longer sent by display_wl.c, kept for other senders)
            DWL_EVENT_TYPE_TOUCH_DOWN | DWL_EVENT_TYPE_TOUCH_MOTION => {
                let tracking_id = if event.event_type == DWL_EVENT_TYPE_TOUCH_DOWN {
                    self.next_tracking_id()
                } else {
                    self.current_tracking_id()
                };

                let events = vec![
                    virtio_input_event::multitouch_slot(0),
                    virtio_input_event::multitouch_tracking_id(tracking_id),
                    virtio_input_event::multitouch_absolute_x(max(0, event.params[0])),
                    virtio_input_event::multitouch_absolute_y(max(0, event.params[1])),
                    virtio_input_event::touch(true),
                ];
                Some(GpuDisplayEvents {
                    events,
                    device_type: EventDeviceKind::Touchscreen,
                })
            }
            DWL_EVENT_TYPE_TOUCH_UP => {
                let events = vec![
                    virtio_input_event::multitouch_slot(0),
                    virtio_input_event::multitouch_tracking_id(-1),
                    virtio_input_event::touch(false),
                ];
                Some(GpuDisplayEvents {
                    events,
                    device_type: EventDeviceKind::Touchscreen,
                })
            }
            _ => {
                error!("unknown event type {}", event.event_type);
                None
            }
        }
    }

    fn flush(&self) {
        // SAFETY:
        // Safe given that the context pointer is valid.
        unsafe {
            dwl_context_dispatch(self.ctx());
        }
    }

    fn create_surface(
        &mut self,
        parent_surface_id: Option<u32>,
        surface_id: u32,
        scanout_id: Option<u32>,
        display_params: &DisplayParameters,
        surf_type: SurfaceType,
    ) -> GpuDisplayResult<Box<dyn GpuDisplaySurface>> {
        let parent_id = parent_surface_id.unwrap_or(0);

        let (width, height) = display_params.get_virtual_display_size();
        let row_size = width * BYTES_PER_PIXEL;
        let fb_size = row_size * height;
        let buffer_size = round_up_to_page_size(fb_size as usize * BUFFER_COUNT);
        let buffer_shm = SharedMemory::new("GpuDisplaySurface", buffer_size as u64)?;
        let buffer_mem = MemoryMappingBuilder::new(buffer_size)
            .from_shared_memory(&buffer_shm)
            .build()
            .unwrap();

        let dwl_surf_flags = match surf_type {
            SurfaceType::Cursor => DWL_SURFACE_FLAG_HAS_ALPHA,
            SurfaceType::Scanout => DWL_SURFACE_FLAG_RECEIVE_INPUT,
        };
        // SAFETY:
        // Safe because only a valid context, parent ID (if not non-zero), and buffer FD are used.
        // The returned surface is checked for validity before being filed away.
        let surface = DwlSurface(unsafe {
            dwl_context_surface_new(
                self.ctx(),
                parent_id,
                surface_id,
                buffer_shm.as_raw_descriptor(),
                buffer_size,
                fb_size as usize,
                width,
                height,
                row_size,
                dwl_surf_flags,
            )
        });

        if surface.0.is_null() {
            return Err(GpuDisplayError::CreateSurface);
        }

        if let Some(scanout_id) = scanout_id {
            // SAFETY:
            // Safe because only a valid surface is used.
            unsafe {
                dwl_surface_set_scanout_id(surface.0, scanout_id);
            }
        }

        Ok(Box::new(WaylandSurface {
            surface,
            row_size,
            buffer_size: fb_size as usize,
            buffer_index: Cell::new(0),
            buffer_mem,
        }))
    }

    fn import_resource(
        &mut self,
        import_id: u32,
        _surface_id: u32,
        external_display_resource: DisplayExternalResourceImport,
    ) -> anyhow::Result<()> {
        // This let pattern is always true if the host_display feature is disabled.
        #[allow(irrefutable_let_patterns)]
        if let DisplayExternalResourceImport::Dmabuf {
            descriptor,
            offset,
            stride,
            modifiers,
            width,
            height,
            fourcc,
        } = external_display_resource
        {
            // SAFETY:
            // Safe given that the context pointer is valid. Any other invalid parameters would be
            // rejected by dwl_context_dmabuf_new safely. We check that the resulting dmabuf is
            // valid before filing it away.
            let dmabuf = DwlDmabuf(unsafe {
                dwl_context_dmabuf_new(
                    self.ctx(),
                    import_id,
                    descriptor.as_raw_descriptor(),
                    offset,
                    stride,
                    modifiers,
                    width,
                    height,
                    fourcc,
                )
            });

            if dmabuf.0.is_null() {
                bail!("dmabuf import failed.");
            }

            self.dmabufs.insert(import_id, dmabuf);

            Ok(())
        } else {
            bail!("gpu_display_wl only supports Dmabuf imports");
        }
    }

    fn release_import(&mut self, _surface_id: u32, import_id: u32) {
        self.dmabufs.remove(&import_id);
    }
}

impl SysDisplayT for DisplayWl {}

impl AsRawDescriptor for DisplayWl {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        // Safe given that the context pointer is valid.
        self.ctx.as_raw_descriptor()
    }
}
