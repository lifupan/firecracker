// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use libc::{c_char, c_int, c_void};
use std::collections::HashMap;
use std::ffi::{CStr, CString, NulError};
use std::fmt::Debug;
use std::ptr::null;
use std::{io, result};

use super::super::DeviceType;
use super::get_fdt_addr;
use super::gic::GICDevice;
use super::layout::FDT_MAX_SIZE;
use aarch64::fdt::Error::CstringFDTTransform;
use memory_model::{GuestAddress, GuestMemory, GuestMemoryError};

// This is a value for uniquely identifying the FDT node declaring the interrupt controller.
const GIC_PHANDLE: u32 = 1;
// This is a value for uniquely identifying the FDT node containing the clock definition.
const CLOCK_PHANDLE: u32 = 2;
// Read the documentation specified when appending the root node to the FDT.
const ADDRESS_CELLS: u32 = 0x2;
const SIZE_CELLS: u32 = 0x2;

// As per kvm tool and
// https://www.kernel.org/doc/Documentation/devicetree/bindings/interrupt-controller/arm%2Cgic.txt
// Look for "The 1st cell..."
const GIC_FDT_IRQ_TYPE_SPI: u32 = 0;
const GIC_FDT_IRQ_TYPE_PPI: u32 = 1;

// From https://elixir.bootlin.com/linux/v4.9.62/source/include/dt-bindings/interrupt-controller/irq.h#L17
const IRQ_TYPE_EDGE_RISING: u32 = 1;
const IRQ_TYPE_LEVEL_HI: u32 = 4;

// This links to libfdt which handles the creation of the binary blob
// flattened device tree (fdt) that is passed to the kernel and indicates
// the hardware configuration of the machine.
extern "C" {
    fn fdt_create(buf: *mut c_void, bufsize: c_int) -> c_int;
    fn fdt_finish_reservemap(fdt: *mut c_void) -> c_int;
    fn fdt_begin_node(fdt: *mut c_void, name: *const c_char) -> c_int;
    fn fdt_property(fdt: *mut c_void, name: *const c_char, val: *const c_void, len: c_int)
        -> c_int;
    fn fdt_end_node(fdt: *mut c_void) -> c_int;
    fn fdt_open_into(fdt: *const c_void, buf: *mut c_void, bufsize: c_int) -> c_int;
    fn fdt_finish(fdt: *const c_void) -> c_int;
    fn fdt_pack(fdt: *mut c_void) -> c_int;
}

/// Trait for devices to be added to the Flattened Device Tree.
pub trait DeviceInfoForFDT {
    /// Returns the address where this device will be loaded.
    fn addr(&self) -> u64;
    /// Returns the associated interrupt for this device.
    fn irq(&self) -> u32;
    /// Returns the amount of memory that needs to be reserved for this device.
    fn length(&self) -> u64;
}

/// Errors thrown while configuring the Flattened Device Tree for aarch64.
#[derive(Debug)]
pub enum Error {
    /// Failed to append node to the FDT.
    AppendFDTNode(io::Error),
    /// Failed to append a property to the FDT.
    AppendFDTProperty(io::Error),
    /// Syscall for creating FDT failed.
    CreateFDT(io::Error),
    /// Failed to obtain a C style string.
    CstringFDTTransform(NulError),
    /// Failure in calling syscall for terminating this FDT.
    FinishFDTReserveMap(io::Error),
    /// FDT was partially written to memory.
    IncompleteFDTMemoryWrite,
    /// Failure in writing FDT in memory.
    WriteFDTToMemory(GuestMemoryError),
}
type Result<T> = result::Result<T, Error>;

