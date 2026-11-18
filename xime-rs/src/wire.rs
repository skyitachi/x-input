//! 最小 Wayland 客户端 wire 协议实现。
//!
//! 为什么不用 wayland-rs：`wayland-backend` 0.3 只剩 system 后端（dlopen 或直接链接
//! `libwayland-client.so`），纯 Rust 后端已被移除。既然目标是"单个静态二进制、零动态依赖"，
//! 就只能自己实现这套线协议——协议本身很小：Unix socket + 定长头 + 按类型序列化参数。
//!
//! 消息格式（小端）：
//! ```text
//! u32 object_id
//! u32 (总长度 << 16) | opcode
//! 参数区：int/uint 4 字节；string = u32(长度含 NUL) + 字节 + NUL + 补齐；array 同理但无 NUL
//! fd 不占消息体，走 SCM_RIGHTS 附属数据，按出现顺序与消息里的 fd 参数一一对应
//! ```

use std::collections::VecDeque;
use std::io;
use std::os::unix::io::RawFd;

/// wl_display 的固定对象 ID
pub const DISPLAY_ID: u32 = 1;

// ---------------------------------------------------------------- 编码

pub struct Msg {
    buf: Vec<u8>,
    fds: Vec<RawFd>,
}

impl Msg {
    pub fn new() -> Self {
        Msg { buf: Vec::with_capacity(64), fds: Vec::new() }
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn string(&mut self, s: &str) -> &mut Self {
        let b = s.as_bytes();
        self.u32(b.len() as u32 + 1);
        self.buf.extend_from_slice(b);
        self.buf.push(0);
        self.pad();
        self
    }

    pub fn new_id(&mut self, id: u32) -> &mut Self {
        self.u32(id)
    }

    /// 记录一个要随消息发送的 fd（不写入消息体）
    pub fn fd(&mut self, fd: RawFd) -> &mut Self {
        self.fds.push(fd);
        self
    }

    fn pad(&mut self) {
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
    }
}

// ---------------------------------------------------------------- 解码

pub struct Args {
    data: Vec<u8>,
    pos: usize,
}

impl Args {
    pub fn u32(&mut self) -> u32 {
        if self.pos + 4 > self.data.len() {
            return 0;
        }
        let v = u32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        v
    }

    pub fn i32(&mut self) -> i32 {
        self.u32() as i32
    }

    pub fn string(&mut self) -> Option<String> {
        let len = self.u32() as usize;
        if len == 0 {
            return None;
        }
        let end = (self.pos + len).min(self.data.len());
        let raw = &self.data[self.pos..end];
        self.pos += len;
        self.pos = (self.pos + 3) & !3; // 跳过补齐
        let s = raw.split(|&c| c == 0).next().unwrap_or(&[]);
        Some(String::from_utf8_lossy(s).into_owned())
    }

}

pub struct Incoming {
    pub obj: u32,
    pub opcode: u16,
    pub args: Args,
}

// ---------------------------------------------------------------- 连接

pub struct Conn {
    fd: RawFd,
    next_id: u32,
    rbuf: Vec<u8>,
    rfds: VecDeque<RawFd>,
}

impl Conn {
    pub fn connect() -> io::Result<Conn> {
        // 合成器直接拉起输入法时（KWin 的「虚拟键盘」就是这条路），它不让你自己
        // 去连 socket，而是把一个**已经连好的** fd 通过 WAYLAND_SOCKET 传进来。
        // 只有这条连接才被允许绑定 zwp_input_method_v2；自己按 WAYLAND_DISPLAY
        // 另开一条，拿到的是普通客户端身份，绑定时会被合成器拒掉。
        if let Some(fd) = inherited_socket() {
            return Ok(Conn::from_fd(fd));
        }

        let path = socket_path()?;
        let cpath = std::ffi::CString::new(path.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径含 NUL"))?;

        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as u16;
        let bytes = cpath.as_bytes();
        if bytes.len() >= addr.sun_path.len() {
            unsafe { libc::close(fd) };
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket 路径过长"));
        }
        for (i, b) in bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }

        let r = unsafe {
            libc::connect(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(io::Error::new(
                e.kind(),
                format!("无法连接 Wayland socket {path}: {e}"),
            ));
        }

        Ok(Conn::from_fd(fd))
    }

    /// 接管一个已连接的 fd：补上 CLOEXEC 与非阻塞，语义与 libwayland 一致。
    /// 非阻塞是必须的——事件循环靠 `wait()` 的 poll 决定何时读，读到 EAGAIN 也要能退出来。
    fn from_fd(fd: RawFd) -> Conn {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
            let fl = libc::fcntl(fd, libc::F_GETFL);
            if fl >= 0 {
                libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
            }
        }
        Conn { fd, next_id: 2, rbuf: Vec::with_capacity(4096), rfds: VecDeque::new() }
    }

