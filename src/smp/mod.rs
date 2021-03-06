//! Symmetric multiprocessing synchronization

use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use spin::Mutex;
use x86_64::VirtAddr;

use crate::driver::acpi;
use crate::driver::ioapic;
use crate::memory;

pub fn current_processor_id() -> ProcessorId {
    if ioapic::is_enabled() {
        ioapic::apic_processor_id()
    } else {
        ProcessorId(0)
    }
}

/// If current core is BSP
pub fn is_bsp() -> bool {
    if ioapic::is_enabled() {
        ioapic::apic_processor_id().0 == 0
    } else {
        true
    }
}

/// Processor (ACPI) id
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ProcessorId(pub u8);
impl fmt::Display for ProcessorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stores pointers to stacks available for new cores
/// Used by rust_ap_entry to create a new stack
static AP_FREE_STACK: AtomicU64 = AtomicU64::new(0);

/// New stack pointer from `AP_FREE_STACK` and resets it back to zero.
/// # Safety
/// Must not be called without setting AP_FREE_STACK.
#[inline]
pub unsafe fn ap_take_stack() -> u64 {
    let value = AP_FREE_STACK.swap(0, Ordering::SeqCst);
    // Sanity check
    assert!(value != 0, "SMP AP stack not set");
    assert!(value > 0xa000, "SMP AP stack invalid");
    value
}

/// Number of AP cores that have completed initialization
static AP_READY_COUNT: AtomicU64 = AtomicU64::new(0);

/// Must not be executed parallely
unsafe fn start_one(acpi_id: ProcessorId) {
    log::debug!("Waking up core {}", acpi_id);

    assert!(AP_FREE_STACK.load(Ordering::SeqCst) == 0);

    // Set up stack
    let stack = memory::configure(|mem_ctrl| {
        mem_ctrl
            .alloc_stack(5)
            .expect("could not allocate stack for smp core")
    });

    log::debug!("STACKSETUP {:x}", stack.top.as_u64());

    AP_FREE_STACK.store(stack.top.as_u64(), Ordering::SeqCst);

    // Senc init signal
    ioapic::apic_wakeup_processor(acpi_id.0);

    log::trace!("Waiting for core {} to be up", acpi_id);

    // Sleep until the core is online, one second timeout
    let mut is_online = false;
    for _ in 0..50_000 {
        crate::driver::tsc::sleep_ns(200_000);
        if AP_FREE_STACK.load(Ordering::SeqCst) == 0 {
            is_online = true;
            break;
        }
    }
    if !is_online {
        panic!("Failed to bringh core {} online (timeout)", acpi_id);
    }

    log::trace!("Core {} online", acpi_id);
}

/// Called by the AP once it has finished initialization
pub fn ap_mark_ready() {
    AP_READY_COUNT.fetch_add(1, Ordering::SeqCst);
}

pub fn start_all() {
    let acpi_data = acpi::ACPI_DATA.r#try().expect("acpi::init not called");

    // TODO: actually check which CPU is BSP
    // TODO: check for disabled CPUs
    let mut count = 0;
    for cpu in acpi_data.cpus.iter().skip(1) {
        unsafe {
            start_one(ProcessorId(cpu.acpi_id));
        }
        count += 1;
    }

    // Wait for all cores to be ready
    while AP_READY_COUNT.load(Ordering::SeqCst) < count {
        // TODO: timeout
        crate::driver::tsc::sleep_ns(200_000);
    }
    log::info!("All CPU cores ready");
}
