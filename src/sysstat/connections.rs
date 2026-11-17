#[cfg(target_os = "linux")]
pub(super) fn count_connections() -> (Option<u64>, Option<u64>) {
    fn table(path: &str, ipv6: bool) -> Option<u64> {
        match std::fs::read_to_string(path) {
            Ok(text) => Some(text.lines().skip(1).filter(|line| !line.trim().is_empty()).count() as u64),
            Err(error) if ipv6 && error.kind() == std::io::ErrorKind::NotFound => Some(0),
            Err(_) => None,
        }
    }
    (
        table("/proc/net/tcp", false).zip(table("/proc/net/tcp6", true)).map(|(a, b)| a + b),
        table("/proc/net/udp", false).zip(table("/proc/net/udp6", true)).map(|(a, b)| a + b),
    )
}

#[cfg(target_os = "windows")]
pub(super) fn count_connections() -> (Option<u64>, Option<u64>) {
    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    fn table(tcp: bool, family: u32) -> Option<u64> {
        let mut size = 0;
        let mut buffer = Vec::<u32>::new();
        for _ in 0..3 {
            let pointer = if buffer.is_empty() {
                std::ptr::null_mut()
            } else {
                buffer.as_mut_ptr().cast()
            };
            let status = unsafe {
                if tcp {
                    GetExtendedTcpTable(pointer, &mut size, 0, family, TCP_TABLE_OWNER_PID_ALL, 0)
                } else {
                    GetExtendedUdpTable(pointer, &mut size, 0, family, UDP_TABLE_OWNER_PID, 0)
                }
            };
            if status == NO_ERROR {
                return buffer.first().map(|count| u64::from(*count));
            }
            if status != ERROR_INSUFFICIENT_BUFFER {
                return None;
            }
            buffer.resize((size as usize).div_ceil(std::mem::size_of::<u32>()), 0);
        }
        None
    }
    let total = |tcp| table(tcp, u32::from(AF_INET))
        .zip(table(tcp, u32::from(AF_INET6))).map(|(a, b)| a + b);
    (total(true), total(false))
}

#[cfg(any(target_os = "macos", test))]
fn netstat_counts(text: &str) -> (u64, u64) {
    text.lines().fold((0, 0), |(tcp, udp), line| match line.split_whitespace().next() {
        Some("tcp" | "tcp4" | "tcp6" | "tcp46") => (tcp + 1, udp),
        Some("udp" | "udp4" | "udp6" | "udp46") => (tcp, udp + 1),
        _ => (tcp, udp),
    })
}

#[cfg(target_os = "macos")]
pub(super) fn count_connections() -> (Option<u64>, Option<u64>) {
    std::process::Command::new("/usr/sbin/netstat")
        .args(["-an"]).output().ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| {
            let (tcp, udp) = netstat_counts(&text);
            (Some(tcp), Some(udp))
        })
        .unwrap_or((None, None))
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub(super) fn count_connections() -> (Option<u64>, Option<u64>) {
    (None, None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn macos_counts_ipv4_ipv6_without_headers_or_unix_sockets() {
        assert_eq!(super::netstat_counts("Active Internet connections\nProto Recv-Q Send-Q\ntcp4 0 0\ntcp6 0 0\nudp4 0 0\nudp6 0 0\nunix 0 0\n"), (2, 2));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_counts_real_listeners() {
        let _tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let (tcp, udp) = super::count_connections();
        assert!(tcp.is_some_and(|count| count > 0));
        assert!(udp.is_some_and(|count| count > 0));
    }
}