    /// 分配一个新对象 ID。Wayland 允许客户端单调递增使用 32 位 ID 空间，
    /// 不回收 delete_id 也不会溢出（输入法一生创建的对象不过十几个）。
    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn send(&mut self, obj: u32, opcode: u16, m: &Msg) -> io::Result<()> {
        let size = 8 + m.buf.len();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&obj.to_le_bytes());
        out.extend_from_slice(&(((size as u32) << 16) | opcode as u32).to_le_bytes());
        out.extend_from_slice(&m.buf);
        sendmsg_fds(self.fd, &out, &m.fds)
    }

    /// 等待可读。返回是否真的有数据。
    pub fn wait(&mut self, timeout_ms: i32) -> io::Result<bool> {
        let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e);
        }
        if r == 0 {
            return Ok(false);
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "合成器关闭了连接"));
        }
        Ok(true)
    }

    /// 把内核里可读的数据全部吸进缓冲（含附属 fd）
    pub fn fill(&mut self) -> io::Result<()> {
        loop {
            let mut buf = [0u8; 8192];
            let mut iov =
                libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
            let mut cbuf = [0u8; 512];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cbuf.len() as _;

            let n = unsafe { libc::recvmsg(self.fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock => return Ok(()),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(e),
                }
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, "合成器断开连接"));
            }

            // 收集附属 fd：顺序必须保持，因为消息里的 fd 参数按出现顺序消耗
            unsafe {
                let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
                while !cmsg.is_null() {
                    if (*cmsg).cmsg_level == libc::SOL_SOCKET
                        && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                    {
                        let hdr = libc::CMSG_LEN(0) as usize;
                        let nbytes = (*cmsg).cmsg_len as usize - hdr;
                        let p = libc::CMSG_DATA(cmsg) as *const RawFd;
                        for i in 0..nbytes / 4 {
                            self.rfds.push_back(*p.add(i));
                        }
                    }
                    cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
                }
            }

            self.rbuf.extend_from_slice(&buf[..n as usize]);
            if (n as usize) < buf.len() {
                return Ok(());
            }
        }
    }

    /// 取出下一条完整消息；不足一条时返回 None
    pub fn next_msg(&mut self) -> Option<Incoming> {
        if self.rbuf.len() < 8 {
            return None;
        }
        let obj = u32::from_le_bytes(self.rbuf[0..4].try_into().unwrap());
        let sz_op = u32::from_le_bytes(self.rbuf[4..8].try_into().unwrap());
        let size = (sz_op >> 16) as usize;
        let opcode = (sz_op & 0xffff) as u16;

        if size < 8 || size % 4 != 0 {
            // 线格式已经错乱，继续解析只会更糟
            self.rbuf.clear();
            return None;
        }
        if self.rbuf.len() < size {
            return None;
        }

        let body: Vec<u8> = self.rbuf[8..size].to_vec();
        self.rbuf.drain(..size);

        Some(Incoming { obj, opcode, args: Args { data: body, pos: 0 } })
    }

    /// 取出排队的下一个 fd。
    ///
    /// fd 在 Wayland 里是跟着消息流顺序消耗的，不能按"每条消息分一批"处理：
    /// 一批读进来多条消息时，只有真正带 fd 参数的那条才应该取走它。本程序里
    /// 事件带 fd 的只有 `grab.keymap`，所以由处理该事件的地方显式取用。
    pub fn take_fd(&mut self) -> RawFd {
        self.rfds.pop_front().unwrap_or(-1)
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
        for fd in self.rfds.drain(..) {
            unsafe { libc::close(fd) };
        }
    }
}

