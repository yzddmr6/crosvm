// Copyright 2026 iceland gvm. Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! iceland gvm: scanout readback for the copy path of 3D (virgl) scanouts.
//!
//! On the OnePlus Pad 4 (GLES 3.2 host, Adreno 840) the window stayed black although the guest's
//! desktop was drawn (V4 desktop runs, 24 Oct). This module gives the copy path three sources:
//!
//! * `ctx0`    - virglrenderer transfer_read on context 0 (the upstream copy path);
//! * `dmabuf`  - mmap of the resource's exported dmabuf (only for a LINEAR layout), read on the CPU;
//! * `backing` - the guest's backing pages of the resource (what a 2D / dumb resource was written
//!               from with TRANSFER_TO_HOST_2D).
//!
//! `CROSVM_GVM_SCANOUT_READ=auto|ctx0|dmabuf|backing` picks the source (default `auto`: the first
//! source whose frame is not entirely black, remembered per resource).
//! `CROSVM_GVM_DUMP_FRAME=<dir>` logs one line of statistics for every source every 5 s (max 5
//! times) and writes the shown frame as `<dir>/gvm-frame-<n>.ppm`.

use std::cell::RefCell;
use std::collections::BTreeMap as Map;
use std::io::IoSliceMut;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use base::warn;
use rutabaga_gfx::Rutabaga;
use rutabaga_gfx::RutabagaHandle;
use rutabaga_gfx::RutabagaIntoRawDescriptor;
use rutabaga_gfx::RutabagaMagmaHandle;
use rutabaga_gfx::Transfer3D;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadMode {
    Auto,
    Ctx0,
    Dmabuf,
    Backing,
}

impl ReadMode {
    fn name(self) -> &'static str {
        match self {
            ReadMode::Auto => "auto",
            ReadMode::Ctx0 => "ctx0",
            ReadMode::Dmabuf => "dmabuf",
            ReadMode::Backing => "backing",
        }
    }
}

fn read_mode() -> ReadMode {
    static MODE: OnceLock<ReadMode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("CROSVM_GVM_SCANOUT_READ").as_deref() {
        Ok("ctx0") => ReadMode::Ctx0,
        Ok("dmabuf") => ReadMode::Dmabuf,
        Ok("backing") => ReadMode::Backing,
        _ => ReadMode::Auto,
    })
}

fn dump_dir() -> Option<&'static str> {
    static DIR: OnceLock<Option<String>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var("CROSVM_GVM_DUMP_FRAME").ok().filter(|s| !s.is_empty()))
        .as_deref()
}

/// CROSVM_GVM_DUMP_EVERY (seconds, default 5) / CROSVM_GVM_DUMP_MAX (default 5): dump cadence; a scanout
/// resource change always dumps (vm-run 24 Oct: 5 dumps in the first 25 s only saw the boot console).
fn dump_cfg() -> (u64, u32) {
    static CFG: OnceLock<(u64, u32)> = OnceLock::new();
    *CFG.get_or_init(|| {
        let n = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
        (n("CROSVM_GVM_DUMP_EVERY", 5).max(1), n("CROSVM_GVM_DUMP_MAX", 5) as u32)
    })
}

/// CROSVM_GVM_TRACE_SCANOUT=1: every SET_SCANOUT / RESOURCE_FLUSH (first 400), to see what an atomic-KMS
/// guest really sends (vm-run v9: atomic kmscube never reached the scanout).
pub fn trace(text: std::fmt::Arguments) {
    static ON: OnceLock<bool> = OnceLock::new();
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if !*ON.get_or_init(|| std::env::var("CROSVM_GVM_TRACE_SCANOUT").is_ok_and(|v| v == "1")) {
        return;
    }
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 400 {
        warn!("GVM-TRACE {}", text);
    }
}

static DROPPED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The copy path skipped a flush because the compositor still held the next buffer.
pub fn dropped(resource_id: u32) {
    use std::sync::atomic::Ordering::Relaxed;
    DROPPED.store(resource_id, Relaxed);
    let n = DROPS.fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() {
        warn!("GVM-DROP {} flushes deferred (compositor held the next buffer), last res={}", n, resource_id);
    }
}

