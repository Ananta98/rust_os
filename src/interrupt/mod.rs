use core::ptr;
use core::mem;

// These constants MUST have defined with same values as those in src/asm_routines/constants.asm
// They also MUST match the ones in plan.md
// If a constant defined here doesn't exists in that file, then it's also fine
const GDT_SELECTOR_CODE: u16 = 0x08;
const IDT_ADDRESS: usize = 0x0;
const IDTR_ADDRESS: usize = 0x1000;
const IDT_ENTRY_COUNT: usize = 0x100;


#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
struct IDTReference {
    limit: u16,
    offset: u64
}
impl IDTReference {
    pub fn new() -> IDTReference {
        IDTReference {
            limit: ((IDT_ENTRY_COUNT-1)*(mem::size_of::<IDTDescriptor>()))  as u16,
            offset: IDT_ADDRESS as u64
        }
    }
    pub fn write(&self) {
        unsafe {
            ptr::write(IDTR_ADDRESS as *mut Self, *self);
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct IDTDescriptor {
    pointer_low: u16,
    gdt_selector: u16,
    zero: u8,
    options: u8,
    pointer_middle: u16,
    pointer_high: u32,
    reserved: u32
}

impl IDTDescriptor {
    pub fn new(present: bool, pointer: u64, ring: u8) -> IDTDescriptor {
        assert!(ring < 4);
        assert!(present || (pointer == 0 && ring == 0)); // pointer and ring must be 0 if not present
        // example options: present => 1, ring 0 => 00, interrupt gate => 0, interrupt gate => 1110,
        let options: u8 = 0b0_00_0_1110 | (ring << 5) | ((if present {1} else {0}) << 7);

        IDTDescriptor {
            pointer_low: (pointer & 0xffff) as u16,
            gdt_selector: GDT_SELECTOR_CODE,
            zero: 0,
            options: options,
            pointer_middle: ((pointer & 0xffff_0000) >> 16) as u16,
            pointer_high: ((pointer & 0xffff_ffff_0000_0000) >> 32) as u32,
            reserved: 0,
        }
    }
}

pub extern "C" fn exception_de() -> ! {
    panic!("Division by zero.");
}

pub extern "C" fn exception_df() -> ! {
    panic!("Double fault.");
}

pub extern "C" fn exception_gp() -> ! {
    unsafe {
        asm!("mov dword ptr [0xb8000], 0x4f654f31" ::: "memory" : "volatile", "intel");
    }
    panic!("General protection fault.");
}

pub fn init() {
    let mut exception_handlers: [Option<*const fn()>; IDT_ENTRY_COUNT] = [None; IDT_ENTRY_COUNT];

    exception_handlers[0x00] = Some(exception_de as *const fn());
    exception_handlers[0x08] = Some(exception_df as *const fn());
    exception_handlers[0x0d] = Some(exception_gp as *const fn());

    for index in 0...(IDT_ENTRY_COUNT-1) {
        let descriptor = match exception_handlers[index] {
            None            => {IDTDescriptor::new(false, 0, 0)},
            Some(pointer)   => {IDTDescriptor::new(true, pointer as u64, 0)} // TODO: currenly all are ring 0b00
        };
        unsafe {
            ptr::write_volatile((IDT_ADDRESS + index * mem::size_of::<IDTDescriptor>()) as *mut _, descriptor);
        }
    }

    IDTReference::new().write();

    unsafe {
        asm!("lidt [$0]" :: "r"(IDTR_ADDRESS) : "memory" : "volatile", "intel");
    }
}