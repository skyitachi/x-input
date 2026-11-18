//! wl_shm 缓冲：用 memfd + mmap 提供像素内存，fd 通过 SCM_RIGHTS 交给合成器。
//!
//! 合成器会用自己的 fd 副本映射同一块内存，因此这里 unmap 不影响已经提交的帧。

use std::io;
use std::os::unix::io::RawFd;

pub struct Shm {
    pub fd: RawFd,
    ptr: *mut u8,
    len: usize,
}

impl Shm {
    pub fn new(name: &str, len: usize) -> io::Result<Shm> {
        let cname = std::ffi::CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "非法名字"))?;
        let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(Shm { fd, ptr: p as *mut u8, len })
    }

    pub fn upload_bytes(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.len);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr, n);
        }
    }

    pub fn upload(&mut self, px: &[u32]) {
        let n = px.len().min(self.len / 4);
        unsafe {
            std::ptr::copy_nonoverlapping(px.as_ptr() as *const u8, self.ptr, n * 4);
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
            libc::close(self.fd);
        }
    }
}