/// A frame of this resource reached the window: nothing pending for it any more.
pub fn presented(resource_id: u32) {
    let _ = DROPPED.compare_exchange(
        resource_id,
        0,
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The resource of a deferred frame, if any (cleared; a new drop sets it again).
pub fn take_dropped() -> Option<u32> {
    match DROPPED.swap(0, std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        r => Some(r),
    }
}

/// CROSVM_GVM_REFRESH_MS set: the refresh timer re-presents every scanout (else it only retries drops).
pub fn refresh_all() -> bool {
    static ALL: OnceLock<bool> = OnceLock::new();
    *ALL.get_or_init(|| std::env::var("CROSVM_GVM_REFRESH_MS").is_ok_and(|v| v.parse::<u64>().is_ok_and(|n| n > 0)))
}

/// One warn line per event kind and resource (at most 64 lines): which resources the guest flushes and
/// which one a scanout shows.
pub fn note(kind: &str, resource_id: u32, text: &str) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.notes.len() < 64 && s.notes.insert((kind.to_string(), resource_id)) {
            warn!("GVM-{} res={} {}", kind, resource_id, text);
        }
    })
}

#[derive(Default)]
struct State {
    /// (event kind, resource id) already logged by note()
    notes: std::collections::BTreeSet<(String, u32)>,
    last_dump_res: u32,
    /// resource id -> guest backing (host address, length) as attached by the guest
    backing: Map<u32, Vec<(usize, usize)>>,
    /// resource id -> the source that last gave a non-black frame (auto mode)
    chosen: Map<u32, ReadMode>,
    /// auto mode: no re-probing until then after an all-black frame
    black_until: Option<Instant>,
    last_dump: Option<Instant>,
    dumps: u32,
    flushes: u64,
    errors_logged: u32,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Records the guest backing of a resource (called from attach_backing).
pub fn set_backing(resource_id: u32, iovecs: &[(usize, usize)]) {
    STATE.with(|s| {
        s.borrow_mut().backing.insert(resource_id, iovecs.to_vec());
    });
}

/// Forgets a resource (detach_backing / unref).
pub fn forget(resource_id: u32, unref: bool) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.backing.remove(&resource_id);
        if unref {
            s.chosen.remove(&resource_id);
        }
    });
}

struct Stats {
    mean: f64,
    nonzero: f64,
}

/// Mean of the colour bytes (0..1) and the fraction of pixels whose colour is not 0, sampled on a
/// grid of about `grid` x `grid` pixels. The layout is 4 bytes per pixel, BGRX.
fn stats(buf: &[u8], width: u32, height: u32, stride: u32, grid: u32) -> Stats {
    let (w, h, st) = (width as usize, height as usize, stride as usize);
    let sx = (w / grid as usize).max(1);
    let sy = (h / grid as usize).max(1);
    let (mut sum, mut n, mut nz) = (0u64, 0u64, 0u64);
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let o = y * st + x * 4;
            if o + 3 <= buf.len() {
                let (b, g, r) = (buf[o], buf[o + 1], buf[o + 2]);
                sum += b as u64 + g as u64 + r as u64;
                n += 1;
                if (b | g | r) != 0 {
                    nz += 1;
                }
            }
            x += sx;
        }
        y += sy;
    }
    if n == 0 {
        return Stats { mean: 0.0, nonzero: 0.0 };
    }
    Stats {
        mean: sum as f64 / (n as f64 * 3.0 * 255.0),
        nonzero: nz as f64 / n as f64,
    }
}

fn is_black(buf: &[u8], width: u32, height: u32, stride: u32) -> bool {
    stats(buf, width, height, stride, 64).nonzero == 0.0
}

fn read_ctx0(
    rutabaga: &mut Rutabaga,
    resource_id: u32,
    width: u32,
    height: u32,
    stride: u32,
    dst: &mut [u8],
) -> Result<String, String> {
    let mut transfer = Transfer3D::new_2d(0, 0, width, height, 0);
    transfer.stride = stride;
    rutabaga
        .transfer_read(0, resource_id, transfer, Some(IoSliceMut::new(dst)))
        .map_err(|e| format!("transfer_read: {}", e))?;
    Ok(String::new())
}

