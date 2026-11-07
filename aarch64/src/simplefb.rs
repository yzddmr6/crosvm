// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright DroidVM contributors
// Additional permissions apply; see ADDITIONAL-PERMISSIONS in the repository root.
//
// iceland gvm: the simplefb layout and device-tree node, ported from DroidVM dvm-crosvm 849e05f
// (aarch64/src/lib.rs get_simplefb_addr / get_simplefb_size, aarch64/src/fdt.rs
// create_simplefb_node); see CREDITS. Reduced to what this crosvm needs: the framebuffer always
// sits just below the static swiotlb (or at the top of the `--mem` block when there is none), so
// the same layout serves Gunyah protected VMs and everything else.

use arch::SimplefbParams;
use cros_fdt::Fdt;

use crate::AARCH64_PHYS_MEM_START;

const ALIGN_2M: u64 = 2 << 20;

/// Bytes the framebuffer region spans: the data rounded up to 2 MiB.
fn data_size_aligned(sfb: &SimplefbParams) -> u64 {
    sfb.data_size().next_multiple_of(ALIGN_2M)
}

/// Guest physical address of the framebuffer region: below the static swiotlb at the top of the
/// `--mem` block, aligned down to 2 MiB (the swiotlb base keeps its own alignment; any gap between
/// the end of the framebuffer data and the swiotlb belongs to the framebuffer region).
///
/// `static_swiotlb` is the size of the swiotlb carved out of the block (0 when the hypervisor
/// does not need a static swiotlb). Returns None if the framebuffer does not fit.
pub fn simplefb_addr(sfb: &SimplefbParams, memory_size: u64, static_swiotlb: u64) -> Option<u64> {
    let top = memory_size.checked_sub(static_swiotlb)?;
    let addr = top.checked_sub(data_size_aligned(sfb))? & !(ALIGN_2M - 1);
    // Leave at least 64 MiB of guest RAM below the framebuffer.
    if addr < (64 << 20) {
        return None;
    }
    Some(AARCH64_PHYS_MEM_START + addr)
}

/// Size of the framebuffer region: from `simplefb_addr` up to the swiotlb base.
pub fn simplefb_size(sfb: &SimplefbParams, memory_size: u64, static_swiotlb: u64) -> Option<u64> {
    let addr = simplefb_addr(sfb, memory_size, static_swiotlb)?;
    Some(AARCH64_PHYS_MEM_START + memory_size - static_swiotlb - addr)
}

/// What the `simple-framebuffer` node says.
pub struct SimplefbDtConfig {
    pub addr: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: String,
}

/// Creates the `simple-framebuffer` node (Linux binding: compatible, reg, width, height, stride,
/// format) pointing at the no-map reserved-memory region `memory_region_phandle`, through which
/// simpledrm maps it write-combining.
pub fn create_simplefb_node(
    fdt: &mut Fdt,
    cfg: &SimplefbDtConfig,
    memory_region_phandle: u32,
) -> cros_fdt::Result<()> {
    let fb_node = fdt
        .root_mut()
        .subnode_mut(&format!("framebuffer@{:x}", cfg.addr))?;
    fb_node.set_prop("compatible", "simple-framebuffer")?;
    fb_node.set_prop("reg", &[cfg.addr, cfg.size])?;
    fb_node.set_prop("width", cfg.width)?;
    fb_node.set_prop("height", cfg.height)?;
    fb_node.set_prop("stride", cfg.stride)?;
    fb_node.set_prop("format", cfg.format.as_str())?;
    fb_node.set_prop("memory-region", memory_region_phandle)?;
    fb_node.set_prop("status", "okay")?;
    Ok(())
}
