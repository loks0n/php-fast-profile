use anyhow::{Result, anyhow};
use nix::sys::uio::{RemoteIoVec, process_vm_readv};
use nix::unistd::Pid;
use std::io::IoSliceMut;

#[derive(Clone, Copy)]
pub struct Remote {
    pid: Pid,
}

impl Remote {
    pub fn new(pid: i32) -> Self {
        Self {
            pid: Pid::from_raw(pid),
        }
    }

    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len();
        let mut local = [IoSliceMut::new(buf)];
        let remote = [RemoteIoVec {
            base: addr as usize,
            len,
        }];
        let n = process_vm_readv(self.pid, &mut local, &remote)
            .map_err(|e| anyhow!("process_vm_readv @ {:#x} ({} bytes): {e}", addr, len))?;
        if n != len {
            return Err(anyhow!(
                "short read @ {:#x}: got {} of {} bytes",
                addr,
                n,
                len
            ));
        }
        Ok(())
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64> {
        let mut buf = [0u8; 8];
        self.read(addr, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read(addr, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    /// Bulk-read into a fixed-size buffer.
    pub fn read_array<const N: usize>(&self, addr: u64) -> Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.read(addr, &mut buf)?;
        Ok(buf)
    }

    /// Read a NUL-terminated C string up to `max` bytes.
    pub fn read_cstring(&self, addr: u64, max: usize) -> Result<String> {
        if addr == 0 {
            return Ok(String::new());
        }
        let mut buf = vec![0u8; max];
        self.read(addr, &mut buf)?;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Read raw bytes of known length.
    pub fn read_vec(&self, addr: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.read(addr, &mut buf)?;
        Ok(buf)
    }
}