/// Creates the flattened device tree for this aarch64 microVM.
pub fn create_fdt<T: DeviceInfoForFDT + Clone + Debug>(
    guest_mem: &GuestMemory,
    vcpu_mpidr: Vec<u64>,
    cmdline: &CStr,
    device_info: Option<&HashMap<(DeviceType, String), T>>,
    gic_device: &Box<dyn GICDevice>,
) -> Result<(Vec<u8>)> {
    // Alocate stuff necessary for the holding the blob.
    let mut fdt = vec![0; FDT_MAX_SIZE];

    allocate_fdt(&mut fdt)?;

    // For an explanation why these nodes were introduced in the blob take a look at
    // https://github.com/torvalds/linux/blob/master/Documentation/devicetree/booting-without-of.txt#L845
    // Look for "Required nodes and properties".

    // Header or the root node as per above mentioned documentation.
    append_begin_node(&mut fdt, "")?;
    append_property_string(&mut fdt, "compatible", "linux,dummy-virt")?;
    // For info on #address-cells and size-cells read "Note about cells and address representation"
    // from the above mentioned txt file.
    append_property_u32(&mut fdt, "#address-cells", ADDRESS_CELLS)?;
    append_property_u32(&mut fdt, "#size-cells", SIZE_CELLS)?;
    // This is not mandatory but we use it to point the root node to the node
    // containing description of the interrupt controller for this VM.
    append_property_u32(&mut fdt, "interrupt-parent", GIC_PHANDLE)?;
    create_cpu_nodes(&mut fdt, &vcpu_mpidr)?;
    create_memory_node(&mut fdt, guest_mem)?;
    create_chosen_node(&mut fdt, cmdline)?;
    create_gic_node(&mut fdt, gic_device)?;
    create_timer_node(&mut fdt)?;
    create_clock_node(&mut fdt)?;
    create_psci_node(&mut fdt)?;
    device_info.map_or(Ok(()), |v| create_devices_node(&mut fdt, v))?;

    // End Header node.
    append_end_node(&mut fdt)?;

    // Allocate another buffer so we can format and then write fdt to guest.
    let mut fdt_final = vec![0; FDT_MAX_SIZE];
    finish_fdt(&mut fdt, &mut fdt_final)?;

    // Write FDT to memory.
    let fdt_address = GuestAddress(get_fdt_addr(&guest_mem));
    let written = guest_mem
        .write_slice_at_addr(fdt_final.as_slice(), fdt_address)
        .map_err(Error::WriteFDTToMemory)?;
    if written < FDT_MAX_SIZE {
        return Err(Error::IncompleteFDTMemoryWrite);
    }
    Ok(fdt_final)
}

// Following are auxiliary functions for allocating and finishing the FDT.
fn allocate_fdt(fdt: &mut Vec<u8>) -> Result<()> {
    // Safe since we allocated this array with FDT_MAX_SIZE.
    let mut fdt_ret = unsafe { fdt_create(fdt.as_mut_ptr() as *mut c_void, FDT_MAX_SIZE as c_int) };

    if fdt_ret != 0 {
        return Err(Error::CreateFDT(io::Error::last_os_error()));
    }

    // The flattened device trees created with fdt_create() contains a list of
    // reserved memory areas. We need to call `fdt_finish_reservemap` so as to make sure that there is a
    // terminator in the reservemap list and whatever happened to be at the
    // start of the FDT data section would end up being interpreted as
    // reservemap entries.
    // Safe since we previously allocated this array.
    fdt_ret = unsafe { fdt_finish_reservemap(fdt.as_mut_ptr() as *mut c_void) };
    if fdt_ret != 0 {
        return Err(Error::FinishFDTReserveMap(io::Error::last_os_error()));
    }
    Ok(())
}

fn finish_fdt(from_fdt: &mut Vec<u8>, to_fdt: &mut Vec<u8>) -> Result<()> {
    // Safe since we allocated `fdt_final` and previously passed in its size.
    let mut fdt_ret = unsafe { fdt_finish(from_fdt.as_mut_ptr() as *mut c_void) };
    if fdt_ret != 0 {
        return Err(Error::FinishFDTReserveMap(io::Error::last_os_error()));
    }

    // Safe because we allocated both arrays with the correct size.
    fdt_ret = unsafe {
        fdt_open_into(
            from_fdt.as_mut_ptr() as *mut c_void,
            to_fdt.as_mut_ptr() as *mut c_void,
            FDT_MAX_SIZE as i32,
        )
    };
    if fdt_ret != 0 {
        return Err(Error::FinishFDTReserveMap(io::Error::last_os_error()));
    }

    // Safe since we allocated `to_fdt`.
    fdt_ret = unsafe { fdt_pack(to_fdt.as_mut_ptr() as *mut c_void) };
    if fdt_ret != 0 {
        return Err(Error::FinishFDTReserveMap(io::Error::last_os_error()));
    }
    Ok(())
}

// Following are auxiliary functions for appending nodes to FDT.
fn append_begin_node(fdt: &mut Vec<u8>, name: &str) -> Result<()> {
    let cstr_name = CString::new(name).map_err(CstringFDTTransform)?;

    // Safe because we allocated fdt and converted name to a CString
    let fdt_ret = unsafe { fdt_begin_node(fdt.as_mut_ptr() as *mut c_void, cstr_name.as_ptr()) };
    if fdt_ret != 0 {
        return Err(Error::AppendFDTNode(io::Error::last_os_error()));
    }
    Ok(())
}

