//! Per-child seccomp network filter. No-op on non-Linux.

/// Install seccomp BPF filter blocking network syscalls.
///
/// # Safety
///
/// Must be called in a `pre_exec` context (after `fork`, before `exec`).
#[cfg(target_os = "linux")]
pub unsafe fn install_child_network_filter() -> std::io::Result<()> {
    use libc::{
        BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, PR_SET_NO_NEW_PRIVS,
        PR_SET_SECCOMP, SECCOMP_MODE_FILTER, SYS_accept, SYS_accept4, SYS_bind, SYS_connect,
        SYS_listen, SYS_sendmsg, SYS_sendto, prctl, sock_filter, sock_fprog,
    };

    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const EPERM_VAL: u32 = 1; // libc::EPERM

    macro_rules! bpf_stmt {
        ($code:expr, $k:expr) => {
            sock_filter {
                code: $code as u16,
                jt: 0,
                jf: 0,
                k: $k as u32,
            }
        };
    }

    macro_rules! bpf_jump {
        ($code:expr, $k:expr, $jt:expr, $jf:expr) => {
            sock_filter {
                code: $code as u16,
                jt: $jt,
                jf: $jf,
                k: $k as u32,
            }
        };
    }

    const NR_OFFSET: u32 = 0; // seccomp_data.nr offset

    let blocked_syscalls: &[i64] = &[
        SYS_connect,
        SYS_bind,
        SYS_sendto,
        SYS_sendmsg,
        SYS_listen,
        SYS_accept,
        SYS_accept4,
    ];

    let mut filter: Vec<sock_filter> = Vec::new();
    let total_checks = blocked_syscalls.len();

    // 1. Load syscall number
    filter.push(bpf_stmt!(BPF_LD | BPF_W | BPF_ABS, NR_OFFSET));

    // 2. Check each blocked syscall
    for (i, &syscall) in blocked_syscalls.iter().enumerate() {
        let remaining = total_checks - i - 1;
        filter.push(bpf_jump!(
            BPF_JMP | BPF_JEQ | BPF_K,
            syscall,
            remaining as u8 + 1, // match: jump to ERRNO
            0                    // no match: check next
        ));
    }

    // 3. Default: ALLOW
    filter.push(bpf_stmt!(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));

    // 4. Blocked: ERRNO(EPERM)
    filter.push(bpf_stmt!(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));

    let prog = sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };

    // Must set PR_SET_NO_NEW_PRIVS before applying seccomp filter
    // SAFETY: prctl with PR_SET_NO_NEW_PRIVS is safe in pre_exec context.
    if unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: prog is a valid sock_fprog pointing to our filter array.
    if unsafe {
        prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER as libc::c_ulong,
            &prog as *const _ as libc::c_ulong,
            0,
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

/// # Safety
///
/// No-op on non-Linux.
#[cfg(not(target_os = "linux"))]
pub unsafe fn install_child_network_filter() -> std::io::Result<()> {
    Ok(())
}
