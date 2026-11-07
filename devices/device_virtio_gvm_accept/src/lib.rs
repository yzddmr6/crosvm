// Copyright 2026 The iceland-mainline authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! gvm-accept: virtio transport that asks a protected Gunyah guest to MEM_ACCEPT / MEM_RELEASE
//! runtime-shared memparcels (iceland gvm, V4).
//!
//! Wire format = the DroidVM guest module `gunyah_guest.ko` (droidvm-guest-additions, GPL-2.0+;
//! virtio id 60), so that module can be used unchanged in the guest:
//! - queue 0 (request): the guest posts device-writable 32-byte buffers; the device fills one per
//!   request `{le32 req_id, le32 op (1 accept, 2 release), le32 handle, le32 flags 0, le64 gpa,
//!   le64 size}`;
//! - queue 1 (completion): the guest sends `{le32 req_id, le32 ret}` (0 or -errno);
//! - queue 2 (pool): DroidVM's growable-pool control plane, not implemented here: every request
//!   gets `ret = -ENOSYS`.
//!
//! The host end of the link is registered with `hypervisor::gunyah::gvm_accept::set_accept_link`;
//! `GunyahVm` drives it synchronously from the vm_memory handler. Single-process crosvm only.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Read;
use std::io::Write;

use anyhow::anyhow;
use anyhow::Context;
use base::error;
use base::warn;
use base::Event;
use base::EventToken;
use base::RawDescriptor;
use base::ReadNotifier;
use base::Tube;
use base::WaitContext;
use base::WorkerThread;
use devices::virtio;
use devices::virtio::DeviceType;
use devices::virtio::Interrupt;
use devices::virtio::Queue;
use devices::virtio::VirtioDevice;
use devices::VirtioDeviceArgs;
use devices::VirtioDeviceModule;
use hypervisor::gunyah::gvm_accept::set_accept_link;
use hypervisor::gunyah::gvm_accept::GvmAcceptOp;
use hypervisor::gunyah::gvm_accept::GvmAcceptRequest;
use hypervisor::gunyah::gvm_accept::GvmAcceptResponse;
#[cfg(any(target_os = "android", target_os = "linux"))]
use jail::JailConfig;
#[cfg(any(target_os = "android", target_os = "linux"))]
use minijail::Minijail;
use snapshot::AnySnapshot;
use vm_memory::GuestMemory;

/// Non-standard virtio id used by the DroidVM guest module.
pub const VIRTIO_ID_GUNYAH_ACCEPT: u32 = 60;
const QUEUE_SIZE: u16 = 64;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE, QUEUE_SIZE, QUEUE_SIZE];
const ENOSYS: i32 = 38;

struct Worker {
    req_q: Queue,
    comp_q: Queue,
    pool_q: Queue,
    tube: Tube,
    pending: VecDeque<GvmAcceptRequest>,
    in_flight: HashMap<u32, u64>,
}

impl Worker {
    /// Hand queued host requests to guest-posted request buffers.
    fn push_requests(&mut self) {
        let mut sent = false;
        while let Some(req) = self.pending.front() {
            let Some(mut desc) = self.req_q.pop() else {
                break;
            };
            let req_id = req.seq as u32;
            let op: u32 = match req.op {
                GvmAcceptOp::Accept => 1,
                GvmAcceptOp::Release => 2,
            };
            let mut buf = [0u8; 32];
            buf[0..4].copy_from_slice(&req_id.to_le_bytes());
            buf[4..8].copy_from_slice(&op.to_le_bytes());
            buf[8..12].copy_from_slice(&req.handle.to_le_bytes());
            buf[16..24].copy_from_slice(&req.gpa.to_le_bytes());
            buf[24..32].copy_from_slice(&req.size.to_le_bytes());
            if let Err(e) = desc.writer.write_all(&buf) {
                error!("gvm-accept: request buffer too small: {}", e);
            }
            self.in_flight.insert(req_id, req.seq);
            self.pending.pop_front();
            self.req_q.add_used(desc);
            sent = true;
        }
        if sent {
            self.req_q.trigger_interrupt();
        }
    }

    /// Guest completions -> host responses.
    fn take_completions(&mut self) {
        let mut used = false;
        while let Some(mut desc) = self.comp_q.pop() {
            let mut buf = [0u8; 8];
            match desc.reader.read_exact(&mut buf) {
                Ok(()) => {
                    let req_id = u32::from_le_bytes(buf[0..4].try_into().unwrap());
                    let ret = i32::from_le_bytes(buf[4..8].try_into().unwrap());
                    match self.in_flight.remove(&req_id) {
                        Some(seq) => {
                            if let Err(e) = self.tube.send(&GvmAcceptResponse { seq, ret }) {
                                error!("gvm-accept: response send failed: {}", e);
                            }
                        }
                        None => warn!("gvm-accept: completion for unknown req {}", req_id),
                    }
                }
                Err(e) => warn!("gvm-accept: short completion: {}", e),
            }
            self.comp_q.add_used(desc);
            used = true;
        }
        if used {
            self.comp_q.trigger_interrupt();
        }
    }

