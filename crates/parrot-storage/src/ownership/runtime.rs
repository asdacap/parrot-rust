use super::{ProcessIdentity, ProcessIdentityStatus, RuntimeIdentity};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
use std::sync::Arc;

const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
const TICKS_PER_SECOND: i64 = 10_000_000;

pub fn fingerprint_host(host_key: &str) -> String {
    format!("{:x}", Sha256::digest(host_key.as_bytes()))
}

pub(crate) fn random_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!("{:032x}", u128::from_be_bytes(bytes)))
}

pub fn host_key() -> io::Result<String> {
    match fs::read_to_string("/etc/machine-id") {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
        Ok(_) => machine_name(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => machine_name(),
        Err(error) => Err(error),
    }
}

fn machine_name() -> io::Result<String> {
    unsafe extern "C" {
        fn gethostname(name: *mut std::ffi::c_char, length: usize) -> std::ffi::c_int;
    }
    let mut name = [0_u8; 256];
    // SAFETY: the writable buffer has exactly the supplied capacity.
    if unsafe { gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    Ok(String::from_utf8_lossy(&name[..end]).into_owned())
}

pub fn capture_runtime_identity(host_key: &str) -> io::Result<RuntimeIdentity> {
    let process_id = i32::try_from(std::process::id())
        .map_err(|_| io::Error::other("current process ID is outside the source range"))?;
    let process = inspect_process(process_id);
    let process_start = match process {
        ProcessIdentity {
            status: ProcessIdentityStatus::Found,
            process_start_token: Some(token),
        } => token,
        _ => {
            return Err(io::Error::other(
                "The current process identity is unavailable.",
            ));
        }
    };
    let boot_identity = match fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_owned(),
        Ok(_) => "unavailable".to_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => "unavailable".to_owned(),
        Err(error) => return Err(error),
    };
    Ok(RuntimeIdentity::new(
        fingerprint_host(host_key),
        boot_identity,
        process_id,
        process_start,
        random_id()?,
        Arc::new(inspect_process),
    ))
}

pub fn inspect_process(process_id: i32) -> ProcessIdentity {
    if process_id <= 0 {
        return identity(ProcessIdentityStatus::Missing, None);
    }
    inspect_native(process_id)
}

fn identity(status: ProcessIdentityStatus, process_start_token: Option<String>) -> ProcessIdentity {
    ProcessIdentity {
        status,
        process_start_token,
    }
}

#[cfg(target_os = "linux")]
fn inspect_native(process_id: i32) -> ProcessIdentity {
    let read = || -> io::Result<Option<String>> {
        let stat = fs::read_to_string(format!("/proc/{process_id}/stat"))?;
        // comm may contain spaces and closing parentheses; fields begin after
        // its final closing parenthesis. starttime is field22, state is field3.
        let tail = stat
            .rfind(')')
            .and_then(|end| stat.get(end + 1..))
            .ok_or_else(|| io::Error::other("invalid process stat record"))?;
        let fields: Vec<_> = tail.split_whitespace().collect();
        if matches!(fields.first(), Some(&"Z") | Some(&"X")) {
            return Ok(None);
        }
        let start = fields
            .get(19)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| io::Error::other("invalid process start clock ticks"))?;
        let boot = fs::read_to_string("/proc/stat")?
            .lines()
            .find_map(|line| line.strip_prefix("btime ")?.trim().parse::<i64>().ok())
            .ok_or_else(|| io::Error::other("process boot time is unavailable"))?;
        unsafe extern "C" {
            fn sysconf(name: std::ffi::c_int) -> std::ffi::c_long;
        }
        // Linux _SC_CLK_TCK is 2; sysconf has no pointer arguments.
        let clock_ticks = unsafe { sysconf(2) };
        let ticks = process_start_ticks(boot, start, u64::try_from(clock_ticks).unwrap_or(0))
            .ok_or_else(|| io::Error::other("process start ticks are unavailable"))?;
        Ok(Some(ticks.to_string()))
    };
    match read() {
        Ok(Some(token)) => identity(ProcessIdentityStatus::Found, Some(token)),
        Ok(None) => identity(ProcessIdentityStatus::Missing, None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            identity(ProcessIdentityStatus::Missing, None)
        }
        Err(_) => identity(ProcessIdentityStatus::Unreadable, None),
    }
}