fn append_end_node(fdt: &mut Vec<u8>) -> Result<()> {
    // Safe because we allocated fdt.
    let fdt_ret = unsafe { fdt_end_node(fdt.as_mut_ptr() as *mut c_void) };
    if fdt_ret != 0 {
        return Err(Error::AppendFDTNode(io::Error::last_os_error()));
    }
    Ok(())
}

// Following are auxiliary functions for appending property nodes to the nodes of the FDT.
fn append_property_u32(fdt: &mut Vec<u8>, name: &str, val: u32) -> Result<()> {
    append_property(fdt, name, &to_be32(val))
}

fn append_property_u64(fdt: &mut Vec<u8>, name: &str, val: u64) -> Result<()> {
    append_property(fdt, name, &to_be64(val))
}

fn append_property_string(fdt: &mut Vec<u8>, name: &str, value: &str) -> Result<()> {
    let cstr_value = CString::new(value).map_err(CstringFDTTransform)?;
    append_property_cstring(fdt, name, &cstr_value)
}

fn append_property_cstring(fdt: &mut Vec<u8>, name: &str, cstr_value: &CStr) -> Result<()> {
    let value_bytes = cstr_value.to_bytes_with_nul();
    let cstr_name = CString::new(name).map_err(CstringFDTTransform)?;
    // Safe because we allocated fdt, converted name and value to CStrings
    let fdt_ret = unsafe {
        fdt_property(
            fdt.as_mut_ptr() as *mut c_void,
            cstr_name.as_ptr(),
            value_bytes.as_ptr() as *mut c_void,
            value_bytes.len() as i32,
        )
    };
    if fdt_ret != 0 {
        return Err(Error::AppendFDTProperty(io::Error::last_os_error()));
    }
    Ok(())
}

fn append_property_null(fdt: &mut Vec<u8>, name: &str) -> Result<()> {
    let cstr_name = CString::new(name).map_err(CstringFDTTransform)?;

    // Safe because we allocated fdt, converted name to a CString
    let fdt_ret = unsafe {
        fdt_property(
            fdt.as_mut_ptr() as *mut c_void,
            cstr_name.as_ptr(),
            null(),
            0,
        )
    };
    if fdt_ret != 0 {
        return Err(Error::AppendFDTProperty(io::Error::last_os_error()));
    }
    Ok(())
}

fn append_property(fdt: &mut Vec<u8>, name: &str, val: &[u8]) -> Result<()> {
    let cstr_name = CString::new(name).map_err(CstringFDTTransform)?;
    let val_ptr = val.as_ptr() as *const c_void;

    // Safe because we allocated fdt and converted name to a CString
    let fdt_ret = unsafe {
        fdt_property(
            fdt.as_mut_ptr() as *mut c_void,
            cstr_name.as_ptr(),
            val_ptr,
            val.len() as i32,
        )
    };
    if fdt_ret != 0 {
        return Err(Error::AppendFDTProperty(io::Error::last_os_error()));
    }
    Ok(())
}

// Auxiliary functions for writing u32/u64 numbers in big endian order.
fn to_be32(input: u32) -> [u8; 4] {
    u32::to_be_bytes(input)
}

fn to_be64(input: u64) -> [u8; 8] {
    u64::to_be_bytes(input)
}

// Helper functions for generating a properly formatted byte vector using 32-bit/64-bit cells.
fn generate_prop32(cells: &[u32]) -> Vec<u8> {
    let mut ret: Vec<u8> = Vec::new();
    for &e in cells {
        ret.extend(to_be32(e).iter());
    }
    ret
}

fn generate_prop64(cells: &[u64]) -> Vec<u8> {
    let mut ret: Vec<u8> = Vec::new();
    for &e in cells {
        ret.extend(to_be64(e).iter());
    }
    ret
}

