use crate::{errors::IOResult, mips32::syscall, BasicKernelInterface, FileDescriptor};

/// Concrete implementation of the [BasicKernelInterface] trait for the `MIPS32rel1` target
/// architecture. Exposes a safe interface for performing IO operations within the kernel.
#[derive(Debug)]
pub(crate) struct Mips32IO;

/// Relevant system call numbers for the `MIPS32rel1` target architecture.
///
/// See [Cannon System Call Specification](https://specs.optimism.io/experimental/fault-proof/cannon-fault-proof-vm.html#syscalls)
///
/// **Note**: This is not an exhaustive list of system calls available to the `client` program,
/// only the ones necessary for the [BasicKernelInterface] trait implementation. If an extension
/// trait for the [BasicKernelInterface] trait is created for the `Cannon` kernel, this list should
/// be extended accordingly.
#[repr(usize)]
pub(crate) enum SyscallNumber {
    /// Sets the Exited and ExitCode states to true and $a0 respectively.
    Exit = 4246,
    /// Similar behavior as Linux/MIPS with support for unaligned reads.
    Read = 4003,
    /// Similar behavior as Linux/MIPS with support for unaligned writes.
    Write = 4004,
}

impl BasicKernelInterface for Mips32IO {
    fn write(fd: FileDescriptor, buf: &[u8]) -> IOResult<usize> {
        unsafe {
            crate::linux::from_ret(syscall::syscall3(
                SyscallNumber::Write as usize,
                fd.into(),
                buf.as_ptr() as usize,
                buf.len(),
            ))
        }
    }

    fn read(fd: FileDescriptor, buf: &mut [u8]) -> IOResult<usize> {
        unsafe {
            crate::linux::from_ret(syscall::syscall3(
                SyscallNumber::Read as usize,
                fd.into(),
                buf.as_ptr() as usize,
                buf.len(),
            ))
        }
    }

    fn exit(code: usize) -> ! {
        unsafe {
            let _ = syscall::syscall1(SyscallNumber::Exit as usize, code);
            panic!()
        }
    }
}