const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241; // 'AB24'
const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258; // 'XB24'
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_END: u64 = 4;

fn fourcc_str(f: u32) -> String {
    f.to_le_bytes()
        .iter()
        .map(|&c| if c.is_ascii_graphic() { c as char } else { '?' })
        .collect()
}

/// Layout of the exported resource: "fourcc=XR24 mod=0x0 stride=6400 offset=0".
fn describe(rutabaga: &Rutabaga, resource_id: u32) -> String {
    match rutabaga.resource3d_info(resource_id) {
        Ok(i) => format!(
            "fourcc={} mod={:#x} stride={} offset={}",
            fourcc_str(i.drm_fourcc),
            i.modifier,
            i.strides[0],
            i.offsets[0]
        ),
        Err(_) => "no-3d-info".to_string(),
    }
}

fn read_dmabuf(
    rutabaga: &mut Rutabaga,
    resource_id: u32,
    width: u32,
    height: u32,
    dst_stride: u32,
    dst: &mut [u8],
) -> Result<String, String> {
    let info = rutabaga
        .resource3d_info(resource_id)
        .map_err(|e| format!("no 3d info: {}", e))?;
    if info.modifier != DRM_FORMAT_MOD_LINEAR && info.modifier != DRM_FORMAT_MOD_INVALID {
        return Err(format!("modifier {:#x} is not linear", info.modifier));
    }
    let (src_stride, offset) = (info.strides[0] as usize, info.offsets[0] as usize);
    if src_stride < width as usize * 4 {
        return Err(format!("stride {} < width*4", src_stride));
    }
    let handle = match rutabaga
        .export_blob(resource_id)
        .map_err(|e| format!("export_blob: {}", e))?
    {
        RutabagaHandle::AhbInfo(_) => return Err("AHB handle".to_string()),
        other => RutabagaMagmaHandle::try_from(other).map_err(|_| "not a fd handle".to_string())?,
    };
    let fd = handle.os_handle.into_raw_descriptor();
    let len = offset + src_stride * height as usize;
    // SAFETY: fd is a dmabuf we own (closed below); the mapping is read-only and unmapped below.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        let e = std::io::Error::last_os_error();
        // SAFETY: we own fd.
        unsafe { libc::close(fd) };
        return Err(format!("mmap {} bytes: {}", len, e));
    }
    let mut sync: u64 = DMA_BUF_SYNC_READ;
    // SAFETY: valid dmabuf fd and pointer to a u64 (struct dma_buf_sync).
    unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &mut sync as *mut u64) };
    let swap = info.drm_fourcc == DRM_FORMAT_ABGR8888 || info.drm_fourcc == DRM_FORMAT_XBGR8888;
    let row = (width as usize * 4).min(dst_stride as usize);
    for y in 0..height as usize {
        let d0 = y * dst_stride as usize;
        if d0 + row > dst.len() {
            break;
        }
        // SAFETY: offset + y*src_stride + row <= len (row <= width*4 <= src_stride).
        let src = unsafe {
            std::slice::from_raw_parts((base as *const u8).add(offset + y * src_stride), row)
        };
        let d = &mut dst[d0..d0 + row];
        d.copy_from_slice(src);
        if swap {
            for px in d.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
    }
    sync = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END;
    // SAFETY: as above; then unmap and close what we mapped / own.
    unsafe {
        libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &mut sync as *mut u64);
        libc::munmap(base, len);
        libc::close(fd);
    }
    Ok(format!("{}{}", fourcc_str(info.drm_fourcc), if swap { " swapped" } else { "" }))
}