// Following are the auxiliary function for creating the different nodes that we append to our FDT.
fn create_cpu_nodes(fdt: &mut Vec<u8>, vcpu_mpidr: &Vec<u64>) -> Result<()> {
    // See https://github.com/torvalds/linux/blob/master/Documentation/devicetree/bindings/arm/cpus.yaml.
    append_begin_node(fdt, "cpus")?;
    // As per documentation, on ARM v8 64-bit systems value should be set to 2.
    append_property_u32(fdt, "#address-cells", 0x02)?;
    append_property_u32(fdt, "#size-cells", 0x0)?;
    let num_cpus = vcpu_mpidr.len();

    for cpu_index in 0..num_cpus {
        let cpu_name = format!("cpu@{:x}", cpu_index);
        append_begin_node(fdt, &cpu_name)?;
        append_property_string(fdt, "device_type", "cpu")?;
        append_property_string(fdt, "compatible", "arm,arm-v8")?;
        if num_cpus > 1 {
            // This is required on armv8 64-bit. See aforementioned documentation.
            append_property_string(fdt, "enable-method", "psci")?;
        }
        // Set the field to first 24 bits of the MPIDR - Multiprocessor Affinity Register.
        // See http://infocenter.arm.com/help/index.jsp?topic=/com.arm.doc.ddi0488c/BABHBJCI.html.
        append_property_u64(fdt, "reg", vcpu_mpidr[cpu_index] & 0x7FFFFF)?;
        append_end_node(fdt)?;
    }
    append_end_node(fdt)?;
    Ok(())
}

fn create_memory_node(fdt: &mut Vec<u8>, guest_mem: &GuestMemory) -> Result<()> {
    let mem_size = guest_mem.end_addr().offset() - super::layout::DRAM_MEM_START;
    // See https://github.com/torvalds/linux/blob/master/Documentation/devicetree/booting-without-of.txt#L960
    // for an explanation of this.
    let mem_reg_prop = generate_prop64(&[super::layout::DRAM_MEM_START as u64, mem_size as u64]);

    append_begin_node(fdt, "memory")?;
    append_property_string(fdt, "device_type", "memory")?;
    append_property(fdt, "reg", &mem_reg_prop)?;
    append_end_node(fdt)?;
    Ok(())
}

fn create_chosen_node(fdt: &mut Vec<u8>, cmdline: &CStr) -> Result<()> {
    append_begin_node(fdt, "chosen")?;
    append_property_cstring(fdt, "bootargs", cmdline)?;
    append_end_node(fdt)?;

    Ok(())
}

fn create_gic_node(fdt: &mut Vec<u8>, gic_device: &Box<dyn GICDevice>) -> Result<()> {
    let gic_reg_prop = generate_prop64(gic_device.device_properties());

    append_begin_node(fdt, "intc")?;
    append_property_string(fdt, "compatible", gic_device.fdt_compatibility())?;
    append_property_null(fdt, "interrupt-controller")?;
    // "interrupt-cells" field specifies the number of cells needed to encode an
    // interrupt source. The type shall be a <u32> and the value shall be 3 if no PPI affinity description
    // is required.
    append_property_u32(fdt, "#interrupt-cells", 3)?;
    append_property(fdt, "reg", &gic_reg_prop)?;
    append_property_u32(fdt, "phandle", GIC_PHANDLE)?;
    append_property_u32(fdt, "#address-cells", 2)?;
    append_property_u32(fdt, "#size-cells", 2)?;
    append_property_null(fdt, "ranges")?;
    let gic_intr = [
        GIC_FDT_IRQ_TYPE_PPI,
        gic_device.fdt_maint_irq(),
        IRQ_TYPE_LEVEL_HI,
    ];
    let gic_intr_prop = generate_prop32(&gic_intr);

    append_property(fdt, "interrupts", &gic_intr_prop)?;
    append_end_node(fdt)?;

    Ok(())
}

fn create_clock_node(fdt: &mut Vec<u8>) -> Result<()> {
    // The Advanced Peripheral Bus (APB) is part of the Advanced Microcontroller Bus Architecture
    // (AMBA) protocol family. It defines a low-cost interface that is optimized for minimal power
    // consumption and reduced interface complexity.
    // PCLK is the clock source and this node defines exactly the clock for the APB.
    append_begin_node(fdt, "apb-pclk")?;
    append_property_string(fdt, "compatible", "fixed-clock")?;
    append_property_u32(fdt, "#clock-cells", 0x0)?;
    append_property_u32(fdt, "clock-frequency", 24000000)?;
    append_property_string(fdt, "clock-output-names", "clk24mhz")?;
    append_property_u32(fdt, "phandle", CLOCK_PHANDLE)?;
    append_end_node(fdt)?;

    Ok(())
}

