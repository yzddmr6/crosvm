// Copyright 2026 The iceland-mainline authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host side of the runtime GuestAccept transport for protected Gunyah VMs (iceland gvm).
//!
//! A protected guest owns its stage-2: the host cannot map a blob into it. The kernel's
//! `GH_VM_PROTECTED_SHARE{,_DMABUF}` (iceland gunyah patch 0008, the in-tree form of DroidVM's
//! gunyah_share) shares the pages as a resource-manager memparcel and returns its handle; the
//! guest must then MEM_ACCEPT that handle at the GPA where crosvm placed the mapping.
//!
//! The request reaches the guest through the `gvm-accept` virtio device (crate
//! `device_virtio_gvm_accept`), whose wire format matches the DroidVM guest module
//! `gunyah_guest.ko` (virtio id 60: queue 0 host->guest requests, queue 1 completions). The device
//! registers its host end here with [`set_accept_link`]; `GunyahVm::add_memory_region` /
//! `remove_memory_region` call [`drive`] synchronously from the vm_memory handler.
//!
//! Requires a single-process crosvm (`--disable-sandbox`): the link is a process global.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use base::error;
use base::info;
use base::Error;
use base::Result;
use base::Tube;
use libc::EIO;
use libc::ENODEV;
use libc::ETIMEDOUT;
use serde::Deserialize;
use serde::Serialize;

/// Operation carried to the guest accept module.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GvmAcceptOp {
    /// MEM_ACCEPT `handle` at `gpa` (IPA-contiguous, `size` bytes).
    Accept,
    /// MEM_RELEASE whatever the guest accepted at `gpa`.
    Release,
}

/// Request from `GunyahVm` to the gvm-accept device worker.
#[derive(Serialize, Deserialize, Debug)]
pub struct GvmAcceptRequest {
    pub seq: u64,
    pub op: GvmAcceptOp,
    pub handle: u32,
    pub gpa: u64,
    pub size: u64,
}

/// Completion relayed from the guest (0 or a negative errno).
#[derive(Serialize, Deserialize, Debug)]
pub struct GvmAcceptResponse {
    pub seq: u64,
    pub ret: i32,
}

static LINK: Mutex<Option<Tube>> = Mutex::new(None);
static SEQ: AtomicU64 = AtomicU64::new(1);

/// iceland gvm (parity step 3): what a runtime blob share costs, to decide on DroidVM-style pre-shared pools.
/// kind 0 = map (SHARE ioctl + guest MEM_ACCEPT), 1 = unmap (guest MEM_RELEASE + UNSHARE).
static STAT_N: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static STAT_US: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static STAT_MAX_US: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static STAT_BYTES: AtomicU64 = AtomicU64::new(0);

/// Records one map / unmap; every 64th map and every 64th unmap logs a GVM-ACCEPT-STATS line.
pub fn record(kind: usize, elapsed: Duration, bytes: u64) {
    let us = elapsed.as_micros() as u64;
    let n = STAT_N[kind].fetch_add(1, Ordering::Relaxed) + 1;
    STAT_US[kind].fetch_add(us, Ordering::Relaxed);
    STAT_MAX_US[kind].fetch_max(us, Ordering::Relaxed);
    if kind == 0 {
        STAT_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
    if n == 1 || n % 64 == 0 {
        log_stats();
    }
}

/// `GVM-ACCEPT-STATS maps=N map_avg_us= map_max_us= mapped_mib= unmaps=N unmap_avg_us= unmap_max_us=`
pub fn log_stats() {
    let (m, u) = (STAT_N[0].load(Ordering::Relaxed), STAT_N[1].load(Ordering::Relaxed));
    let avg = |k: usize, n: u64| if n == 0 { 0 } else { STAT_US[k].load(Ordering::Relaxed) / n };
    base::warn!(
        "GVM-ACCEPT-STATS maps={} map_avg_us={} map_max_us={} mapped_mib={} unmaps={} unmap_avg_us={} unmap_max_us={}",
        m,
        avg(0, m),
        STAT_MAX_US[0].load(Ordering::Relaxed),
        STAT_BYTES.load(Ordering::Relaxed) >> 20,
        u,
        avg(1, u),
        STAT_MAX_US[1].load(Ordering::Relaxed)
    );
}

/// How long one accept / release may take: an RM RPC from the guest is well below a millisecond,
/// the budget covers the virtqueue interrupt and the guest workqueue.
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(3);

/// Called once by the gvm-accept device when it is created.
pub fn set_accept_link(tube: Tube) {
    *LINK.lock().unwrap() = Some(tube);
}

/// True when a gvm-accept device exists in this VM.
pub fn accept_link_present() -> bool {
    LINK.lock().unwrap().is_some()
}

/// Whether the guest may need the RM-RPC message queue (set from the environment by the VMM
/// configuration: `CROSVM_GVM_ACCEPT=1`).
pub fn accept_enabled_by_env() -> bool {
    std::env::var("CROSVM_GVM_ACCEPT").map(|v| v == "1").unwrap_or(false)
}

/// One synchronous round trip through the guest accept module.
pub fn drive(op: GvmAcceptOp, handle: u32, gpa: u64, size: u64) -> Result<()> {
    let guard = LINK.lock().unwrap();
    let tube = match guard.as_ref() {
        Some(t) => t,
        None => {
            error!("gvm-accept: no accept device in this VM; cannot {:?} at {:#x}", op, gpa);
            return Err(Error::new(ENODEV));
        }
    };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    tube.send(&GvmAcceptRequest {
        seq,
        op,
        handle,
        gpa,
        size,
    })
    .map_err(|e| {
        error!("gvm-accept: send failed: {}", e);
        Error::new(EIO)
    })?;
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            error!("gvm-accept: {:?} gpa={:#x} timed out", op, gpa);
            return Err(Error::new(ETIMEDOUT));
        }
        tube.set_recv_timeout(Some(left)).map_err(|_| Error::new(EIO))?;
        match tube.recv::<GvmAcceptResponse>() {
            Ok(r) if r.seq == seq => {
                return if r.ret == 0 {
                    info!(
                        "gvm-accept: {:?} handle={:#x} gpa={:#x} size={:#x} done",
                        op, handle, gpa, size
                    );
                    Ok(())
                } else {
                    error!("gvm-accept: guest failed {:?} gpa={:#x}: {}", op, gpa, r.ret);
                    Err(Error::new(-r.ret))
                };
            }
            // A late answer to an earlier, timed-out request.
            Ok(_) => continue,
            Err(e) => {
                error!("gvm-accept: recv failed: {}", e);
                return Err(Error::new(ETIMEDOUT));
            }
        }
    }
}
