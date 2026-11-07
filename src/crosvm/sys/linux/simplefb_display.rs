// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright DroidVM contributors
// Additional permissions apply; see ADDITIONAL-PERMISSIONS in the repository root.
//
// iceland gvm: the `--simplefb` display bridge. Ported in reduced form from DroidVM dvm-crosvm
// 849e05f, src/crosvm/sys/linux/simplefb_display.rs: a thread polls the shared framebuffer at
// poll-hz and presents it on a gpu_display surface; the data-cache invalidation before each read
// (`dcache_line_size`, `invalidate_dcache_range`) is theirs. The Wayland window, the input routing,
// the change detection and the GVM-SIMPLEFB statistics lines are iceland's. See CREDITS.
//
// No GPU transport, no VNC / Android exporters: one Wayland window through the existing
// gpu_display backend (window title / app id / server-side decoration / GVM-WINDOW-CLOSE come from
// display_wl.c as for the virtio-gpu window), CPU copy only.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use base::error;
use base::info;
use base::warn;
use base::EventToken;
use base::SendTube;
use base::VmEventType;
use base::VolatileSlice;
use base::WaitContext;
use gpu_display::EventDevice;
use gpu_display::GpuDisplay;
use gpu_display::GpuDisplayError;
use gpu_display::GpuDisplayExt;
use gpu_display::SurfaceType;
use gpu_display::SysGpuDisplayExt;
use vm_control::gpu::DisplayMode;
use vm_control::gpu::DisplayParameters;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;

/// What the bridge needs to know about the framebuffer and the window.
pub struct SimplefbParams {
    /// Guest physical address and size of the SharedFramebuffer region.
    pub addr: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Row pitch in bytes, as written into the device tree.
    pub stride: u32,
    /// Device-tree format string ("a8r8g8b8" / "x8r8g8b8": B,G,R,A/X bytes = the window's
    /// XRGB8888 / ARGB8888 layout, so rows are copied as they are).
    pub format: String,
    pub poll_hz: u32,
    /// `--wayland-sock` (the unnamed one), else $WAYLAND_DISPLAY.
    pub wayland_socket: Option<PathBuf>,
}

/// Logs the GVM-SIMPLEFB line and starts the bridge thread. The thread lives until the
/// compositor goes away or the window is closed with CROSVM_GVM_CLOSE=exit; the VM itself keeps
/// running headless if the window cannot be opened.
pub fn start(
    guest_mem: GuestMemory,
    params: SimplefbParams,
    event_devices: Vec<EventDevice>,
    vm_evt_wrtube: SendTube,
) -> Result<()> {
    warn!(
        "GVM-SIMPLEFB {}x{} {} stride={} base={:#x} size={:#x} shared poll-hz={} input-devices={}",
        params.width,
        params.height,
        params.format,
        params.stride,
        params.addr,
        params.size,
        params.poll_hz,
        event_devices.len(),
    );
    let fb_bytes = params.stride as u64 * params.height as u64;
    anyhow::ensure!(
        fb_bytes <= params.size,
        "simplefb: {fb_bytes:#x} bytes of framebuffer in a {:#x} region",
        params.size
    );
    thread::Builder::new()
        .name("simplefb_display".into())
        .spawn(move || {
            if let Err(e) = bridge(guest_mem, &params, event_devices, &vm_evt_wrtube) {
                error!("GVM-SIMPLEFB bridge stopped: {:#}", e);
            }
        })
        .context("failed to spawn the simplefb display thread")?;
    Ok(())
}

/// Length of a data cache line, from `CTR_EL0.DminLine` -- the granule `dc civac` operates on.
/// (DroidVM.)
#[cfg(target_arch = "aarch64")]
fn dcache_line_size() -> usize {
    let ctr: u64;
    // SAFETY: a read of a system register with no side effects. `CTR_EL0` is readable from EL0 on
    // every arm64 Linux (trapped and emulated where the hardware does not allow it).
    unsafe {
        std::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags));
    }
    // DminLine is log2 of the line length in WORDS, not bytes.
    4usize << ((ctr >> 16) & 0xf)
}

