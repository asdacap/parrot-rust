//! A scoped snapshot of descendants, captured while the direct child is unreaped.

use std::io;

#[derive(Debug)]
pub(super) struct OwnedDescendants {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    targets: Vec<platform::Target>,
}

impl OwnedDescendants {
    /// Discovery errors do not discard descendants that were already captured.
    pub(super) fn capture(root: u32) -> (Self, io::Result<()>) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let (targets, result) = platform::capture(root);
            (Self { targets }, result)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = root;
            (Self {}, Err(unsupported()))
        }
    }

    /// Attempt every target even if an earlier signal fails. This can be retried.
    pub(super) fn terminate(&self) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut result = Ok(());
            for target in &self.targets {
                remember(&mut result, target.terminate());
            }
            result
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err(unsupported())
    }

    /// Call after terminating the direct child/group as well. Failed signals are
    /// reported, rather than waiting indefinitely for an unsignaled process.
    pub(super) fn wait(&self) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut result = Ok(());
            for target in &self.targets {
                remember(&mut result, target.wait());
            }
            result
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err(unsupported())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "descendant cleanup is unsupported",
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remember(result: &mut io::Result<()>, next: io::Result<()>) {
    if result.is_ok() {
        *result = next;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn vanished(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

#[cfg(any(target_os = "macos", test))]
fn child_pid_slice(buffer: &[libc::pid_t], count: usize) -> io::Result<&[libc::pid_t]> {
    buffer
        .get(..count)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid child PID count"))
}

#[cfg(test)]
mod child_pid_slice_tests {
    use super::child_pid_slice;

    #[test]
    fn returned_count_is_a_pid_count_not_bytes() {
        let buffer = [11, 22, 33, 44, 55, 66, 77, 88];
        assert_eq!(child_pid_slice(&buffer, 1).unwrap(), &[11]);
        assert_eq!(child_pid_slice(&buffer, 4).unwrap(), &[11, 22, 33, 44]);
        let full = child_pid_slice(&buffer, buffer.len()).unwrap();
        assert_eq!(full, &buffer);
        assert_eq!(full.len(), buffer.len());
    }

    #[test]
    fn returned_count_cannot_exceed_capacity() {
        let buffer = [11, 22, 33, 44];
        assert_eq!(
            child_pid_slice(&buffer, buffer.len() + 1)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData,
        );
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{remember, vanished};
    use std::collections::HashSet;
    use std::fs;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Identity {
        pid: u32,
        parent: u32,
        birth: u64,
    }

    impl Identity {
        fn same_process(self, other: Self) -> bool {
            self.pid == other.pid && self.birth == other.birth
        }
    }

    #[derive(Debug)]
    pub(super) struct Target {
        descriptor: OwnedFd,
        signaled: AtomicBool,
    }

    impl Target {
        pub(super) fn terminate(&self) -> io::Result<()> {
            loop {
                // SAFETY: the owned pidfd binds this signal to the captured process.
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.descriptor.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0_u32,
                    )
                };
                if result == 0 {
                    self.signaled.store(true, Ordering::Relaxed);
                    return Ok(());
                }
                let error = io::Error::last_os_error();
                if vanished(&error) {
                    self.signaled.store(true, Ordering::Relaxed);
                    return Ok(());
                }
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }

        pub(super) fn wait(&self) -> io::Result<()> {
            if !self.signaled.load(Ordering::Relaxed) {
                return Err(io::Error::other("descendant was not successfully signaled"));
            }
            let mut poll = libc::pollfd {
                fd: self.descriptor.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            loop {
                // SAFETY: poll points to one initialized pollfd holding an owned fd.
                if unsafe { libc::poll(&mut poll, 1, -1) } < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if poll.revents & libc::POLLNVAL != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                }
                if poll.revents & libc::POLLERR != 0 {
                    return Err(io::Error::other("descendant pidfd poll failed"));
                }
                if poll.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                    return Ok(());
                }
            }
        }
    }

    pub(super) fn capture(root: u32) -> (Vec<Target>, io::Result<()>) {
        let mut targets = Vec::new();
        let root = match identity(root) {
            Ok(Some(root)) => root,
            Ok(None) => return (targets, Ok(())),
            Err(error) => return (targets, Err(error)),
        };
        let mut pending = vec![root];
        let mut seen = HashSet::from([(root.pid, root.birth)]);
        let mut result = Ok(());
        while let Some(parent) = pending.pop() {
            let (children, discovery) = children(parent.pid);
            remember(&mut result, discovery);
            for pid in children {
                match capture_child(parent, pid) {
                    Ok(Some((child, target))) => {
                        if seen.insert((child.pid, child.birth)) {
                            pending.push(child);
                            targets.push(target);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => remember(&mut result, Err(error)),
                }
            }
        }
        (targets, result)
    }

    fn capture_child(parent: Identity, pid: u32) -> io::Result<Option<(Identity, Target)>> {
        if !identity(parent.pid)?.is_some_and(|now| parent.same_process(now)) {
            return Ok(None);
        }
        let Some(before) = identity(pid)? else {
            return Ok(None);
        };
        if before.parent != parent.pid {
            return Ok(None);
        }
        // SAFETY: pidfd_open has no pointer arguments; ownership is taken below.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if vanished(&error) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: a successful pidfd_open returned a new descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let Some(after) = identity(pid)? else {
            return Ok(None);
        };
        if !child_matches(parent, before, after)
            || !identity(parent.pid)?.is_some_and(|now| parent.same_process(now))
        {
            return Ok(None);
        }
        Ok(Some((
            after,
            Target {
                descriptor,
                signaled: AtomicBool::new(false),
            },
        )))
    }

    fn child_matches(parent: Identity, before: Identity, after: Identity) -> bool {
        before.same_process(after) && before.parent == parent.pid && after.parent == parent.pid
    }

    fn identity(pid: u32) -> io::Result<Option<Identity>> {
        match fs::read(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let value = parse_stat(&stat)?;
                if value.pid != pid {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "process stat PID mismatch",
                    ));
                }
                Ok(Some(value))
            }
            Err(error) if vanished(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn parse_stat(stat: &[u8]) -> io::Result<Identity> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid process stat record");
        // comm may contain spaces, newlines and parentheses, including a final ')'.
        let open = stat
            .iter()
            .position(|byte| *byte == b'(')
            .ok_or_else(invalid)?;
        let close = stat
            .iter()
            .rposition(|byte| *byte == b')')
            .ok_or_else(invalid)?;
        if close <= open {
            return Err(invalid());
        }
        let pid = std::str::from_utf8(&stat[..open])
            .map_err(|_| invalid())?
            .trim()
            .parse()
            .map_err(|_| invalid())?;
        let tail = std::str::from_utf8(&stat[close + 1..]).map_err(|_| invalid())?;
        let mut fields = tail.split_ascii_whitespace();
        fields.next().ok_or_else(invalid)?; // state, field 3
        let parent = fields
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let birth = fields
            .nth(17)
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        Ok(Identity { pid, parent, birth })
    }

    fn children(pid: u32) -> (Vec<u32>, io::Result<()>) {
        let mut children = Vec::new();
        let mut result = Ok(());
        let tasks = match fs::read_dir(format!("/proc/{pid}/task")) {
            Ok(tasks) => tasks,
            Err(error) if vanished(&error) => return (children, result),
            Err(error) => return (children, Err(error)),
        };
        // Linux children are per-thread, not just the thread-group leader's list.
        for task in tasks {
            let task = match task {
                Ok(task) => task,
                Err(error) if vanished(&error) => continue,
                Err(error) => {
                    remember(&mut result, Err(error));
                    continue;
                }
            };
            let contents = match fs::read_to_string(task.path().join("children")) {
                Ok(contents) => contents,
                Err(error) if vanished(&error) => continue,
                Err(error) => {
                    remember(&mut result, Err(error));
                    continue;
                }
            };
            for child in contents.split_ascii_whitespace() {
                match child.parse::<u32>() {
                    Ok(child) if child > 0 => children.push(child),
                    _ => remember(
                        &mut result,
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid child PID",
                        )),
                    ),
                }
            }
        }
        (children, result)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stat_parser_handles_arbitrary_comm_parentheses() {
            let mut stat = b"42 (name (with) spaces)\nand a final )) S 7".to_vec();
            for _ in 5..22 {
                stat.extend_from_slice(b" 0");
            }
            stat.extend_from_slice(b" 123456 0 0\n");
            assert_eq!(
                parse_stat(&stat).unwrap(),
                Identity {
                    pid: 42,
                    parent: 7,
                    birth: 123456
                }
            );
        }

        #[test]
        fn child_validation_rejects_changed_birth_or_parent() {
            let parent = Identity {
                pid: 7,
                parent: 1,
                birth: 10,
            };
            let child = Identity {
                pid: 42,
                parent: 7,
                birth: 20,
            };
            assert!(child_matches(parent, child, child));
            assert!(!child_matches(
                parent,
                child,
                Identity { birth: 21, ..child }
            ));
            assert!(!child_matches(
                parent,
                child,
                Identity { parent: 8, ..child }
            ));
            assert!(!child_matches(
                parent,
                Identity { parent: 8, ..child },
                child
            ));
            assert!(!parent.same_process(Identity {
                birth: 11,
                ..parent
            }));
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{child_pid_slice, remember, vanished};
    use std::collections::HashSet;
    use std::io;
    use std::mem::{MaybeUninit, size_of};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[link(name = "proc")]
    unsafe extern "C" {}

    #[derive(Clone, Copy, Debug)]
    struct Identity {
        pid: u32,
        parent: u32,
        birth: (u64, u64),
        zombie: bool,
    }

    impl Identity {
        fn same_process(self, other: Self) -> bool {
            self.pid == other.pid && self.birth == other.birth
        }
    }

    #[derive(Debug)]
    pub(super) struct Target {
        identity: Identity,
        signaled: AtomicBool,
    }

    impl Target {
        pub(super) fn terminate(&self) -> io::Result<()> {
            let current = identity(self.identity.pid)?;
            if current.is_none_or(|now| !self.identity.same_process(now) || now.zombie) {
                self.signaled.store(true, Ordering::Relaxed);
                return Ok(());
            }
            // macOS has no pidfd: birth validation makes numeric signaling best
            // effort, but cannot eliminate reuse between this check and kill.
            if unsafe { libc::kill(self.identity.pid as libc::pid_t, libc::SIGKILL) } < 0 {
                let error = io::Error::last_os_error();
                if !vanished(&error) {
                    return Err(error);
                }
            }
            self.signaled.store(true, Ordering::Relaxed);
            Ok(())
        }

        pub(super) fn wait(&self) -> io::Result<()> {
            if !self.signaled.load(Ordering::Relaxed) {
                return Err(io::Error::other("descendant was not successfully signaled"));
            }
            loop {
                match identity(self.identity.pid)? {
                    Some(now) if self.identity.same_process(now) && !now.zombie => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => return Ok(()),
                }
            }
        }
    }

    pub(super) fn capture(root: u32) -> (Vec<Target>, io::Result<()>) {
        let mut targets = Vec::new();
        let root = match identity(root) {
            Ok(Some(root)) => root,
            Ok(None) => return (targets, Ok(())),
            Err(error) => return (targets, Err(error)),
        };
        let mut pending = vec![root];
        let mut seen = HashSet::from([(root.pid, root.birth)]);
        let mut result = Ok(());
        while let Some(parent) = pending.pop() {
            let (children, discovery) = children(parent.pid);
            remember(&mut result, discovery);
            for pid in children {
                match capture_child(parent, pid) {
                    Ok(Some(child)) if seen.insert((child.pid, child.birth)) => {
                        pending.push(child);
                        targets.push(Target {
                            identity: child,
                            signaled: AtomicBool::new(false),
                        });
                    }
                    Ok(_) => {}
                    Err(error) => remember(&mut result, Err(error)),
                }
            }
        }
        (targets, result)
    }

    fn capture_child(parent: Identity, pid: u32) -> io::Result<Option<Identity>> {
        if !identity(parent.pid)?.is_some_and(|now| parent.same_process(now)) {
            return Ok(None);
        }
        let Some(before) = identity(pid)? else {
            return Ok(None);
        };
        if before.parent != parent.pid {
            return Ok(None);
        }
        let Some(after) = identity(pid)? else {
            return Ok(None);
        };
        if !before.same_process(after)
            || after.parent != parent.pid
            || !identity(parent.pid)?.is_some_and(|now| parent.same_process(now))
        {
            return Ok(None);
        }
        Ok(Some(after))
    }

    fn identity(pid: u32) -> io::Result<Option<Identity>> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer has exactly the layout and size required by libproc.
        let result = unsafe {
            *libc::__error() = 0;
            libc::proc_pidinfo(
                pid as libc::pid_t,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if result != size {
            let error = io::Error::last_os_error();
            return if result <= 0 && vanished(&error) {
                Ok(None)
            } else if result <= 0 && error.raw_os_error() != Some(0) {
                Err(error)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete process BSD info",
                ))
            };
        }
        // SAFETY: libproc reported a complete initialized proc_bsdinfo.
        let info = unsafe { info.assume_init() };
        if info.pbi_pid != pid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process BSD info PID mismatch",
            ));
        }
        Ok(Some(Identity {
            pid,
            parent: info.pbi_ppid,
            birth: (info.pbi_start_tvsec, info.pbi_start_tvusec),
            zombie: info.pbi_status == libc::SZOMB,
        }))
    }

    fn children(pid: u32) -> (Vec<u32>, io::Result<()>) {
        let mut children = Vec::new();
        let mut buffer = vec![0 as libc::pid_t; 32];
        loop {
            let bytes = match buffer
                .len()
                .checked_mul(size_of::<libc::pid_t>())
                .and_then(|bytes| libc::c_int::try_from(bytes).ok())
            {
                Some(bytes) => bytes,
                None => {
                    return (
                        children,
                        Err(io::Error::other("child PID buffer is too large")),
                    );
                }
            };
            // SAFETY: the initialized PID buffer is writable for exactly bytes.
            let count = unsafe {
                *libc::__error() = 0;
                libc::proc_listchildpids(pid as libc::pid_t, buffer.as_mut_ptr().cast(), bytes)
            };
            if count <= 0 {
                let error = io::Error::last_os_error();
                return if (count == 0 && error.raw_os_error() == Some(0)) || vanished(&error) {
                    (children, Ok(()))
                } else if error.raw_os_error() == Some(0) {
                    (
                        children,
                        Err(io::Error::other("child PID discovery failed")),
                    )
                } else {
                    (children, Err(error))
                };
            }
            // proc_listchildpids accepts bytes but returns the number of PIDs.
            let captured = match child_pid_slice(&buffer, count as usize) {
                Ok(captured) => captured,
                Err(error) => return (children, Err(error)),
            };
            children.extend(
                captured
                    .iter()
                    .filter_map(|pid| u32::try_from(*pid).ok().filter(|pid| *pid > 0)),
            );
            if captured.len() < buffer.len() {
                return (children, Ok(()));
            }
            // A full buffer may have omitted children. Preserve its partial list
            // if the subsequent larger request fails.
            buffer.resize(buffer.len() * 2, 0);
        }
    }
}
