use core::num::NonZeroU64;
use core::fmt;
use core::u64;
use serde::{Serialize, Deserialize};
use x86_64::structures::idt::InterruptStackFrameValue;
use x86_64::VirtAddr;
use x86_64::structures::idt::PageFaultErrorCode;

/// ProcessId is stores as `NonZeroU64`, so that `Option<ProcessId>`
/// still has uses only `size_of<Processid>` bytes
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub struct ProcessId(NonZeroU64);
impl ProcessId {
    /// # Safety
    /// Must be called only once
    pub const unsafe fn first() -> Self {
        Self(NonZeroU64::new_unchecked(1))
    }

    /// # Safety
    /// Only to be used by the process scheduler
    pub unsafe fn next(self) -> Self {
        assert_ne!(self.0.get(), u64::MAX, "Kernel process id has no successor");
        Self(NonZeroU64::new(self.0.get() + 1).expect("Overflow"))
    }

    pub const fn as_u64(self) -> u64 {
        self.0.get()
    }
}
impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ProcessResult {
    /// The process exited with a return code
    Completed(u64),
    /// The process was terminated because an error occurred
    Failed(Error),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Error {
    /// Division by zero
    DivideByZero(InterruptStackFrameValue),
    /// Page fault
    PageFault(InterruptStackFrameValue, VirtAddr, PageFaultErrorCode),
    /// Unhandled interrupt without an error code
    Interrupt(u8, InterruptStackFrameValue),
    /// Unhandled interrupt with an error code
    InterruptWithCode(u8, InterruptStackFrameValue, u32),
    /// Invalid system call number
    SyscallNumber(u64),
    /// Invalid pointer passed to system call
    Pointer(VirtAddr),
    /// Owner process died
    ChainedTermination,
}