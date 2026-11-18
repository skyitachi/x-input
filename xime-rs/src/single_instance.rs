//! 单实例锁。
//!
//! xime 可能同时被多个入口拉起：systemd 用户单元、KWin（kwinrc InputMethod）、
//! XDG autostart、用户手工执行。输入法席位本身要求独占，但不是所有合成器都会
//! 在第二个绑定者出现时明确拒绝——Hyprland 就只是静默换人，表现为「候选窗
//! 时有时无」。一把会话级 flock 把这一整类问题挡掉：先到先得，后来者立即退出。
//!
//! 锁文件放在 XDG_RUNTIME_DIR（tmpfs，登录会话级，重启即清）。

use std::os::unix::io::RawFd;

pub enum Lock {
    /// 本进程持有锁，fd 刻意不关闭——锁要活到进程结束（关闭即释放）
    #[allow(dead_code)]
    Acquired(RawFd),
    /// 已有实例在跑
    Busy,
    /// 锁文件建不出来（XDG_RUNTIME_DIR 不可写之类）：不能因为锁失败就不当输入法
    Unavailable,
}

/// true = 本进程可以继续当输入法；false = 已有实例，该退出。
pub fn try_lock() -> bool {
    !matches!(acquire(), Lock::Busy)
}

fn lock_path() -> Option<String> {
    let dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    if dir.is_empty() {
        return None;
    }
    Some(format!("{dir}/xime.lock"))
}

pub fn acquire() -> Lock {
    match lock_path() {
        Some(p) => acquire_at(&p),
        None => Lock::Unavailable,
    }
}

fn acquire_at(path: &str) -> Lock {
    let cpath = match std::ffi::CString::new(path) {
        Ok(c) => c,
        Err(_) => return Lock::Unavailable,
    };
    let fd = unsafe {
        libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC, 0o600)
    };
    if fd < 0 {
        return Lock::Unavailable;
    }
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        unsafe { libc::close(fd) };
        return Lock::Busy;
    }
    Lock::Acquired(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("xime-lock-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("xime.lock").to_str().unwrap().to_string()
    }

    #[test]
    fn 第二次加锁会被拒绝() {
        let p = tmp_path("busy");
        match acquire_at(&p) {
            Lock::Acquired(fd) => {
                assert!(matches!(acquire_at(&p), Lock::Busy), "第二个进程应拿不到锁");
                unsafe { libc::close(fd) };
            }
            _ => panic!("空目录下首次加锁应成功"),
        }
        let _ = std::fs::remove_dir_all(std::path::Path::new(&p).parent().unwrap());
    }

    #[test]
    fn 锁随进程关闭而释放() {
        let p = tmp_path("release");
        match acquire_at(&p) {
            Lock::Acquired(fd) => {
                unsafe { libc::close(fd) };
                match acquire_at(&p) {
                    Lock::Acquired(fd2) => unsafe { libc::close(fd2); },
                    _ => panic!("释放后应能重新拿锁"),
                }
            }
            _ => panic!("首次加锁应成功"),
        }
        let _ = std::fs::remove_dir_all(std::path::Path::new(&p).parent().unwrap());
    }

    #[test]
    fn 目录不可写时降级为可用() {
        assert!(matches!(acquire_at("/nonexistent-dir-xyz/xime.lock"), Lock::Unavailable));
    }
}
