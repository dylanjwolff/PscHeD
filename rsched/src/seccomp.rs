#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::arch::global_asm;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
global_asm!(
    r#"
    .text
    .globl rsched_internal_syscall6
    .hidden rsched_internal_syscall6
    .type rsched_internal_syscall6,@function
rsched_internal_syscall6:
    mov rax, rdi
    mov rdi, rsi
    mov rsi, rdx
    mov rdx, rcx
    mov r10, r8
    mov r8,  r9
    mov r9,  qword ptr [rsp + 8]
    .globl rsched_internal_syscall6_insn
    .hidden rsched_internal_syscall6_insn
rsched_internal_syscall6_insn:
    syscall
    ret
    .size rsched_internal_syscall6, .-rsched_internal_syscall6
"#
);

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe extern "C" {
    fn rsched_internal_syscall6(
        nr: libc::c_long,
        a0: libc::c_long,
        a1: libc::c_long,
        a2: libc::c_long,
        a3: libc::c_long,
        a4: libc::c_long,
        a5: libc::c_long,
    ) -> libc::c_long;

    static rsched_internal_syscall6_insn: u8;
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) unsafe fn raw_syscall6(
    nr: libc::c_long,
    a0: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
) -> libc::c_long {
    rsched_internal_syscall6(nr, a0, a1, a2, a3, a4, a5)
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) unsafe fn raw_syscall6(
    nr: libc::c_long,
    a0: libc::c_long,
    a1: libc::c_long,
    a2: libc::c_long,
    a3: libc::c_long,
    a4: libc::c_long,
    a5: libc::c_long,
) -> libc::c_long {
    libc::syscall(nr, a0, a1, a2, a3, a4, a5)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
extern "C" fn sigsys_handler(
    _sig: libc::c_int,
    _info: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
) {
    // SAFETY: Linux invokes a SA_SIGINFO handler with `ucontext` pointing to a
    // writable `ucontext_t`. This handler only uses async-signal-safe raw
    // syscalls and edits the saved register frame before returning.
    unsafe {
        let ctx = &mut *(ucontext as *mut libc::ucontext_t);
        let regs = &mut ctx.uc_mcontext.gregs;
        let nr = regs[libc::REG_RAX as usize] as libc::c_long;

        if crate::is_in_rsched() {
            let ret = raw_syscall6(
                nr,
                regs[libc::REG_RDI as usize] as libc::c_long,
                regs[libc::REG_RSI as usize] as libc::c_long,
                regs[libc::REG_RDX as usize] as libc::c_long,
                regs[libc::REG_R10 as usize] as libc::c_long,
                regs[libc::REG_R8 as usize] as libc::c_long,
                regs[libc::REG_R9 as usize] as libc::c_long,
            );
            regs[libc::REG_RAX as usize] = ret;
            return;
        }

        const MSG: &[u8] = b"rsched: intercepted raw futex/clone syscall outside rsched\n";
        let _ = raw_syscall6(
            libc::SYS_write as libc::c_long,
            libc::STDERR_FILENO as libc::c_long,
            MSG.as_ptr() as libc::c_long,
            MSG.len() as libc::c_long,
            0,
            0,
            0,
        );
        let _ = raw_syscall6(libc::SYS_exit_group as libc::c_long, 101, 0, 0, 0, 0, 0);
        core::hint::unreachable_unchecked();
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) unsafe fn install_tripwire_if_requested() {
    if std::env::var("RSCHED_SECCOMP").map_or(true, |v| v != "1") {
        return;
    }

    install_sigsys_handler();
    install_filter();
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) unsafe fn install_tripwire_if_requested() {
    if std::env::var("RSCHED_SECCOMP").map_or(false, |v| v == "1") {
        panic!("rsched: RSCHED_SECCOMP=1 is only implemented on linux x86_64");
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn install_sigsys_handler() {
    let mut sa: libc::sigaction = core::mem::zeroed();
    sa.sa_sigaction = sigsys_handler as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO;
    libc::sigemptyset(&mut sa.sa_mask);
    let r = libc::sigaction(libc::SIGSYS, &sa, core::ptr::null_mut());
    assert_eq!(r, 0, "rsched: sigaction(SIGSYS) failed");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn install_filter() {
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    const SECCOMP_DATA_NR: u32 = 0;
    const SECCOMP_DATA_ARCH: u32 = 4;
    const SECCOMP_DATA_IP_LO: u32 = 8;
    const SECCOMP_DATA_IP_HI: u32 = 12;
    const SYS_CLONE3_X86_64: u32 = 435;

    fn stmt(code: u16, k: u32) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        }
    }
    fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
        libc::sock_filter { code, jt, jf, k }
    }

    let syscall_ip = &rsched_internal_syscall6_insn as *const u8 as usize as u64 + 2;
    let syscall_ip_lo = syscall_ip as u32;
    let syscall_ip_hi = (syscall_ip >> 32) as u32;
    let trap_clone = std::env::var("RSCHED_SECCOMP_TRAP_CLONE").is_ok_and(|v| v == "1");
    let clone_syscall = if trap_clone {
        libc::SYS_clone as u32
    } else {
        u32::MAX
    };
    let clone3_syscall = if trap_clone {
        SYS_CLONE3_X86_64
    } else {
        u32::MAX - 1
    };

    let mut filter = [
        stmt(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_ARCH,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            AUDIT_ARCH_X86_64,
            1,
            0,
        ),
        stmt(
            (libc::BPF_RET | libc::BPF_K) as u16,
            SECCOMP_RET_KILL_PROCESS,
        ),
        stmt(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_NR,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            libc::SYS_futex as u32,
            3,
            0,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            clone_syscall,
            2,
            0,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            clone3_syscall,
            1,
            0,
        ),
        stmt((libc::BPF_RET | libc::BPF_K) as u16, SECCOMP_RET_ALLOW),
        stmt(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_IP_LO,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            syscall_ip_lo,
            0,
            2,
        ),
        stmt(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_IP_HI,
        ),
        jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            syscall_ip_hi,
            1,
            0,
        ),
        stmt((libc::BPF_RET | libc::BPF_K) as u16, SECCOMP_RET_TRAP),
        stmt((libc::BPF_RET | libc::BPF_K) as u16, SECCOMP_RET_ALLOW),
    ];
    let mut prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };

    let r = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    assert_eq!(r, 0, "rsched: prctl(PR_SET_NO_NEW_PRIVS) failed");
    let r = libc::prctl(
        libc::PR_SET_SECCOMP,
        libc::SECCOMP_MODE_FILTER,
        &mut prog as *mut libc::sock_fprog,
    );
    assert_eq!(r, 0, "rsched: prctl(PR_SET_SECCOMP) failed");
}