fn create_timer_node(fdt: &mut Vec<u8>) -> Result<()> {
    // See
    // https://github.com/torvalds/linux/blob/master/Documentation/devicetree/bindings/interrupt-controller/arch_timer.txt
    // These are fixed interrupt numbers for the timer device.
    let irqs = [13, 14, 11, 10];
    let compatible = "arm,armv8-timer";

    let mut timer_reg_cells: Vec<u32> = Vec::new();
    for &irq in irqs.iter() {
        timer_reg_cells.push(GIC_FDT_IRQ_TYPE_PPI);
        timer_reg_cells.push(irq);
        timer_reg_cells.push(IRQ_TYPE_LEVEL_HI);
    }
    let timer_reg_prop = generate_prop32(timer_reg_cells.as_slice());

    append_begin_node(fdt, "timer")?;
    append_property_string(fdt, "compatible", compatible)?;
    append_property_null(fdt, "always-on")?;
    append_property(fdt, "interrupts", &timer_reg_prop)?;
    append_end_node(fdt)?;

    Ok(())
}

fn create_psci_node(fdt: &mut Vec<u8>) -> Result<()> {
    let compatible = "arm,psci-0.2";
    append_begin_node(fdt, "psci")?;
    append_property_string(fdt, "compatible", compatible)?;
    // Two methods available: hvc and smc.
    // As per documentation, PSCI calls between a guest and hypervisor may use the HVC conduit instead of SMC.
    // So, since we are using kvm, we need to use hvc.
    append_property_string(fdt, "method", "hvc")?;
    append_end_node(fdt)?;

    Ok(())
}

fn create_virtio_node<T: DeviceInfoForFDT + Clone + Debug>(
    fdt: &mut Vec<u8>,
    dev_info: &T,
) -> Result<()> {
    let device_reg_prop = generate_prop64(&[dev_info.addr(), dev_info.length()]);
    let irq = generate_prop32(&[GIC_FDT_IRQ_TYPE_SPI, dev_info.irq(), IRQ_TYPE_EDGE_RISING]);

    append_begin_node(fdt, &format!("virtio_mmio@{:x}", dev_info.addr()))?;
    append_property_string(fdt, "compatible", "virtio,mmio")?;
    append_property(fdt, "reg", &device_reg_prop)?;
    append_property(fdt, "interrupts", &irq)?;
    append_property_u32(fdt, "interrupt-parent", GIC_PHANDLE)?;
    append_end_node(fdt)?;

    Ok(())
}

fn create_serial_node<T: DeviceInfoForFDT + Clone + Debug>(
    fdt: &mut Vec<u8>,
    dev_info: &T,
) -> Result<()> {
    let serial_reg_prop = generate_prop64(&[dev_info.addr(), dev_info.length()]);
    let irq = generate_prop32(&[GIC_FDT_IRQ_TYPE_SPI, dev_info.irq(), IRQ_TYPE_EDGE_RISING]);

    append_begin_node(fdt, &format!("uart@{:x}", dev_info.addr()))?;
    append_property_string(fdt, "compatible", "ns16550a")?;
    append_property(fdt, "reg", &serial_reg_prop)?;
    append_property_u32(fdt, "clocks", CLOCK_PHANDLE)?;
    append_property_string(fdt, "clock-names", "apb_pclk")?;
    append_property(fdt, "interrupts", &irq)?;
    append_end_node(fdt)?;

    Ok(())
}

fn create_rtc_node<T: DeviceInfoForFDT + Clone + Debug>(
    fdt: &mut Vec<u8>,
    dev_info: &T,
) -> Result<()> {
    let compatible = b"arm,pl031\0arm,primecell\0";
    let rtc_reg_prop = generate_prop64(&[dev_info.addr(), dev_info.length()]);
    let irq = generate_prop32(&[GIC_FDT_IRQ_TYPE_SPI, dev_info.irq(), IRQ_TYPE_LEVEL_HI]);
    append_begin_node(fdt, &format!("rtc@{:x}", dev_info.addr()))?;
    append_property(fdt, "compatible", compatible)?;
    append_property(fdt, "reg", &rtc_reg_prop)?;
    append_property(fdt, "interrupts", &irq)?;
    append_property_u32(fdt, "clocks", CLOCK_PHANDLE)?;
    append_property_string(fdt, "clock-names", "apb_pclk")?;
    append_end_node(fdt)?;

    Ok(())
}

