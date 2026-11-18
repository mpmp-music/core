//! UNC paths that lead back to this computer (Windows): `\\localhost\C$\Music`,
//! `\\127.0.0.1\c$\Music`, `\\<this computer's name>\Music` and a drive mapped to one of
//! them are the same folder as `C:\Music`, so they must be judged as that folder
//! (a `forbid` of `C:\Music` must hold them). [`local_path`] turns such a path into the
//! local one; [`may_be_local`] tells a UNC path whose host may be this computer (for a
//! `forbid`, when the local path cannot be told).
//!
//! No name is looked up on the network (that can block for long): a host is this computer
//! when it is `localhost`, a loopback address, one of this computer's own names
//! (`GetComputerNameExW`) or one of its own addresses (`GetAdaptersAddresses`). The
//! parsing and judging are plain functions, tested on every platform; only [`system`]
//! asks Windows.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A UNC path, split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unc<'a> {
    pub host: &'a str,
    pub share: &'a str,
    /// What follows the share, without the separator before it (empty for the share
    /// itself).
    pub rest: &'a str,
    /// `\\?\UNC\…` (or `\??\UNC\…`): only `\` separates, and nothing is resolved.
    pub verbatim: bool,
}

/// What a UNC host is to this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    /// This computer, for sure.
    Local,
    /// Perhaps this computer (a spelling of its names that is not one of them exactly, an
    /// unspecified address): cannot be told without the network.
    MaybeLocal,
    /// Another computer (by its address), or a name that is none of this computer's.
    Other,
}

/// This computer's own names and addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Me {
    pub names: Vec<String>,
    pub addrs: Vec<IpAddr>,
}

fn is_sep(c: u8, verbatim: bool) -> bool {
    c == b'\\' || (!verbatim && c == b'/')
}

fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Splits a UNC path: `\\host\share\rest`, `//host/share/rest`, `\\?\UNC\host\share\rest`,
/// `\??\UNC\host\share\rest`, `\\.\UNC\host\share\rest`. `None` for anything else
/// (a drive path, `\\?\C:\…`, `\\.\pipe\…`, a host or share that is empty).
pub(crate) fn parse(path: &str) -> Option<Unc<'_>> {
    let (body, verbatim) = if let Some(body) = strip_prefix_ignoring_case(path, r"\\?\UNC\")
        .or_else(|| strip_prefix_ignoring_case(path, r"\??\UNC\"))
    {
        (body, true)
    } else if let Some(body) = strip_prefix_ignoring_case(path, r"\\.\UNC\")
        .or_else(|| strip_prefix_ignoring_case(path, "//./UNC/"))
    {
        (body, false)
    } else {
        let bytes = path.as_bytes();
        if bytes.len() < 2 || !is_sep(bytes[0], false) || !is_sep(bytes[1], false) {
            return None;
        }
        let body = &path[2..];
        // `\\?\…` and `\\.\…` (verbatim and device paths) are not UNC.
        let device = body.as_bytes();
        if matches!(device.first(), Some(b'?' | b'.'))
            && device.get(1).is_none_or(|&c| is_sep(c, false))
        {
            return None;
        }
        (body, false)
    };
    let (host, after) = split_once(body, verbatim);
    let (share, rest) = split_once(after, verbatim);
    (!host.is_empty() && !share.is_empty()).then_some(Unc {
        host,
        share,
        rest,
        verbatim,
    })
}

/// `text` before and after its first separator (all of it, and nothing, without one).
fn split_once(text: &str, verbatim: bool) -> (&str, &str) {
    match text.bytes().position(|c| is_sep(c, verbatim)) {
        Some(at) => (&text[..at], &text[at + 1..]),
        None => (text, ""),
    }
}

/// The drive letter of an administrative share (`C$`, `d$`), upper case.
pub(crate) fn admin_drive(share: &str) -> Option<char> {
    match share.as_bytes() {
        [letter, b'$'] if letter.is_ascii_alphabetic() => Some(letter.to_ascii_uppercase() as char),
        _ => None,
    }
}

/// The local path `unc` names on this computer, with `share_path` telling a share's local
/// folder (`X$` is `X:\`). `None` when the share's folder is unknown or not on a drive, or a
/// verbatim rest holds what a local path would read otherwise (`/`, `.`, `..`).
pub(crate) fn local_form(
    unc: &Unc<'_>,
    share_path: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let base = match admin_drive(unc.share) {
        Some(letter) => format!(r"{letter}:\"),
        None => share_path(unc.share).filter(|p| is_drive_path(p))?,
    };
    if unc.verbatim
        && (unc.rest.contains('/') || unc.rest.split('\\').any(|part| matches!(part, "." | "..")))
    {
        return None;
    }
    let mut out = base.trim_end_matches(['\\', '/']).to_owned();
    out.push('\\');
    out.push_str(unc.rest);
    Some(out)
}

/// `X:\` or `X:\…`.
fn is_drive_path(path: &str) -> bool {
    matches!(path.as_bytes(), [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic())
}

/// What `host` (as in a UNC path) is to the computer `me`. Nothing is looked up.
pub(crate) fn host_kind(host: &str, me: &Me) -> Host {
    // A name may end in the root's `.`.
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return Host::MaybeLocal;
    }
    if let Some(ip) = ip_literal(host) {
        return ip_kind(ip, me);
    }
    let name = host.to_lowercase();
    if name == "localhost" || me.names.iter().any(|n| n.to_lowercase() == name) {
        return Host::Local;
    }
    // Another spelling of one of them (`pc.local`, `pc.lan`, `x.localhost`): perhaps.
    let first = |n: &str| n.split('.').next().unwrap_or_default().to_lowercase();
    let label = first(&name);
    if label == "localhost"
        || name.ends_with(".localhost")
        || me.names.iter().any(|n| first(n) == label)
    {
        return Host::MaybeLocal;
    }
    Host::Other
}

fn ip_kind(ip: IpAddr, me: &Me) -> Host {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    if ip.is_loopback() || me.addrs.contains(&ip) {
        Host::Local
    } else if ip.is_unspecified() {
        Host::MaybeLocal
    } else {
        Host::Other
    }
}

/// An address written as a UNC host: IPv4 as Windows reads it (`127.0.0.1`, `127.1`,
/// `0x7f.1`, `2130706433`), IPv6 bare or in brackets (a zone dropped), or as an
/// `ipv6-literal.net` name (`0--1.ipv6-literal.net` is `::1`).
pub(crate) fn ip_literal(host: &str) -> Option<IpAddr> {
    let v6 = |text: &str| {
        let text = text.split('%').next().unwrap_or_default();
        text.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
    };
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6(inner);
    }
    if host.contains(':') {
        return v6(host);
    }
    if let Some(literal) = strip_suffix_ignoring_case(host, ".ipv6-literal.net") {
        let text: String = literal
            .chars()
            .map(|c| match c {
                '-' => ':',
                's' | 'S' => '%',
                c => c,
            })
            .collect();
        return v6(&text);
    }
    inet_aton(host).map(IpAddr::V4)
}

fn strip_suffix_ignoring_case<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let at = text.len().checked_sub(suffix.len())?;
    let tail = text.get(at..)?;
    tail.eq_ignore_ascii_case(suffix).then(|| &text[..at])
}

/// IPv4 in the forms `inet_aton` reads: one to four parts, each decimal, octal (leading
/// `0`) or hexadecimal (`0x`); the last part fills the bytes left.
fn inet_aton(text: &str) -> Option<Ipv4Addr> {
    let parts = text.split('.').map(number).collect::<Option<Vec<u64>>>()?;
    let (last, init) = parts.split_last()?;
    if init.len() > 3 || init.iter().any(|&n| n > 255) {
        return None;
    }
    let left = 8 * (4 - init.len() as u32);
    if *last >= 1u64 << left {
        return None;
    }
    let mut value = *last as u32;
    for (i, &n) in init.iter().enumerate() {
        value |= (n as u32) << (24 - 8 * i as u32);
    }
    Some(Ipv4Addr::from(value))
}

fn number(part: &str) -> Option<u64> {
    let (digits, radix) = if let Some(hex) = strip_prefix_ignoring_case(part, "0x") {
        (hex, 16)
    } else if part.len() > 1 && part.starts_with('0') {
        (&part[1..], 8)
    } else {
        (part, 10)
    };
    if part.is_empty() || digits.len() > 16 || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, radix).ok()
}

/// The local path a UNC path to this computer names (`\\localhost\C$\Music` →
/// `C:\Music`); `None` when it is no such path, or its share's local folder cannot be told.
/// Asks Windows (nothing elsewhere).
pub(crate) fn local_path(path: &str) -> Option<String> {
    let unc = parse(path)?;
    if host_kind(unc.host, &system::me()) != Host::Local {
        return None;
    }
    local_form(&unc, system::share_path)
}

/// Whether `path` is a UNC path whose host is, or may be, this computer.
pub(crate) fn may_be_local(path: &str) -> bool {
    parse(path).is_some_and(|unc| host_kind(unc.host, &system::me()) != Host::Other)
}

#[cfg(not(windows))]
mod system {
    use super::Me;

    pub(super) fn me() -> Me {
        Me::default()
    }

    pub(super) fn share_path(_share: &str) -> Option<String> {
        None
    }
}

#[cfg(windows)]
mod system {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_FRIENDLY_NAME,
        GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
        IP_ADAPTER_UNICAST_ADDRESS_LH,
    };
    use windows_sys::Win32::NetworkManagement::NetManagement::{NERR_Success, NetApiBufferFree};
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        NetShareGetInfo, SHARE_INFO_2, STYPE_DISKTREE, STYPE_MASK,
    };
    use windows_sys::Win32::System::SystemInformation::{
        COMPUTER_NAME_FORMAT, ComputerNameDnsFullyQualified, ComputerNameDnsHostname,
        ComputerNameNetBIOS, ComputerNamePhysicalDnsFullyQualified,
        ComputerNamePhysicalDnsHostname, ComputerNamePhysicalNetBIOS, GetComputerNameExW,
    };

    use super::Me;

    pub(super) fn me() -> Me {
        let mut names = Vec::new();
        for format in [
            ComputerNameNetBIOS,
            ComputerNameDnsHostname,
            ComputerNameDnsFullyQualified,
            ComputerNamePhysicalNetBIOS,
            ComputerNamePhysicalDnsHostname,
            ComputerNamePhysicalDnsFullyQualified,
        ] {
            if let Some(name) = computer_name(format)
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
        Me {
            names,
            addrs: addresses(),
        }
    }

    fn computer_name(format: COMPUTER_NAME_FORMAT) -> Option<String> {
        let mut size = 0u32;
        // SAFETY: a null buffer of size 0 only asks for the size needed.
        unsafe { GetComputerNameExW(format, null_mut(), &mut size) };
        if size == 0 || size > 1024 {
            return None;
        }
        let mut buffer = vec![0u16; size as usize];
        // SAFETY: `buffer` holds `size` UTF-16 units.
        let ok = unsafe { GetComputerNameExW(format, buffer.as_mut_ptr(), &mut size) };
        if ok == 0 {
            return None;
        }
        // On success `size` is the length without the terminating null.
        let name = String::from_utf16(buffer.get(..size as usize)?).ok()?;
        (!name.is_empty()).then_some(name)
    }

    /// This computer's unicast addresses, on every adapter.
    fn addresses() -> Vec<IpAddr> {
        let flags = GAA_FLAG_SKIP_ANYCAST
            | GAA_FLAG_SKIP_MULTICAST
            | GAA_FLAG_SKIP_DNS_SERVER
            | GAA_FLAG_SKIP_FRIENDLY_NAME;
        let mut size = 16 * 1024u32;
        for _ in 0..4 {
            // `u64`s: aligned for the structures written into it.
            let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
            let first = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            // SAFETY: `buffer` holds at least `size` bytes.
            let status =
                unsafe { GetAdaptersAddresses(AF_UNSPEC as u32, flags, null(), first, &mut size) };
            if status == ERROR_BUFFER_OVERFLOW {
                continue;
            }
            if status != 0 {
                return Vec::new();
            }
            let mut found = Vec::new();
            let mut adapter = first.cast_const();
            while !adapter.is_null() {
                // SAFETY: a list the call wrote into `buffer`, which outlives this loop.
                let a = unsafe { &*adapter };
                let mut unicast = a.FirstUnicastAddress.cast_const();
                while !unicast.is_null() {
                    // SAFETY: as above.
                    let u: &IP_ADAPTER_UNICAST_ADDRESS_LH = unsafe { &*unicast };
                    // SAFETY: the address the call wrote, of `iSockaddrLength` bytes.
                    if let Some(ip) = unsafe { address(u.Address.lpSockaddr) } {
                        found.push(ip);
                    }
                    unicast = u.Next.cast_const();
                }
                adapter = a.Next.cast_const();
            }
            return found;
        }
        Vec::new()
    }

    /// # Safety
    /// `sockaddr` is null or points to a socket address of its family's size.
    unsafe fn address(sockaddr: *const SOCKADDR) -> Option<IpAddr> {
        if sockaddr.is_null() {
            return None;
        }
        // SAFETY: by the caller.
        let family = unsafe { (*sockaddr).sa_family };
        match family {
            AF_INET => {
                // SAFETY: an `AF_INET` address is a `SOCKADDR_IN`.
                let v4 = unsafe { &*sockaddr.cast::<SOCKADDR_IN>() };
                // SAFETY: every view of the union is plain bytes.
                let raw = unsafe { v4.sin_addr.S_un.S_addr };
                // In network order in memory.
                Some(IpAddr::V4(Ipv4Addr::from(raw.to_ne_bytes())))
            }
            AF_INET6 => {
                // SAFETY: an `AF_INET6` address is a `SOCKADDR_IN6`.
                let v6 = unsafe { &*sockaddr.cast::<SOCKADDR_IN6>() };
                // SAFETY: every view of the union is plain bytes.
                let bytes = unsafe { v6.sin6_addr.u.Byte };
                Some(IpAddr::V6(Ipv6Addr::from(bytes)))
            }
            _ => None,
        }
    }

    /// The local folder of this computer's disk share `share` (`NetShareGetInfo`, level 2;
    /// Windows gives it to administrators only, so usually `None`).
    pub(super) fn share_path(share: &str) -> Option<String> {
        if share.contains('\0') {
            return None;
        }
        let name: Vec<u16> = share.encode_utf16().chain(Some(0)).collect();
        let mut buffer: *mut u8 = null_mut();
        // SAFETY: `name` ends in a null; the call allocates `buffer`, freed below.
        let status = unsafe { NetShareGetInfo(null(), name.as_ptr(), 2, &mut buffer) };
        let path = if status == NERR_Success && !buffer.is_null() {
            // SAFETY: level 2 writes a `SHARE_INFO_2`.
            let info = unsafe { &*buffer.cast::<SHARE_INFO_2>() };
            if info.shi2_type & STYPE_MASK == STYPE_DISKTREE {
                // SAFETY: a null-terminated string in the same allocation.
                unsafe { wide(info.shi2_path) }
            } else {
                None
            }
        } else {
            None
        };
        if !buffer.is_null() {
            // SAFETY: allocated by `NetShareGetInfo`.
            unsafe { NetApiBufferFree(buffer.cast_const().cast()) };
        }
        path
    }

    /// # Safety
    /// `text` is null or a null-terminated UTF-16 string.
    unsafe fn wide(text: *const u16) -> Option<String> {
        if text.is_null() {
            return None;
        }
        let mut len = 0usize;
        // SAFETY: by the caller; Windows paths are far shorter than this bound.
        while len < 32_768 && unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` units were read above.
        let units = unsafe { std::slice::from_raw_parts(text, len) };
        String::from_utf16(units).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_paths_are_split() {
        let unc = |host, share, rest, verbatim| {
            Some(Unc {
                host,
                share,
                rest,
                verbatim,
            })
        };
        assert_eq!(
            parse(r"\\localhost\C$\Music\a.m3u"),
            unc("localhost", "C$", r"Music\a.m3u", false)
        );
        assert_eq!(parse("//nas/music/x"), unc("nas", "music", "x", false));
        assert_eq!(parse(r"\\nas\music"), unc("nas", "music", "", false));
        assert_eq!(parse(r"\\nas\music\"), unc("nas", "music", "", false));
        assert_eq!(
            parse(r"\\?\UNC\127.0.0.1\c$\x/y"),
            unc("127.0.0.1", "c$", "x/y", true)
        );
        assert_eq!(
            parse(r"\\?\unc\h\s\x"),
            unc("h", "s", "x", true),
            "the prefix is read without regard to case"
        );
        assert_eq!(parse(r"\??\UNC\h\s"), unc("h", "s", "", true));
        assert_eq!(parse(r"\\.\UNC\h\s\x"), unc("h", "s", "x", false));
        for not in [
            r"C:\Music",
            r"\\?\C:\Music",
            r"\\.\C:\Music",
            r"\\.\pipe\x",
            r"\\?\Volume{1}\x",
            r"\\server",
            r"\\server\",
            r"\\\share\x",
            "/home/me",
            "",
        ] {
            assert_eq!(parse(not), None, "{not:?}");
        }
    }

    #[test]
    fn hosts_are_told_without_the_network() {
        let me = Me {
            names: vec!["DESKTOP-7Q".into(), "desktop-7q.corp.example".into()],
            addrs: vec![
                "192.168.1.20".parse().unwrap(),
                "fe80::1234".parse().unwrap(),
            ],
        };
        for local in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "127.0.0.1",
            "127.1.2.3",
            "127.1",
            "0x7f.1",
            "0177.0.0.1",
            "2130706433",
            "::1",
            "[::1]",
            "[0:0:0:0:0:0:0:1]",
            "::ffff:127.0.0.1",
            "0--1.ipv6-literal.net",
            "desktop-7q",
            "Desktop-7Q.corp.example",
            "192.168.1.20",
            "[fe80::1234%3]",
            "fe80--1234s3.ipv6-literal.net",
        ] {
            assert_eq!(host_kind(local, &me), Host::Local, "{local}");
        }
        for maybe in [
            "desktop-7q.local",
            "DESKTOP-7Q.lan",
            "x.localhost",
            "localhost.example",
            "0.0.0.0",
            "0",
            "[::]",
        ] {
            assert_eq!(host_kind(maybe, &me), Host::MaybeLocal, "{maybe}");
        }
        for other in [
            "nas",
            "nas.local",
            "10.255.255.1",
            "192.168.1.21",
            "[fe80::1]",
            "8.8.8.8",
            "desktop-7qx",
        ] {
            assert_eq!(host_kind(other, &me), Host::Other, "{other}");
        }
    }

    #[test]
    fn ipv4_is_read_as_windows_reads_it() {
        let v4 = |s: &str| inet_aton(s).map(|a| a.to_string());
        assert_eq!(v4("127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(v4("127.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(v4("127.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(v4("0x7F000001").as_deref(), Some("127.0.0.1"));
        assert_eq!(v4("017700000001").as_deref(), Some("127.0.0.1"));
        assert_eq!(v4("10.0x10.010.1").as_deref(), Some("10.16.8.1"));
        for not in [
            "",
            "1.2.3.4.5",
            "256.1.1.1",
            "1.2.3.256",
            "1..2",
            "08",
            "0x1g",
            "+1",
            "a.b",
        ] {
            assert_eq!(v4(not), None, "{not:?}");
        }
    }

    #[test]
    fn shares_lead_to_local_folders() {
        let none = |_: &str| None;
        let form = |path: &str, share: Option<&str>| {
            local_form(&parse(path).unwrap(), |_| share.map(str::to_owned))
        };
        assert_eq!(
            form(r"\\h\C$\Music\a.m3u", None).as_deref(),
            Some(r"C:\Music\a.m3u")
        );
        assert_eq!(form(r"\\h\d$", None).as_deref(), Some(r"D:\"));
        assert_eq!(form("//h/c$/x/../y", None).as_deref(), Some(r"C:\x/../y"));
        assert_eq!(
            form(r"\\h\Music\Jazz", Some(r"E:\Media\Music\")).as_deref(),
            Some(r"E:\Media\Music\Jazz")
        );
        assert_eq!(form(r"\\h\Music\Jazz", None), None, "share not known");
        assert_eq!(
            form(r"\\h\Music\Jazz", Some(r"\\other\x")),
            None,
            "not on a drive"
        );
        assert_eq!(form(r"\\h\IPC$", None), None);
        // Verbatim: a `/`, `.` or `..` would be read otherwise once local.
        assert_eq!(form(r"\\?\UNC\h\C$\x\y", None).as_deref(), Some(r"C:\x\y"));
        for verbatim in [
            r"\\?\UNC\h\C$\x/y",
            r"\\?\UNC\h\C$\x\..\y",
            r"\\?\UNC\h\C$\.\y",
        ] {
            assert_eq!(
                local_form(&parse(verbatim).unwrap(), none),
                None,
                "{verbatim}"
            );
        }
        assert_eq!(admin_drive("c$"), Some('C'));
        for not in ["C", "CC$", "1$", "ADMIN$", "$"] {
            assert_eq!(admin_drive(not), None, "{not}");
        }
    }

    /// Elsewhere than on Windows no host is this computer by asking the system.
    #[cfg(not(windows))]
    #[test]
    fn only_windows_is_asked() {
        assert_eq!(local_path(r"\\nas\C$\x"), None);
        assert!(
            may_be_local(r"\\localhost\C$\x"),
            "localhost needs no asking"
        );
        assert_eq!(local_path(r"\\localhost\C$\x").as_deref(), Some(r"C:\x"));
        assert!(!may_be_local(r"\\nas\C$\x"));
        assert!(!may_be_local("/home/x"));
    }
}