// ---------------------------------------------------------------- 底层 IO

/// 合成器是否通过 `WAYLAND_SOCKET` 把连接直接交了过来。
///
/// 判断依据只有环境变量（fd 号本身在解析时才用到），供 `--check` 说明当前是
/// 「合成器拉起」还是「自己连」。
pub fn inherited_socket_str() -> Option<String> {
    std::env::var("WAYLAND_SOCKET").ok().filter(|s| !s.trim().is_empty())
}

/// 取出 WAYLAND_SOCKET 里的 fd。不是合法 fd 就当没设（退回按 WAYLAND_DISPLAY 连），
/// 免得一个脏环境变量让程序连错误信息都给不出来。
fn inherited_socket() -> Option<RawFd> {
    let v = inherited_socket_str()?;
    let fd: RawFd = v.trim().parse().ok()?;
    check_fd(fd)
}

/// fd 还活着才算数：环境变量可能是陈旧或伪造的（比如从父 shell 继承下来）。
fn check_fd(fd: RawFd) -> Option<RawFd> {
    if fd < 0 {
        return None;
    }
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return None;
    }
    Some(fd)
}

fn socket_path() -> io::Result<String> {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    if display.starts_with('/') {
        return Ok(display);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR 未设置"))?;
    Ok(format!("{dir}/{display}"))
}

fn sendmsg_fds(fd: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov =
        libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;

    let mut cbuf = [0u8; 512];
    if !fds.is_empty() {
        let space = unsafe { libc::CMSG_SPACE((fds.len() * 4) as libc::c_uint) } as usize;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as libc::c_uint) as _;
            let p = libc::CMSG_DATA(cmsg) as *mut RawFd;
            for (i, f) in fds.iter().enumerate() {
                *p.add(i) = *f;
            }
        }
    }

    loop {
        let n = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::Interrupted => continue,
            // 非阻塞 socket 写满：Wayland 消息都很小，短暂自旋即可
            io::ErrorKind::WouldBlock => {
                std::thread::yield_now();
                continue;
            }
            _ => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socketpair() -> (RawFd, RawFd) {
        let mut sv = [0 as RawFd; 2];
        let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) };
        assert_eq!(r, 0, "socketpair 失败");
        (sv[0], sv[1])
    }

    /// 合成器交过来的 fd 要补上 CLOEXEC（避免后续 fork/exec 时泄漏给子进程），
    /// 且必须是非阻塞，否则事件循环会卡在 read 上。
    #[test]
    fn from_fd_sets_cloexec_and_nonblock() {
        let (a, b) = socketpair();
        let c = Conn::from_fd(a);
        let fd_flags = unsafe { libc::fcntl(a, libc::F_GETFD) };
        let st_flags = unsafe { libc::fcntl(a, libc::F_GETFL) };
        assert_ne!(fd_flags & libc::FD_CLOEXEC, 0, "应设置 FD_CLOEXEC");
        assert_ne!(st_flags & libc::O_NONBLOCK, 0, "应设为非阻塞");
        drop(c); // 顺带关掉 a
        unsafe { libc::close(b) };
    }

    /// 陈旧/伪造的 fd 号不能被当成有效连接
    #[test]
    fn check_fd_rejects_bad_fd() {
        assert!(check_fd(-1).is_none());
        let (a, b) = socketpair();
        unsafe {
            libc::close(a);
            libc::close(b);
        }
        assert!(check_fd(a).is_none(), "已关闭的 fd 应被判无效");
    }

    #[test]
    fn check_fd_accepts_live_fd() {
        let (a, b) = socketpair();
        assert_eq!(check_fd(a), Some(a));
        unsafe {
            libc::close(a);
            libc::close(b);
        }
    }
}