/// Copies the guest backing (linear, `width * 4` bytes per row) into dst.
fn read_backing(
    backing: &[(usize, usize)],
    width: u32,
    height: u32,
    dst_stride: u32,
    dst: &mut [u8],
) -> Result<String, String> {
    let total: usize = backing.iter().map(|&(_, l)| l).sum();
    let src_stride = width as usize * 4;
    if total < src_stride * height as usize {
        return Err(format!("backing {} bytes < {}", total, src_stride * height as usize));
    }
    let row = src_stride.min(dst_stride as usize);
    let (mut seg, mut seg_off) = (0usize, 0usize);
    for y in 0..height as usize {
        let d0 = y * dst_stride as usize;
        let mut done = 0usize;
        // source rows are contiguous in the backing; walk the segments
        let mut want = src_stride;
        while want > 0 && seg < backing.len() {
            let (addr, len) = backing[seg];
            let n = want.min(len - seg_off);
            if done < row {
                let c = n.min(row - done);
                if d0 + done + c <= dst.len() {
                    // SAFETY: addr..addr+len is a mapping of guest memory recorded at attach_backing.
                    let src = unsafe { std::slice::from_raw_parts((addr + seg_off) as *const u8, c) };
                    dst[d0 + done..d0 + done + c].copy_from_slice(src);
                }
                done += c;
            }
            seg_off += n;
            want -= n;
            if seg_off == len {
                seg += 1;
                seg_off = 0;
            }
        }
    }
    Ok(format!("{} segments", backing.len()))
}

fn read_from(
    mode: ReadMode,
    rutabaga: &mut Rutabaga,
    resource_id: u32,
    width: u32,
    height: u32,
    stride: u32,
    dst: &mut [u8],
    backing: Option<&Vec<(usize, usize)>>,
) -> Result<String, String> {
    match mode {
        ReadMode::Ctx0 | ReadMode::Auto => read_ctx0(rutabaga, resource_id, width, height, stride, dst),
        ReadMode::Dmabuf => read_dmabuf(rutabaga, resource_id, width, height, stride, dst),
        ReadMode::Backing => match backing {
            Some(b) => read_backing(b, width, height, stride, dst),
            None => Err("no backing attached".to_string()),
        },
    }
}

fn write_ppm(path: &str, buf: &[u8], width: u32, height: u32, stride: u32) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(width as usize * height as usize * 3 + 32);
    write!(out, "P6\n{} {}\n255\n", width, height)?;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let o = y * stride as usize + x * 4;
            if o + 3 <= buf.len() {
                out.extend_from_slice(&[buf[o + 2], buf[o + 1], buf[o]]);
            } else {
                out.extend_from_slice(&[0, 0, 0]);
            }
        }
    }
    std::fs::write(path, out)
}