fn process_start_ticks(boot_unix_seconds: i64, start: u64, frequency: u64) -> Option<i64> {
    if frequency == 0 {
        return None;
    }
    let elapsed = i128::from(start) * i128::from(TICKS_PER_SECOND) / i128::from(frequency);
    i64::try_from(
        i128::from(UNIX_EPOCH_TICKS)
            + i128::from(boot_unix_seconds) * i128::from(TICKS_PER_SECOND)
            + elapsed,
    )
    .ok()
}

#[cfg(target_os = "macos")]
fn inspect_native(process_id: i32) -> ProcessIdentity {
    #[repr(C)]
    #[derive(Default)]
    struct BsdInfo {
        fields: [u32; 12],
        command: [u8; 16],
        name: [u8; 32],
        trailing: [u32; 6],
        start_seconds: u64,
        start_microseconds: u64,
    }
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut std::ffi::c_void,
            size: i32,
        ) -> i32;
    }
    let mut info = BsdInfo::default();
    let length = std::mem::size_of::<BsdInfo>() as i32;
    // SAFETY: PROC_PIDTBSDINFO (3) fills the repr(C) proc_bsdinfo buffer.
    let result =
        unsafe { proc_pidinfo(process_id, 3, 0, (&mut info as *mut BsdInfo).cast(), length) };
    if result != length {
        let error = io::Error::last_os_error();
        return identity(
            if error.raw_os_error() == Some(3) {
                ProcessIdentityStatus::Missing
            } else {
                ProcessIdentityStatus::Unreadable
            },
            None,
        );
    }
    if info.fields[1] == 5 {
        return identity(ProcessIdentityStatus::Missing, None);
    }
    let ticks = i128::from(UNIX_EPOCH_TICKS)
        + i128::from(info.start_seconds) * i128::from(TICKS_PER_SECOND)
        + i128::from(info.start_microseconds) * 10;
    match i64::try_from(ticks) {
        Ok(ticks) => identity(ProcessIdentityStatus::Found, Some(ticks.to_string())),
        Err(_) => identity(ProcessIdentityStatus::Unreadable, None),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn inspect_native(_: i32) -> ProcessIdentity {
    identity(ProcessIdentityStatus::Unreadable, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_clock_ticks_use_dotnet_utc_epoch_and_integer_precision() {
        assert_eq!(process_start_ticks(0, 0, 100), Some(UNIX_EPOCH_TICKS));
        assert_eq!(
            process_start_ticks(1, 123, 100),
            Some(UNIX_EPOCH_TICKS + 22_300_000)
        );
        assert_eq!(
            process_start_ticks(1, 1, 3),
            Some(UNIX_EPOCH_TICKS + 13_333_333)
        );
        assert_eq!(process_start_ticks(1, 1, 0), None);
    }

    #[test]
    fn missing_process_and_current_identity_are_observable() {
        assert_eq!(inspect_process(0).status, ProcessIdentityStatus::Missing);
        assert_eq!(inspect_process(-1).status, ProcessIdentityStatus::Missing);
        let identity = capture_runtime_identity("synthetic-host").unwrap();
        assert_eq!(identity.host_identity, fingerprint_host("synthetic-host"));
        assert_eq!(identity.process_id, std::process::id() as i32);
        assert!(identity.process_start_token.parse::<i64>().unwrap() > UNIX_EPOCH_TICKS);
        assert_eq!(identity.runtime_instance_id.len(), 32);
    }
}
