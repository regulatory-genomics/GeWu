//! Process ownership shared by local tools and app adapters.
use std::{io, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt},
    process::{Child, Command},
};

#[cfg(not(target_os = "linux"))]
fn own_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    command.kill_on_drop(true);
}

fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Only called on children whose fresh group we created.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

pub async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    max: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        let lf = chunk.iter().position(|&b| b == b'\n');
        let len = lf.unwrap_or(chunk.len());
        if len > max.saturating_sub(frame.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process frame exceeds size limit",
            ));
        }
        frame.extend_from_slice(&chunk[..len]);
        reader.consume(len + usize::from(lf.is_some()));
        if lf.is_some() {
            return Ok(Some(frame));
        }
    }
}

pub struct ProcessScope {
    child: Child,
    #[cfg(target_os = "linux")]
    session: u32,
    cleaned: bool,
}

impl ProcessScope {
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        #[cfg(not(target_os = "linux"))]
        own_group(&mut command);
        let child = command.spawn()?;
        Ok(Self {
            #[cfg(target_os = "linux")]
            session: child.id().expect("unreaped child"),
            child,
            cleaned: false,
        })
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Validate a request to begin work in a separate group. Linux tracks the entire
    /// owned session, so cleanup still finds its workers after the leader exits.
    pub fn register(&self, pid: u32, pgid: u32) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if pid == pgid
            && pid != self.session
            && let Ok(child) = linux::stat(pid)
            && child.session == self.session
            && child.group == pgid
            && !child.dead
        {
            let mut parent = child.parent;
            while parent > 1 {
                if parent == self.session {
                    return Ok(());
                }
                let Ok(info) = linux::stat(parent) else { break };
                if info.session != self.session || info.parent == parent {
                    break;
                }
                parent = info.parent;
            }
        }
        let _ = (pid, pgid);
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot verify ownership of reported process group",
        ))
    }

    #[cfg(target_os = "linux")]
    fn remaining(&self, force: bool) -> io::Result<bool> {
        linux::remaining(self.session, force)
    }

    /// Wait for cooperative adapter exit without killing attached services.
    pub async fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        self.cleanup(timeout, false).await
    }

    pub async fn stop(&mut self, timeout: Duration) -> io::Result<()> {
        self.cleanup(timeout, true).await
    }

    async fn cleanup(&mut self, timeout: Duration, force: bool) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        if force {
            kill_group(&mut self.child);
        }
        let until = tokio::time::Instant::now() + timeout;
        loop {
            #[cfg(target_os = "linux")]
            let remaining = self.remaining(force)?;
            #[cfg(not(target_os = "linux"))]
            let remaining = false;
            if !remaining && self.child.try_wait()?.is_some() {
                self.cleaned = true;
                return Ok(());
            }
            if tokio::time::Instant::now() >= until {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process scope did not exit",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for ProcessScope {
    fn drop(&mut self) {
        if !self.cleaned {
            #[cfg(target_os = "linux")]
            let _ = self.remaining(true);
            kill_group(&mut self.child);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs, io,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };

    pub(super) struct Stat {
        pub parent: u32,
        pub group: u32,
        pub session: u32,
        pub dead: bool,
        start: u64,
    }

    pub(super) fn stat(pid: u32) -> io::Result<Stat> {
        let contents = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let invalid = || io::Error::from(io::ErrorKind::InvalidData);
        let mut fields = contents
            .rsplit_once(')')
            .ok_or_else(invalid)?
            .1
            .split_whitespace();
        let dead = matches!(fields.next(), Some("Z" | "X"));
        let mut next = || {
            fields
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(invalid)
        };
        let (parent, group, session) = (next()?, next()?, next()?);
        let start = fields
            .nth(15)
            .and_then(|s| s.parse().ok())
            .ok_or_else(invalid)?;
        Ok(Stat {
            parent,
            group,
            session,
            dead,
            start,
        })
    }

    pub(super) fn remaining(session: u32, force: bool) -> io::Result<bool> {
        let mut live = false;
        for entry in fs::read_dir("/proc")? {
            let Ok(entry) = entry else { continue };
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(before) = stat(pid) else { continue };
            if before.session != session || before.dead {
                continue;
            }
            live = true;
            if !force {
                return Ok(true);
            }
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) };
            if fd < 0 {
                // shortcut: older Linux lacks pidfds; verify session and identity
                // immediately before signaling. Prefer pidfds when available.
                if io::Error::last_os_error().raw_os_error() == Some(libc::ENOSYS)
                    && let Ok(after) = stat(pid)
                    && after.session == session
                    && after.start == before.start
                {
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
                continue;
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
            // Pin the target before rechecking identity; never signal a recycled PID.
            if let Ok(after) = stat(pid)
                && after.session == session
                && after.start == before.start
            {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0u32,
                    );
                }
            }
        }
        Ok(live)
    }
}