/// Drops the host's cached copy of `[ptr, ptr + len)` so the next read of it comes from memory.
/// (DroidVM.) The guest maps the framebuffer write-combining while the host mapping is
/// cacheable, so without this a read can return a line the guest has since overwritten.
/// `dc civac` is clean-and-invalidate; this mapping is only ever read, so its lines are clean and
/// nothing is written back over the guest. `civac` is allowed at EL0 (SCTLR_EL1.UCI).
#[cfg(target_arch = "aarch64")]
fn invalidate_dcache_range(ptr: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    let line = dcache_line_size();
    let mut addr = (ptr as usize) & !(line - 1);
    let end = (ptr as usize).saturating_add(len);
    while addr < end {
        // SAFETY: `dc civac` takes a virtual address and affects only cache state. The range walked
        // is the caller's mapping, rounded down to a line boundary at the start.
        unsafe {
            std::arch::asm!("dc civac, {}", in(reg) addr, options(nostack, preserves_flags));
        }
        addr += line;
    }
    // SAFETY: a barrier; no memory operands.
    unsafe {
        std::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn invalidate_dcache_range(_ptr: *const u8, _len: usize) {}

/// Mean of the colour bytes (0..1) and the fraction of non-black pixels, sampled on a grid of
/// about 64 x 64 pixels (same definition as the GVM-FRAME lines of the virtio-gpu path).
fn stats(buf: &[u8], width: usize, height: usize, stride: usize) -> (f64, f64) {
    let sx = (width / 64).max(1);
    let sy = (height / 64).max(1);
    let (mut sum, mut n, mut nz) = (0u64, 0u64, 0u64);
    for y in (0..height).step_by(sy) {
        for x in (0..width).step_by(sx) {
            let o = y * stride + x * 4;
            if o + 3 <= buf.len() {
                let (b, g, r) = (buf[o], buf[o + 1], buf[o + 2]);
                sum += b as u64 + g as u64 + r as u64;
                n += 1;
                if (b | g | r) != 0 {
                    nz += 1;
                }
            }
        }
    }
    if n == 0 {
        return (0.0, 0.0);
    }
    (
        sum as f64 / (n as f64 * 3.0 * 255.0),
        nz as f64 / n as f64,
    )
}

/// Copies one frame (`src`, rows of `src_stride`) into the surface's next buffer and flips.
/// Returns false when the surface had no buffer to give.
fn present(display: &mut GpuDisplay, surface_id: u32, src: &mut [u8], params: &SimplefbParams) -> bool {
    let Some(fb) = display.framebuffer(surface_id) else {
        return false;
    };
    let dst = fb.as_volatile_slice();
    let dst_stride = fb.stride() as usize;
    let src_stride = params.stride as usize;
    let row = (params.width as usize * 4).min(dst_stride).min(src_stride);
    let height = params.height as usize;
    if dst_stride == src_stride {
        let len = (src_stride * height).min(dst.size()).min(src.len());
        VolatileSlice::new(&mut src[..len]).copy_to_volatile_slice(dst);
    } else {
        for y in 0..height {
            let Ok(d) = dst.sub_slice(y * dst_stride, row) else {
                break;
            };
            let s = &mut src[y * src_stride..y * src_stride + row];
            VolatileSlice::new(s).copy_to_volatile_slice(d);
        }
    }
    display.flip(surface_id);
    true
}

#[derive(EventToken)]
enum Token {
    Display,
}

fn bridge(
    guest_mem: GuestMemory,
    params: &SimplefbParams,
    event_devices: Vec<EventDevice>,
    vm_evt_wrtube: &SendTube,
) -> Result<()> {
    let mut display = GpuDisplay::open_wayland(params.wayland_socket.as_ref())
        .context("simplefb: failed to open the Wayland display")?;
    for dev in event_devices {
        display
            .import_event_device(dev)
            .context("simplefb: failed to import an input device")?;
    }
    let surface_id = display
        .create_surface(
            None,
            None,
            &DisplayParameters::default_with_mode(DisplayMode::Windowed(
                params.width,
                params.height,
            )),
            SurfaceType::Scanout,
        )
        .context("simplefb: failed to create the window surface")?;
    let wait_ctx: WaitContext<Token> =
        WaitContext::new().context("simplefb: failed to create the wait context")?;
    wait_ctx
        .add(&display, Token::Display)
        .context("simplefb: failed to wait on the display")?;

    let fb_len = params.stride as usize * params.height as usize;
    let mut cur = vec![0u8; fb_len];
    let mut shown = vec![0u8; fb_len];
    // Nothing presented yet: the first tick presents whatever is there (the window maps on it).
    let mut pending = true;
    let mut frames: u64 = 0;
    let tick = Duration::from_nanos(1_000_000_000 / params.poll_hz.max(1) as u64);
    let started = Instant::now();
    let mut next_tick = started;
    let mut next_stats = Duration::from_secs(10);
    info!(
        "simplefb: window {}x{} on {}, polling at {} Hz",
        params.width,
        params.height,
        params
            .wayland_socket
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "$WAYLAND_DISPLAY".to_string()),
        params.poll_hz
    );

    loop {
        // Compositor and input events until the next tick, so input is not held back by polling.
        let timeout = next_tick.saturating_duration_since(Instant::now());
        let events = wait_ctx
            .wait_timeout(timeout)
            .context("simplefb: wait failed")?;
        if !events.is_empty() || timeout.is_zero() {
            match display.dispatch_events() {
                Ok(()) => {}
                Err(GpuDisplayError::ConnectionBroken) => {
                    info!("GVM-SIMPLEFB the compositor closed the connection; no window from now on");
                    return Ok(());
                }
                Err(GpuDisplayError::IoError(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e).context("simplefb: dispatch_events failed"),
            }
            if display.close_requested(surface_id) {
                // Only with CROSVM_GVM_CLOSE=exit (display_wl.c otherwise just reports
                // GVM-WINDOW-CLOSE): the upstream behaviour, the VM exits.
                info!("GVM-SIMPLEFB window closed: exiting the VM");
                let _ = vm_evt_wrtube.send::<VmEventType>(&VmEventType::Exit);
                return Ok(());
            }
        }
        let now = Instant::now();
        if now < next_tick {
            continue;
        }
        next_tick += tick;
        if next_tick < now {
            next_tick = now + tick; // fell behind: do not try to catch up
        }

        let src = guest_mem
            .get_slice_at_addr(GuestAddress(params.addr), fb_len)
            .context("simplefb: framebuffer not readable")?;
        invalidate_dcache_range(src.as_ptr(), fb_len);
        src.copy_to_volatile_slice(VolatileSlice::new(&mut cur));
        if cur != shown {
            std::mem::swap(&mut cur, &mut shown);
            pending = true;
        }
        if pending && !display.next_buffer_in_use(surface_id) {
            if present(&mut display, surface_id, &mut shown, params) {
                pending = false;
                frames += 1;
            }
        }

        let elapsed = now.duration_since(started);
        if elapsed >= next_stats {
            let (mean, nonzero) = stats(
                &shown,
                params.width as usize,
                params.height as usize,
                params.stride as usize,
            );
            warn!(
                "GVM-SIMPLEFB-FRAME {} t={}s mean={:.3} nonzero={:.3}",
                frames,
                elapsed.as_secs(),
                mean,
                nonzero
            );
            // every 10 s for the first minute, then once a minute
            next_stats += if next_stats < Duration::from_secs(60) {
                Duration::from_secs(10)
            } else {
                Duration::from_secs(60)
            };
        }
    }
}