    /// DroidVM pool requests: not supported, answer -ENOSYS.
    fn refuse_pool(&mut self) {
        let mut used = false;
        while let Some(mut desc) = self.pool_q.pop() {
            let mut req = [0u8; 32];
            let req_id = match desc.reader.read_exact(&mut req) {
                Ok(()) => u32::from_le_bytes(req[0..4].try_into().unwrap()),
                Err(_) => 0,
            };
            let mut resp = [0u8; 16];
            resp[0..4].copy_from_slice(&req_id.to_le_bytes());
            resp[4..8].copy_from_slice(&(-ENOSYS).to_le_bytes());
            let _ = desc.writer.write_all(&resp);
            self.pool_q.add_used(desc);
            used = true;
        }
        if used {
            self.pool_q.trigger_interrupt();
        }
    }

    fn run(&mut self, kill_evt: Event) -> anyhow::Result<()> {
        #[derive(EventToken)]
        enum Token {
            ReqQueue,
            CompQueue,
            PoolQueue,
            HostRequest,
            Kill,
        }

        let wait_ctx = WaitContext::build_with(&[
            (self.req_q.event(), Token::ReqQueue),
            (self.comp_q.event(), Token::CompQueue),
            (self.pool_q.event(), Token::PoolQueue),
            (self.tube.get_read_notifier(), Token::HostRequest),
            (&kill_evt, Token::Kill),
        ])
        .context("failed creating WaitContext")?;

        loop {
            let events = wait_ctx.wait().context("failed polling for events")?;
            for event in events.iter().filter(|e| e.is_readable) {
                match event.token {
                    Token::ReqQueue => {
                        self.req_q.event().wait().context("req queue event")?;
                        self.push_requests();
                    }
                    Token::CompQueue => {
                        self.comp_q.event().wait().context("comp queue event")?;
                        self.take_completions();
                    }
                    Token::PoolQueue => {
                        self.pool_q.event().wait().context("pool queue event")?;
                        self.refuse_pool();
                    }
                    Token::HostRequest => match self.tube.recv::<GvmAcceptRequest>() {
                        Ok(req) => {
                            self.pending.push_back(req);
                            self.push_requests();
                        }
                        Err(e) => {
                            error!("gvm-accept: host link closed: {}", e);
                            return Ok(());
                        }
                    },
                    Token::Kill => return Ok(()),
                }
            }
        }
    }
}

struct GvmAccept {
    worker_thread: Option<WorkerThread<Worker>>,
    tube: Option<Tube>,
    virtio_features: u64,
}

impl VirtioDevice for GvmAccept {
    fn keep_rds(&self) -> Vec<RawDescriptor> {
        Vec::new()
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::VendorDevice(VIRTIO_ID_GUNYAH_ACCEPT)
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        self.virtio_features
    }

    fn activate(
        &mut self,
        _mem: GuestMemory,
        _interrupt: Interrupt,
        mut queues: BTreeMap<usize, Queue>,
    ) -> anyhow::Result<()> {
        if queues.len() != 3 {
            return Err(anyhow!("expected 3 queues, got {}", queues.len()));
        }
        let tube = self
            .tube
            .take()
            .ok_or_else(|| anyhow!("gvm-accept activated twice"))?;
        let req_q = queues.remove(&0).unwrap();
        let comp_q = queues.remove(&1).unwrap();
        let pool_q = queues.remove(&2).unwrap();
        self.worker_thread = Some(WorkerThread::start("v_gvm_accept", move |kill_evt| {
            let mut worker = Worker {
                req_q,
                comp_q,
                pool_q,
                tube,
                pending: VecDeque::new(),
                in_flight: HashMap::new(),
            };
            if let Err(e) = worker.run(kill_evt) {
                error!("gvm-accept worker failed: {:#}", e);
            }
            worker
        }));
        Ok(())
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        if let Some(worker_thread) = self.worker_thread.take() {
            let worker = worker_thread.stop();
            self.tube = Some(worker.tube);
        }
        Ok(())
    }

    fn virtio_snapshot(&mut self) -> anyhow::Result<AnySnapshot> {
        Err(anyhow!("gvm-accept does not support snapshots"))
    }
}

/// The gvm-accept device. Added by crosvm for protected Gunyah VMs when `CROSVM_GVM_ACCEPT=1`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GvmAcceptModule;

impl VirtioDeviceModule for GvmAcceptModule {
    fn sort_name(&self) -> &'static str {
        "gvm_accept"
    }

    fn create(&self, args: &mut VirtioDeviceArgs<'_>) -> anyhow::Result<Box<dyn VirtioDevice>> {
        let (host_end, device_end) = Tube::pair().context("gvm-accept tube")?;
        set_accept_link(host_end);
        Ok(Box::new(GvmAccept {
            worker_thread: None,
            tube: Some(device_end),
            virtio_features: virtio::base_features(args.protection_type),
        }))
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    fn create_jail(&self, _jail_config: Option<&JailConfig>) -> anyhow::Result<Option<Minijail>> {
        // Needs the process-global link in the hypervisor crate: never jailed.
        Ok(None)
    }
}
