// Copyright 2026 iceland gvm. Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! iceland gvm: a SIGBUS reporter. vm-run v10b: the Windows 11 guest's crosvm died with SIGBUS after ~9 min
//! with no log line (core dumps disabled). On Gunyah a host access to guest memory that is lent to the
//! protected VM faults with SIGBUS; this prints which thread touched which address, then the process mappings
//! (to place the address in guest RAM, the swiotlb pool or the simplefb region) and re-raises the signal.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

static IN_HANDLER: AtomicBool = AtomicBool::new(false);

fn put(buf: &mut [u8], n: &mut usize, s: &[u8]) {
    for &b in s {
        if *n < buf.len() {
            buf[*n] = b;
            *n += 1;
        }
    }
}

fn put_hex(buf: &mut [u8], n: &mut usize, v: usize) {
    put(buf, n, b"0x");
    let mut started = false;
    for i in (0..(usize::BITS / 4)).rev() {
        let d = (v >> (i * 4)) & 0xf;
        if d != 0 || started || i == 0 {
            started = true;
            put(buf, n, &[b"0123456789abcdef"[d]]);
        }
    }
}

extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // SAFETY: only async-signal-safe calls (prctl, write, open, read, signal, raise) on local buffers.
    unsafe {
        if !IN_HANDLER.swap(true, Ordering::SeqCst) {
            let addr = if info.is_null() { 0 } else { (*info).si_addr() as usize };
            let code = if info.is_null() { 0 } else { (*info).si_code };
            let mut name = [0u8; 17];
            libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr() as libc::c_ulong, 0, 0, 0);
            let len = name.iter().position(|&c| c == 0).unwrap_or(16);
            let mut buf = [0u8; 256];
            let mut n = 0;
            put(&mut buf, &mut n, b"\nGVM-SIGBUS signal ");
            put_hex(&mut buf, &mut n, sig as usize);
            put(&mut buf, &mut n, b" code ");
            put_hex(&mut buf, &mut n, code as usize);
            put(&mut buf, &mut n, b" addr ");
            put_hex(&mut buf, &mut n, addr);
            put(&mut buf, &mut n, b" thread ");
            put(&mut buf, &mut n, &name[..len]);
            put(&mut buf, &mut n, b" tid ");
            put_hex(&mut buf, &mut n, libc::syscall(libc::SYS_gettid) as usize);
            put(&mut buf, &mut n, b"\nGVM-SIGBUS maps:\n");
            libc::write(2, buf.as_ptr() as *const libc::c_void, n);
            let fd = libc::open(b"/proc/self/maps\0".as_ptr() as *const libc::c_char, libc::O_RDONLY);
            if fd >= 0 {
                let mut chunk = [0u8; 4096];
                loop {
                    let r = libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len());
                    if r <= 0 {
                        break;
                    }
                    libc::write(2, chunk.as_ptr() as *const libc::c_void, r as usize);
                }
                libc::close(fd);
            }
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Installs the reporter for SIGBUS and SIGSEGV.
pub fn install() {
    // SAFETY: a plain sigaction with an extern "C" handler; the struct is zero-initialised.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
    }
}