/// Fills `dst` (the shm framebuffer, BGRX, `stride` bytes per row) with the scanout resource.
/// Returns the source that was used.
pub fn fill(
    rutabaga: &mut Rutabaga,
    resource_id: u32,
    blob: bool,
    width: u32,
    height: u32,
    stride: u32,
    dst: &mut [u8],
) -> ReadMode {
    STATE.with(|st| {
        let mut st = st.borrow_mut();
        st.flushes += 1;
        let backing = st.backing.get(&resource_id).cloned();
        let want = read_mode();

        // statistics of every source, every 5 s, at most 5 times
        let (every, max) = dump_cfg();
        let dump = dump_dir().is_some()
            && st.dumps < max
            && st.last_dump.is_none_or(|t| {
                // a scanout resource change dumps at once, but at most once a second (v3 run: page flips
                // between 3 resources made a dump per flip); otherwise every `every` seconds
                (st.last_dump_res != resource_id && t.elapsed() >= Duration::from_secs(1))
                    || t.elapsed() >= Duration::from_secs(every)
            });
        if dump {
            let mut line = format!(
                "GVM-FRAME {} res={} {}x{} blob={} backing={} {} flush#{}",
                st.dumps,
                resource_id,
                width,
                height,
                blob as u8,
                backing.as_ref().map_or(0, |b| b.iter().map(|&(_, l)| l).sum::<usize>()),
                describe(rutabaga, resource_id),
                st.flushes
            );
            let mut tmp = vec![0u8; dst.len()];
            // the CPU read of the exported dmabuf only on request (CROSVM_GVM_DUMP_DMABUF=1): v3 run, the tablet
            // reset shortly after the last dump of a page-flipping glamor desktop; keep the dumps minimal
            let dmabuf = std::env::var("CROSVM_GVM_DUMP_DMABUF").is_ok_and(|v| v == "1");
            for m in [ReadMode::Ctx0, ReadMode::Dmabuf, ReadMode::Backing] {
                if m == ReadMode::Dmabuf && !dmabuf {
                    continue;
                }
                tmp.iter_mut().for_each(|b| *b = 0);
                match read_from(m, rutabaga, resource_id, width, height, stride, &mut tmp, backing.as_ref()) {
                    Ok(note) => {
                        let s = stats(&tmp, width, height, stride, 256);
                        line += &format!(
                            " | {} mean={:.3} nonzero={:.3}{}",
                            m.name(),
                            s.mean,
                            s.nonzero,
                            if note.is_empty() { String::new() } else { format!(" ({})", note) }
                        );
                    }
                    Err(e) => line += &format!(" | {} ERR {}", m.name(), e),
                }
            }
            warn!("{}", line);
        }

        let mut used = want;
        if want != ReadMode::Auto {
            if let Err(e) = read_from(want, rutabaga, resource_id, width, height, stride, dst, backing.as_ref()) {
                if st.errors_logged < 10 {
                    st.errors_logged += 1;
                    warn!("GVM-SCANOUT res={} {} read failed: {}; using ctx0", resource_id, want.name(), e);
                }
                let _ = read_ctx0(rutabaga, resource_id, width, height, stride, dst);
                used = ReadMode::Ctx0;
            }
        } else {
            let first = st.chosen.get(&resource_id).copied().unwrap_or(ReadMode::Ctx0);
            let ok = read_from(first, rutabaga, resource_id, width, height, stride, dst, backing.as_ref()).is_ok();
            used = first;
            let probe = !ok
                || (is_black(dst, width, height, stride)
                    && st.black_until.is_none_or(|t| Instant::now() >= t));
            if probe {
                let mut found = None;
                for m in [ReadMode::Ctx0, ReadMode::Dmabuf, ReadMode::Backing] {
                    if m == first {
                        continue;
                    }
                    if read_from(m, rutabaga, resource_id, width, height, stride, dst, backing.as_ref()).is_ok()
                        && !is_black(dst, width, height, stride)
                    {
                        found = Some(m);
                        break;
                    }
                }
                match found {
                    Some(m) => {
                        if st.chosen.get(&resource_id) != Some(&m) {
                            warn!(
                                "GVM-SCANOUT res={} {}: {} frame black, showing {} ({})",
                                resource_id,
                                describe(rutabaga, resource_id),
                                first.name(),
                                m.name(),
                                if blob { "blob" } else { "classic" }
                            );
                        }
                        st.chosen.insert(resource_id, m);
                        used = m;
                    }
                    None => {
                        // every source black (a black screen, or nothing works): show ctx0, probe again in 1 s
                        let _ = read_from(first, rutabaga, resource_id, width, height, stride, dst, backing.as_ref());
                        st.black_until = Some(Instant::now() + Duration::from_secs(1));
                    }
                }
            }
        }

        if dump {
            if let Some(dir) = dump_dir() {
                let path = format!("{}/gvm-frame-{}.ppm", dir, st.dumps);
                let s = stats(dst, width, height, stride, 256);
                match write_ppm(&path, dst, width, height, stride) {
                    Ok(()) => warn!(
                        "GVM-FRAME {} shown={} mean={:.3} nonzero={:.3} -> {}",
                        st.dumps,
                        used.name(),
                        s.mean,
                        s.nonzero,
                        path
                    ),
                    Err(e) => warn!("GVM-FRAME {} shown={} (write {}: {})", st.dumps, used.name(), path, e),
                }
            }
            st.dumps += 1;
            st.last_dump = Some(Instant::now());
            st.last_dump_res = resource_id;
        }
        used
    })
}