fn create_devices_node<T: DeviceInfoForFDT + Clone + Debug>(
    fdt: &mut Vec<u8>,
    dev_info: &HashMap<(DeviceType, String), T>,
) -> Result<()> {
    // Create one temp Vec to store all virtio devices
    let mut ordered_virtio_device: Vec<&T> = Vec::new();

    for ((device_type, _device_id), info) in dev_info {
        match device_type {
            DeviceType::RTC => create_rtc_node(fdt, info)?,
            DeviceType::Serial => create_serial_node(fdt, info)?,
            DeviceType::Virtio(_) => {
                ordered_virtio_device.push(info);
            }
        }
    }

    // Sort out virtio devices by address from low to high and insert them into fdt table.
    ordered_virtio_device.sort_by(|a, b| a.addr().cmp(&b.addr()));
    for ordered_device_info in ordered_virtio_device.drain(..) {
        create_virtio_node(fdt, ordered_device_info)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aarch64::gic::create_gic;
    use aarch64::{arch_memory_regions, layout};
    use kvm_ioctls::Kvm;

    const LEN: u64 = 4096;

    #[derive(Clone, Debug)]
    pub struct MMIODeviceInfo {
        addr: u64,
        irq: u32,
    }

    impl DeviceInfoForFDT for MMIODeviceInfo {
        fn addr(&self) -> u64 {
            self.addr
        }
        fn irq(&self) -> u32 {
            self.irq
        }
        fn length(&self) -> u64 {
            LEN
        }
    }
    // The `load` function from the `device_tree` will mistakenly check the actual size
    // of the buffer with the allocated size. This works around that.
    fn set_size(buf: &mut [u8], pos: usize, val: usize) {
        buf[pos] = ((val >> 24) & 0xff) as u8;
        buf[pos + 1] = ((val >> 16) & 0xff) as u8;
        buf[pos + 2] = ((val >> 8) & 0xff) as u8;
        buf[pos + 3] = (val & 0xff) as u8;
    }

    #[test]
    fn test_create_fdt_with_devices() {
        let regions = arch_memory_regions(layout::FDT_MAX_SIZE + 0x1000);
        let mem = GuestMemory::new(&regions).expect("Cannot initialize memory");

        let dev_info: HashMap<(DeviceType, std::string::String), MMIODeviceInfo> = [
            (
                (DeviceType::Serial, DeviceType::Serial.to_string()),
                MMIODeviceInfo { addr: 0x00, irq: 1 },
            ),
            (
                (DeviceType::Virtio(1), "virtio".to_string()),
                MMIODeviceInfo {
                    addr: 0x00 + LEN,
                    irq: 2,
                },
            ),
            (
                (DeviceType::RTC, "rtc".to_string()),
                MMIODeviceInfo {
                    addr: 0x00 + 2 * LEN,
                    irq: 3,
                },
            ),
        ]
        .iter()
        .cloned()
        .collect();
        let kvm = Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        let gic = create_gic(&vm, 1).unwrap();
        assert!(create_fdt(
            &mem,
            vec![0],
            &CString::new("console=tty0").unwrap(),
            Some(&dev_info),
            &gic,
        )
        .is_ok())
    }

    #[test]
    fn test_create_fdt() {
        let regions = arch_memory_regions(layout::FDT_MAX_SIZE + 0x1000);
        let mem = GuestMemory::new(&regions).expect("Cannot initialize memory");
        let kvm = Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        let gic = create_gic(&vm, 1).unwrap();
        let mut dtb = create_fdt(
            &mem,
            vec![0],
            &CString::new("console=tty0").unwrap(),
            None::<&std::collections::HashMap<(DeviceType, std::string::String), MMIODeviceInfo>>,
            &gic,
        )
        .unwrap();

        /* Use this code when wanting to generate a new DTB sample.
        {
            use std::fs;
            use std::io::Write;
            use std::path::PathBuf;
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(path.join("src/aarch64/output.dtb"))
                .unwrap();
            output.write_all(&dtb).unwrap();
        }
        */

        let bytes = include_bytes!("output.dtb");
        let pos = 4;
        let val = layout::FDT_MAX_SIZE;
        let mut buf = vec![];
        buf.extend_from_slice(bytes);

        set_size(&mut buf, pos, val);
        set_size(&mut dtb, pos, val);
        let original_fdt = device_tree::DeviceTree::load(&buf).unwrap();
        let generated_fdt = device_tree::DeviceTree::load(&dtb).unwrap();
        assert!(format!("{:?}", original_fdt) == format!("{:?}", generated_fdt));
    }
}
