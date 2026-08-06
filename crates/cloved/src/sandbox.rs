//! Layer-2 self-restriction — the pledge/unveil doctrine on Linux mechanisms
//! (`docs/SCOPE.md` §5).
//!
//! Once the daemon has finished initialising — config read, data directory
//! created, token loaded, registry opened, control socket bound — it no longer
//! needs most of the process it started as. It does not exec, it does not
//! ptrace, it does not load modules, it does not touch the filesystem outside
//! its own data directory, and the only address it dials is the SAM bridge.
//! [`enter_post_init`] takes those capabilities away from itself. That call is
//! the single phase hook: an OpenBSD port drops `pledge(2)`/`unveil(2)` in at
//! exactly this point and deletes nothing else.
//!
//! Two mechanisms:
//!
//! - **Landlock** confines the filesystem to the paths handed in, each to the
//!   rights its [`Role`] actually needs rather than to all sixteen; confines
//!   outbound TCP to the SAM port (ABI 4); and forbids reaching outside the
//!   domain by abstract unix socket or signal (ABI 6). Where the kernel is newer
//!   still it also forbids connecting to any *pathname* unix socket (ABI 9).
//!   ABI 6 — Linux 6.12, the floor in `docs/SCOPE.md` §0 — is a hard
//!   requirement; only the ABI 9 addition is best-effort.
//! - **seccomp** installs an *allowlist* over the syscalls in `ALLOWED`:
//!   anything not on it returns `ENOSYS`. Four of them are narrowed by
//!   argument as well; see `argument_restricted`.
//!
//! The daemon was run under `strace`
//! against a SAM bridge complete enough to bring the whole network path up —
//! session, forwarded listener, naming lookup, tracker announce, inbound peer —
//! and driven through every API operation, and the trace was split at the
//! `seccomp(2)` call that installs the filter. Everything the daemon used after
//! that point is on the list, plus the entries reasoning adds for paths a trace
//! cannot reach and for the several spellings a libc may choose (see the groups
//! below). The procedure is written down in `docs/SCOPE.md` §5 so it can be
//! repeated when the daemon learns to do something new.
//!
//! Neither is allowed to fail startup. Every failure path here degrades to a
//! log line, per the no-layer-assumes-another rule: Layer 2 being absent must
//! not be mistaken for Layers 1 and 3 being absent.
//!
//! Restriction is applied **before any thread is spawned**. A Landlock domain
//! covers the calling thread and everything it goes on to create; sibling
//! threads that already exist are untouched. Since the supervisor and persist
//! threads are the ones doing file I/O, restricting after they start would
//! leave the holes it is supposed to close.

use std::path::Path;

/// What the daemon still needs after initialisation.
pub(crate) struct Limits<'a> {
    /// The directories it may still touch, each with what it is *for*.
    ///
    /// A role rather than an access set, so the caller names paths and this
    /// module decides rights. Landlock vocabulary stays on this side of the
    /// boundary, and the two questions — "which directory is this?" and "what
    /// may that kind of directory do?" — stay in the file that can answer each.
    pub(crate) paths: &'a [(&'a Path, Role)],
    /// The one TCP port it may connect to: the SAM bridge. `None` when the
    /// port could not be determined, in which case outbound TCP is left alone
    /// rather than guessed at — a wrong guess is a daemon that cannot reach
    /// its router.
    pub(crate) connect_tcp: Option<u16>,
}

/// What a directory is for, which is what decides the rights it gets.
///
/// Every writable path used to receive `AccessFs::from_all()` — all sixteen
/// filesystem rights, including `Execute`, `MakeSym`, `MakeBlock` and `Refer`.
/// The ABI has allowed finer grants since ABI 1; the code simply never asked for
/// one, so the sandbox was as wide as the widest thing any of its paths needed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Role {
    /// The data directory: torrents, resume files, the token, the destination
    /// key, and the downloads themselves. Read, write, create and delete both
    /// files and directories, and truncate.
    State,
    /// The watch directory: list it, read what is dropped in, and rename a file
    /// within it once taken. Nothing else — no directories are created here and
    /// nothing is executed.
    Watch,
}

/// What the two mechanisms actually came to.
///
/// Booleans as well as prose, so `sandbox require` and `GET /v1/status` do not
/// have to read the message to know what happened.
pub(crate) struct Verdict {
    /// The operator-facing summary, without the `sandbox:` prefix the startup
    /// line adds.
    pub(crate) line: String,
    /// Landlock is enforcing. Partial enforcement counts: only the ABI 9
    /// addition is best-effort, and the ABI 6 floor cannot be partially met.
    pub(crate) landlock: bool,
    /// The seccomp filter is installed.
    pub(crate) seccomp: bool,
}

impl Verdict {
    /// Whether both mechanisms applied, which is what `sandbox require` means.
    pub(crate) fn is_complete(&self) -> bool {
        self.landlock && self.seccomp
    }
}

/// Drop the capabilities the daemon no longer needs, and report what happened.
///
/// Never fails: an unavailable mechanism is reported, not raised. Whether that
/// is enough is the caller's decision — see `sandbox require`.
pub(crate) fn enter_post_init(limits: &Limits) -> Verdict {
    let (landlock, fs) = landlock_restrict(limits);
    let (seccomp, syscalls) = seccomp_restrict();
    Verdict {
        line: format!("{fs}; {syscalls}"),
        landlock,
        seccomp,
    }
}

#[cfg(not(target_os = "linux"))]
fn landlock_restrict(_limits: &Limits) -> (bool, String) {
    (false, "landlock unavailable (not Linux)".to_owned())
}

/// Confine the filesystem, outbound TCP, and — where the kernel has it — the
/// unix sockets the daemon may connect to.
///
/// **Two tiers, deliberately.** `REQUIRED` is ABI 6, which is Linux 6.12, which
/// is the floor `docs/SCOPE.md` §0 commits to — so it is asked for as a
/// `HardRequirement`. A kernel that cannot provide it is below the documented
/// baseline and should say so rather than run half-confined and report success.
/// `AccessFs::ResolveUnix` is ABI 9 (Linux 7.1) and is asked for `BestEffort`,
/// because it is a bonus rather than a promise.
///
/// That split is what makes the returned status mean something. The old code
/// targeted ABI 5 best-effort throughout, so `PartiallyEnforced` covered
/// everything from "one nicety missing" to "barely any of this applied" and the
/// message could only shrug at "kernel supports an older ABI". Here the required
/// tier cannot be partial — it either applies or errors — so a partial result has
/// exactly one possible cause, and the message can name it.
///
/// The rights themselves come from [`Role`], per path. Note that
/// `handle_access` still covers *every* filesystem right: what is handled is
/// what Landlock mediates, and anything unhandled is permitted everywhere. The
/// narrowing belongs in the per-path rules, never in the handled set.
#[cfg(target_os = "linux")]
#[allow(clippy::needless_pass_by_value)] // the landlock builders consume self
fn landlock_restrict(limits: &Limits) -> (bool, String) {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
    };

    /// The floor: ABI 6 is Linux 6.12 (SCOPE §0). Required, not negotiated.
    const REQUIRED: ABI = ABI::V6;

    let restrict = || -> Result<RulesetStatus, landlock::RulesetError> {
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(REQUIRED))?;
        if limits.connect_tcp.is_some() {
            ruleset = ruleset.handle_access(AccessNet::ConnectTcp)?;
        }
        // ABI 6, so guaranteed at the floor. Neither is something the daemon
        // does: it serves its CLI over a *pathname* socket, and signals nothing.
        ruleset = ruleset.scope(Scope::AbstractUnixSocket | Scope::Signal)?;
        // ABI 9 (Linux 7.1), and handled without ever being granted: the daemon
        // connects to no unix socket at all — it binds one before this point and
        // only accepts on it afterwards. Handling it therefore costs nothing and
        // takes away reaching a container runtime's socket, a message bus, an
        // agent, or any other daemon's control socket. `Scope::AbstractUnixSocket`
        // above is the abstract-namespace half of the same door; this is the
        // pathname half.
        //
        // `BestEffort`, so a kernel below ABI 9 declines it and carries on rather
        // than failing the whole ruleset.
        ruleset = ruleset
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::ResolveUnix)?;

        let mut created = ruleset.create()?;
        // One call per path rather than one per role: `path_beneath_rules` is
        // what turns a path into an opened `PathFd` and folds the open error into
        // `RulesetError`, and a rule is cheap enough that grouping would only
        // save allocations nobody is counting.
        for (path, role) in limits.paths {
            created = created.add_rules(path_beneath_rules([*path], role.rights()))?;
        }
        if let Some(port) = limits.connect_tcp {
            created = created.add_rule(NetPort::new(port, AccessNet::ConnectTcp))?;
        }
        Ok(created.restrict_self()?.ruleset)
    };

    match restrict() {
        Ok(RulesetStatus::FullyEnforced) => (
            true,
            "landlock enforced (unix-socket connects denied too)".to_owned(),
        ),
        // The required tier is a hard requirement, so nothing in it can be
        // missing here: the only access asked for best-effort is `ResolveUnix`,
        // and the only kernels that decline it are those below ABI 9.
        Ok(RulesetStatus::PartiallyEnforced) => (
            true,
            "landlock enforced; unix-socket connects unrestricted (kernel below ABI 9)".to_owned(),
        ),
        Ok(RulesetStatus::NotEnforced) => (
            false,
            "landlock not applied (the kernel accepted the ruleset and enforced nothing)"
                .to_owned(),
        ),
        Err(e) => (false, why_landlock_is_unavailable(&e)),
    }
}

/// Say *why* the ruleset could not be applied, having asked the kernel.
///
/// `RestrictSelf` reports the running kernel's Landlock status without building
/// a domain; `no_new_privs(false)` keeps the probe from touching the process.
/// Blaming the kernel version for all three causes was wrong on two of them.
#[cfg(target_os = "linux")]
fn why_landlock_is_unavailable(e: &landlock::RulesetError) -> String {
    use landlock::{ABI, LandlockStatus, RestrictSelf};

    match RestrictSelf::default().no_new_privs(false).apply() {
        Ok(status) => match status.landlock {
            // No mention of the 6.12 floor: a 6.18 kernel built without
            // `CONFIG_SECURITY_LANDLOCK` lands here too.
            LandlockStatus::NotImplemented => "landlock unavailable: this kernel has no Landlock \
                 at all (CONFIG_SECURITY_LANDLOCK is off, or the kernel predates it) — clove \
                 expects it, see docs/SCOPE.md §0"
                .to_owned(),
            LandlockStatus::NotEnabled => "landlock unavailable: built into this kernel but not \
                 enabled — add it to the lsm= boot parameter or CONFIG_LSM"
                .to_owned(),
            // The genuine too-old case; name the ABI found, not the one wanted.
            LandlockStatus::Available { effective_abi, .. } => format!(
                "landlock unavailable: this kernel offers ABI {}, below the {} clove requires \
                 (Linux 6.12; see docs/SCOPE.md §0)",
                effective_abi as u32,
                ABI::V6 as u32
            ),
        },
        // Both failed; report both rather than choose which to believe.
        Err(probe) => format!("landlock unavailable: {e} (probing why also failed: {probe})"),
    }
}

impl Role {
    /// The filesystem rights this kind of directory actually needs.
    ///
    /// Derived from what the daemon does, not from a convenient superset. What is
    /// *absent* is the point, so the notable omissions are named in place.
    #[cfg(target_os = "linux")]
    fn rights(self) -> landlock::BitFlags<landlock::AccessFs> {
        use landlock::{AccessFs, make_bitflags};
        match self {
            // Everything a state directory and a download tree need between
            // them. `Truncate` covers both `O_TRUNC` on an atomic-write temp file
            // and `set_len` under `preallocate yes`.
            //
            // Absent, and each for a reason: `Execute`, so bytes a peer sent
            // cannot be run or mapped executable — which `seccomp`'s `execve`
            // denial does not cover, since it says nothing about
            // `mmap(PROT_EXEC)`. `MakeSym`, so the daemon cannot create the
            // symlink that `storage`'s `O_NOFOLLOW` walk exists to refuse.
            // `MakeChar`, `MakeBlock`, `MakeFifo` and `MakeSock`, none of which a
            // torrent is made of. `IoctlDev`, since there are no device files
            // here. And `Refer`: every rename in the daemon is within one
            // directory — the atomic-write temp, the resume file, the identity —
            // and Landlock only requires `Refer` to link or rename *across*
            // directories.
            Role::State => make_bitflags!(AccessFs::{
                ReadFile | WriteFile | ReadDir | MakeReg | MakeDir
                    | RemoveFile | RemoveDir | Truncate
            }),
            // List the directory, read what was dropped in, and rename it to
            // `.added` or `.rejected` in place — which needs `MakeReg` for the
            // new name and `RemoveFile` for the old one, both in this directory,
            // and so again no `Refer`. No `MakeDir`: nothing here creates one.
            Role::Watch => make_bitflags!(AccessFs::{
                ReadDir | ReadFile | MakeReg | RemoveFile
            }),
        }
    }
}

// The seccomp side is gated on the architectures `seccompiler` can emit a
// filter for. Everything it needs lives in the module, so the platform
// condition is spelled exactly twice: once here, once inverted.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
use seccomp::restrict as seccomp_restrict;

#[cfg(not(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
)))]
fn seccomp_restrict() -> (bool, String) {
    (
        false,
        "seccomp unavailable (unsupported platform)".to_owned(),
    )
}

#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
mod seccomp {
    use std::collections::BTreeMap;

    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };

    /// Everything the daemon still needs after initialisation *whatever
    /// arguments it passes*, named rather than numbered so the list reads as the
    /// policy it is. Anything absent from both this and `argument_restricted`
    /// returns `ENOSYS`.
    ///
    /// Portable across the three architectures this module is compiled for; the
    /// legacy non-`at` spellings that exist only on x86-64 are in
    /// [`ALLOWED_LEGACY`] below, because naming one here would not compile on
    /// `aarch64`.
    ///
    /// Grouped by what the daemon is *doing*, since that is the question asked
    /// when this list has to change. Every entry was either observed in the
    /// measured trace or is justified in place.
    const ALLOWED: &[libc::c_long] = &[
        // --- Threads. Thread-per-peer (Q5), so this is the busiest group.
        // `clone` as well as `clone3`: musl uses it, and glibc falls back to it
        // when `clone3` is unavailable — which, under a filter that denied it,
        // would be a daemon that cannot start a peer.
        libc::SYS_clone,
        libc::SYS_clone3,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_gettid,
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_yield,
        libc::SYS_exit,
        // Not in the trace only because the daemon was killed rather than asked
        // to leave; a process that cannot exit_group cannot shut down.
        libc::SYS_exit_group,
        // --- Futexes: every mutex and condvar in the engine.
        libc::SYS_futex,
        // Newer glibc reaches for this instead. A wait primitive, and denying it
        // would wedge whatever was waiting.
        libc::SYS_futex_waitv,
        // --- Memory. The allocator's, mostly. `mmap` and `mprotect` are not
        // here: they are allowed by argument, in `argument_restricted`.
        libc::SYS_munmap,
        libc::SYS_madvise,
        libc::SYS_mremap,
        // glibc's main arena grows with `brk`, not `mmap`. It appeared only
        // before the filter in the trace because the arena was already large
        // enough — under a heavier torrent it is called after, and an `EPERM`
        // here is an allocation failure and an abort.
        libc::SYS_brk,
        // --- Signals. Rust installs handlers (stack-overflow detection among
        // them) and every thread gets an alternate stack.
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        // Returning from a handler. Denying it turns any delivered signal into a
        // second, fatal one.
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        // `abort()` raises through this. Denying it does not prevent an abort,
        // it only makes one incomprehensible.
        libc::SYS_tgkill,
        // The kernel's own resumption of a blocking call a signal interrupted.
        // Not something userspace calls, and filtered all the same.
        libc::SYS_restart_syscall,
        // --- Time. Normally the vDSO answers these without a syscall, which is
        // why the trace does not show them; the vDSO is not guaranteed, and
        // `Instant::now()` is on every hot path in the engine.
        libc::SYS_clock_gettime,
        libc::SYS_gettimeofday,
        libc::SYS_clock_nanosleep,
        // --- Files. The data directory: state, resume files, torrent payloads.
        libc::SYS_openat,
        libc::SYS_close,
        libc::SYS_read,
        libc::SYS_write,
        // Positioned I/O is how storage reads and writes blocks without a lock.
        // `pwrite64` is absent from the trace only because no block ever
        // arrived in it — it is the single most important entry here.
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_lseek,
        libc::SYS_fstat,
        libc::SYS_statx,
        // What glibc used for the `stat` family before 2.33, and what aarch64
        // uses regardless.
        libc::SYS_newfstatat,
        // `File::set_len`, for `preallocate yes`. Not exercised by the trace
        // because preallocation is off by default.
        libc::SYS_ftruncate,
        libc::SYS_fsync,
        libc::SYS_getdents64,
        libc::SYS_fcntl,
        // `ioctl` is not here either — see `argument_restricted`.
        // The `at`-relative forms: what `storage`'s `openat` walk uses directly,
        // and what glibc uses for the legacy names on every architecture that
        // has no legacy names. `mkdirat` is absent from the trace because the
        // measured torrent had a single path component and so no directory to
        // create.
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_renameat2,
        // --- Sockets. The SAM bridge, the forwarded inbound listener, and the
        // control socket. `socket` itself is restricted by family below.
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept4,
        libc::SYS_getsockname,
        libc::SYS_setsockopt,
        libc::SYS_getsockopt,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        // Other spellings of the same two operations, depending on libc and
        // path.
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        // Ending a connection in both directions — how a peer's parked reader is
        // reclaimed, and how a SAM session is closed. Absent from the trace for
        // the same reason `exit_group` is.
        libc::SYS_shutdown,
        // `connect_timeout` waits on this. `ppoll` because aarch64 and riscv64
        // have no `poll` syscall at all, so glibc's `poll()` becomes one there.
        libc::SYS_ppoll,
        // --- Odds and ends.
        libc::SYS_getpid,
        // The API token, the peer id, and the reconnect jitter.
        libc::SYS_getrandom,
    ];

    /// The legacy, non-`at` spellings glibc uses on x86-64 — and *only* there.
    ///
    /// `aarch64` and `riscv64` were built without them, so `libc` does not define
    /// the constants and naming one unconditionally is a compile error rather
    /// than a portability wart. On those architectures glibc uses the `at` forms
    /// in [`ALLOWED`], which is why this list has no counterpart.
    #[cfg(target_arch = "x86_64")]
    const ALLOWED_LEGACY: &[libc::c_long] = &[
        libc::SYS_open,
        libc::SYS_stat,
        libc::SYS_poll,
        libc::SYS_mkdir,
        libc::SYS_rmdir,
        libc::SYS_unlink,
        libc::SYS_rename,
    ];

    #[cfg(not(target_arch = "x86_64"))]
    const ALLOWED_LEGACY: &[libc::c_long] = &[];

    /// Masks `SOCK_CLOEXEC`/`SOCK_NONBLOCK` off `socket(2)`'s type argument.
    /// glibc spells it `SOCK_TYPE_MASK`; `libc` does not re-export it.
    const SOCK_TYPE_MASK: u64 = 0xf;

    /// One condition of the form `(arg & mask) == value`. Fails closed: a
    /// constant that did not fit becomes `u64::MAX`, which nothing equals.
    fn masked(
        arg: u8,
        len: SeccompCmpArgLen,
        mask: u64,
        value: libc::c_int,
    ) -> Result<SeccompCondition, seccompiler::Error> {
        Ok(SeccompCondition::new(
            arg,
            len,
            SeccompCmpOp::MaskedEq(mask),
            value.try_into().unwrap_or(u64::MAX),
        )?)
    }

    /// The calls the daemon needs, but only with certain arguments.
    ///
    /// Conditions within a rule are combined with AND, rules for one syscall
    /// with OR, and the mismatch action refuses — so each rule reads "allowed
    /// only when this holds". Measured from the same traced run as [`ALLOWED`].
    fn argument_restricted() -> Result<Vec<(libc::c_long, Vec<SeccompRule>)>, seccompiler::Error> {
        use SeccompCmpArgLen::{Dword, Qword};

        Ok(vec![
            // 6 of 6 sockets in the trace are `AF_INET, SOCK_STREAM`. The
            // type half is not redundant with Landlock: `ConnectTcp` says
            // nothing about UDP, so this is where Layer 2 closes datagram
            // egress (`docs/SCOPE.md` §5).
            (
                libc::SYS_socket,
                vec![SeccompRule::new(vec![
                    SeccompCondition::new(
                        0,
                        Dword,
                        SeccompCmpOp::Eq,
                        libc::AF_INET.try_into().unwrap_or(u64::MAX),
                    )?,
                    masked(1, Dword, SOCK_TYPE_MASK, libc::SOCK_STREAM)?,
                ])?],
            ),
            // All 8 `ioctl` calls in the trace set a socket non-blocking.
            // Narrowed because `SIOCGIFCONF` is reachable the same way, and
            // Landlock's `IoctlDev` covers device files, not socket ioctls.
            // `Qword` because the request is an `unsigned long`.
            (
                libc::SYS_ioctl,
                vec![SeccompRule::new(vec![SeccompCondition::new(
                    1,
                    Qword,
                    SeccompCmpOp::Eq,
                    libc::FIONBIO,
                )?])?],
            ),
            // W^X, the semantics of `MemoryDenyWriteExecute=yes`. Not the
            // stricter "no `PROT_EXEC` at all", which would bet that no library
            // is ever loaded lazily — the unwinder among them, on a path only a
            // panic in a peer thread reaches. `pkey_mprotect` and `shmat` are
            // on neither list.
            (
                libc::SYS_mmap,
                vec![
                    SeccompRule::new(vec![masked(2, Dword, exec_mask(), 0)?])?,
                    SeccompRule::new(vec![masked(2, Dword, write_mask(), 0)?])?,
                ],
            ),
            (
                libc::SYS_mprotect,
                vec![SeccompRule::new(vec![masked(2, Dword, exec_mask(), 0)?])?],
            ),
        ])
    }

    /// `PROT_EXEC` as a mask, failing closed like [`masked`].
    fn exec_mask() -> u64 {
        libc::PROT_EXEC.try_into().unwrap_or(u64::MAX)
    }

    /// `PROT_WRITE` as a mask, on the same terms as [`exec_mask`].
    fn write_mask() -> u64 {
        libc::PROT_WRITE.try_into().unwrap_or(u64::MAX)
    }

    /// Build the program: everything in [`ALLOWED`] (plus [`ALLOWED_LEGACY`])
    /// whatever its arguments, everything in [`argument_restricted`] only with
    /// the arguments named there. Everything else is `ENOSYS`.
    ///
    /// What this denies is everything nobody thought to allow, which includes —
    /// and is no longer limited to — the groups the old deny list named: exec,
    /// `ptrace`, module and BPF loading, mount and namespace manipulation,
    /// kernel replacement, the keyring and tracing interfaces, and `io_uring`,
    /// whose absence from that list was the reason for this change.
    fn build() -> Result<Vec<seccompiler::sock_filter>, seccompiler::Error> {
        let arch = TargetArch::try_from(std::env::consts::ARCH)?;
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        for &nr in ALLOWED.iter().chain(ALLOWED_LEGACY) {
            // An empty rule vector matches the syscall whatever its arguments.
            rules.insert(nr, Vec::new());
        }
        for (nr, conditional) in argument_restricted()? {
            // In both lists is a bug in the lists, not a hole: this runs
            // second, so the narrow rule wins and refuses more than intended.
            debug_assert!(
                !rules.contains_key(&nr),
                "{nr} is in both ALLOWED and argument_restricted"
            );
            rules.insert(nr, conditional);
        }
        let filter = SeccompFilter::new(
            rules,
            // Mismatch — not on the list, or on it with the wrong arguments.
            SeccompAction::Errno(u32::try_from(libc::ENOSYS).unwrap_or(1)),
            // Match is allowed.
            SeccompAction::Allow,
            arch,
        )?;
        Ok(filter.try_into()?)
    }

    /// Install the deny filter on every thread, and say what happened.
    ///
    /// An errno rather than `SIGSYS`, and `ENOSYS` rather than `EPERM`: it is
    /// what a libc's fallback paths are written for — glibc falls back from
    /// `clone3` to `clone` on `ENOSYS` alone. Neither is logged anywhere;
    /// `ci/router.sh --trace` is what turns a refusal into an answer.
    pub(super) fn restrict() -> (bool, String) {
        match build() {
            Ok(program) => match seccompiler::apply_filter_all_threads(&program) {
                Ok(()) => (true, "seccomp filter installed".to_owned()),
                Err(e) => (false, format!("seccomp filter not installed ({e})")),
            },
            Err(e) => (false, format!("seccomp filter not built ({e})")),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The rights each role withholds, asserted as the property rather than as
    /// a list compared against itself.
    ///
    /// The part of the policy checkable without a kernel that has Landlock.
    /// Every absence below is load-bearing and easy to undo with `from_all()`.
    #[test]
    fn the_narrowed_roles_withhold_what_they_should() {
        use landlock::AccessFs;

        for (role, name) in [(Role::State, "state"), (Role::Watch, "watch")] {
            let rights = role.rights();
            // Peer-supplied bytes land under the state directory, and the watch
            // directory holds files somebody else wrote. Neither may be run —
            // and unlike seccomp's `execve` denial, this also covers mapping a
            // file executable.
            assert!(
                !rights.contains(AccessFs::Execute),
                "{name} may execute files"
            );
            // The symlink escape `storage`'s O_NOFOLLOW walk exists to refuse;
            // this stops the daemon being a source of one.
            assert!(
                !rights.contains(AccessFs::MakeSym),
                "{name} may create symlinks"
            );
            // Every rename the daemon performs is within one directory, so the
            // cross-directory right is not needed — and it is the one that would
            // let a rename leave the tree.
            assert!(!rights.contains(AccessFs::Refer), "{name} may refer out");
            // A torrent is regular files and directories. Nothing here is a
            // device, a socket or a pipe.
            for forbidden in [
                AccessFs::MakeChar,
                AccessFs::MakeBlock,
                AccessFs::MakeFifo,
                AccessFs::MakeSock,
                AccessFs::IoctlDev,
            ] {
                assert!(
                    !rights.contains(forbidden),
                    "{name} may {forbidden:?}, which no torrent needs"
                );
            }
            // Connecting to a unix socket is handled and granted to nothing; a
            // role that granted it would undo that.
            assert!(
                !rights.contains(AccessFs::ResolveUnix),
                "{name} may connect to unix sockets"
            );
        }

        // And the positive half, so this cannot pass by granting nothing: the
        // state directory must still be able to do the daemon's actual work.
        let state = Role::State.rights();
        for needed in [
            AccessFs::ReadFile,
            AccessFs::WriteFile,
            AccessFs::ReadDir,
            AccessFs::MakeReg,
            AccessFs::MakeDir,
            AccessFs::RemoveFile,
            AccessFs::RemoveDir,
            AccessFs::Truncate,
        ] {
            assert!(state.contains(needed), "state cannot {needed:?}");
        }
        // The watch directory is read-and-rename only: it never creates a
        // directory, which is what separates it from the state role.
        let watch = Role::Watch.rights();
        assert!(!watch.contains(AccessFs::MakeDir), "watch may create dirs");
        assert!(
            !watch.contains(AccessFs::WriteFile),
            "watch may write files"
        );
        for needed in [
            AccessFs::ReadDir,
            AccessFs::ReadFile,
            AccessFs::MakeReg,
            AccessFs::RemoveFile,
        ] {
            assert!(watch.contains(needed), "watch cannot {needed:?}");
        }
    }

    #[test]
    fn restricts_the_filesystem_in_a_child() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let out = std::process::Command::new(exe)
            .arg("--exact")
            .arg("sandbox::tests::child_under_landlock")
            .arg("--ignored")
            .arg("--nocapture")
            .env("CLOVE_SANDBOX_CHILD", "1")
            .output();
        let Ok(out) = out else { return };
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "child failed: {text}");
        // A kernel with neither mechanism cannot demonstrate anything; say so
        // rather than passing quietly or failing on someone's old machine.
        if text.contains("SKIP") {
            eprintln!("neither landlock nor seccomp applied here; confinement not exercised");
            return;
        }
        assert!(text.contains("CONFINED"), "child did not confine: {text}");
    }

    #[test]
    #[ignore = "spawned by restricts_the_filesystem_in_a_child; confines this process"]
    fn child_under_landlock() {
        assert!(
            std::env::var_os("CLOVE_SANDBOX_CHILD").is_some(),
            "run via its parent test"
        );
        let dir = std::env::temp_dir().join(format!("clove-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let outside = std::env::temp_dir().join(format!("clove-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).expect("outside dir");

        // Bound *before* restricting, because that is when the daemon binds its
        // control socket. After this point it only ever accepts on it — which is
        // why `socket(AF_UNIX)` is not on the allowlist, and why a test that
        // bound one here would be testing something the daemon does not do.
        let control = std::os::unix::net::UnixListener::bind(dir.join("api.sock"))
            .expect("bind the control socket during initialisation");
        control
            .set_nonblocking(true)
            .expect("a listener that can be polled without a client");

        let paths: &[(&Path, Role)] = &[(dir.as_path(), Role::State)];
        let verdict = enter_post_init(&Limits {
            paths,
            connect_tcp: None,
        });

        // "landlock enforced" prefixes both the fully- and partially-enforced
        // messages, the difference between them being only whether the kernel
        // took the ABI 9 addition — so one substring covers both.
        // The booleans, not the prose meant for operators.
        let (landlocked, seccomped) = (verdict.landlock, verdict.seccomp);
        if !landlocked && !seccomped {
            println!("SKIP {}", verdict.line);
            return;
        }
        if landlocked {
            std::fs::write(dir.join("inside"), b"ok")
                .expect("write inside the permitted directory");
            assert!(
                std::fs::write(outside.join("nope"), b"no").is_err(),
                "write outside the permitted directory succeeded"
            );
        }
        if seccomped {
            // The capability drop is real, not just the paths.
            assert!(
                std::process::Command::new("/bin/true").status().is_err(),
                "exec succeeded under the filter"
            );
            // The property that made this worth changing: a syscall nobody
            // enumerated is refused. `linkat` and `symlinkat` stand in for the
            // whole class — ordinary calls, reachable from safe `std`, that the
            // daemon has no business making — and `io_uring_setup` is denied by
            // exactly the same mechanism and no other. That is the difference
            // from a deny list, where each of the three had to be thought of
            // first, and `io_uring` was not.
            //
            // `unsafe` is forbidden workspace-wide, so `io_uring_setup` cannot
            // be called from here to be asserted on directly. It does not need
            // to be: under an allowlist there is nothing special about it.
            let target = dir.join("blocks.bin");
            std::fs::write(&target, b"x").expect("a file to link to");
            assert!(
                std::fs::hard_link(&target, dir.join("hard")).is_err(),
                "linkat succeeded under the filter; the allowlist is not closed"
            );
            assert!(
                std::os::unix::fs::symlink(&target, dir.join("soft")).is_err(),
                "symlinkat succeeded under the filter"
            );
            // `AF_UNIX` and `SOCK_DGRAM` at once, either enough to refuse it.
            // Isolating the type half wants `std::net::UdpSocket`, which the
            // Layer 1 lint bans workspace-wide.
            assert!(
                std::os::unix::net::UnixDatagram::unbound().is_err(),
                "socket(AF_UNIX, SOCK_DGRAM) succeeded under the filter"
            );
            // And the daemon's own work still runs. This is the half an
            // allowlist can get wrong, and the half a deny list never could.
            exercise_the_daemons_syscalls(&dir, &control);
        }
        println!("CONFINED {}", verdict.line);
    }

    /// A representative slice of what `cloved` does after initialisation, so a
    /// missing allowlist entry fails here rather than in the field.
    ///
    /// Deliberately not a unit test of the filter's contents: a list of numbers
    /// can be compared against itself and prove nothing. What matters is whether
    /// the operations the daemon actually performs still work, so this performs
    /// them — positioned file I/O and an fsync, a directory walk, an atomic
    /// temp-then-rename, a thread, a unix socket, a loopback TCP
    /// accept/connect, a clock read, randomness, and enough allocation to make
    /// the allocator ask the kernel for more.
    #[allow(
        clippy::expect_used,
        reason = "a helper outside #[test], where the lint's test exemption does not reach; \
                  every expect here names the syscall group it is asserting, which is the \
                  whole content of the test"
    )]
    fn exercise_the_daemons_syscalls(dir: &Path, control: &std::os::unix::net::UnixListener) {
        use std::io::{Read as _, Write as _};
        use std::os::unix::fs::FileExt as _;

        // Positioned I/O, the shape `storage` uses for every block.
        let path = dir.join("blocks.bin");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open a data file");
        file.set_len(4096).expect("preallocate");
        file.write_all_at(b"block", 1024).expect("pwrite a block");
        let mut buf = [0u8; 5];
        file.read_exact_at(&mut buf, 1024).expect("pread a block");
        assert_eq!(&buf, b"block");
        file.sync_all().expect("fsync");
        drop(file);

        // Nested directories, a listing, and the temp-then-rename every state
        // write goes through.
        let nested = dir.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("create nested directories");
        let tmp = nested.join("state.tmp");
        std::fs::write(&tmp, b"state").expect("write a temp file");
        std::fs::rename(&tmp, nested.join("state")).expect("rename over the target");
        let entries = std::fs::read_dir(&nested).expect("read_dir").count();
        assert!(entries > 0, "the directory listing came back empty");
        std::fs::remove_file(nested.join("state")).expect("unlink");

        // A thread, and a channel to join it through — thread-per-peer is the
        // engine's whole shape.
        let (tx, rx) = std::sync::mpsc::sync_channel::<u64>(1);
        let worker = std::thread::spawn(move || {
            let _ =
                tx.send(u64::try_from(std::time::Instant::now().elapsed().as_nanos()).unwrap_or(0));
        });
        assert!(rx.recv().is_ok(), "a worker thread could not report back");
        worker.join().expect("join a worker thread");

        // The control socket. The daemon's *only* post-init operation on it is
        // `accept4` — it was bound during initialisation, and `socket(AF_UNIX)`,
        // `connect` to a unix path and `chmod` all happen before the filter, so
        // none of the three is on the allowlist. Two earlier versions of this
        // test failed here for precisely that reason, first reaching for
        // `socketpair(2)` and then for a post-init `chmod`; both times the test
        // was wrong and the filter stayed narrow.
        //
        // A non-blocking accept with nobody dialling must come back `WouldBlock`
        // — the syscall ran and found no connection — and not `PermissionDenied`,
        // which is what a filter missing `accept4` would return. That distinction
        // is the whole assertion.
        match control.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("accept4 on the control socket was refused: {e}"),
            Ok(_) => panic!("a connection arrived on a socket nothing dialled"),
        }
        // The daemon's only `ioctl`, and the only one the filter permits. The
        // request number is compared exactly, so a wrong constant would look
        // like a missing entry: a refusal on a rare path.
        control
            .set_nonblocking(true)
            .expect("ioctl(FIONBIO) on the control socket");

        // The SAM bridge's transport: a loopback TCP socket, bound, listened,
        // connected and accepted. Through `i2pnet`'s own API rather than
        // `std::net`, because the workspace clippy type ban is Layer 1 and a
        // test is not a reason to poke a hole in it — the first version of this
        // used `TcpListener` directly and the lint refused it, which is the lint
        // working. Landlock's `connect_tcp` is unrestricted in this child
        // (`connect_tcp: None`), so this exercises the seccomp side alone.
        let listener = i2pnet::api::ApiListener::bind_loopback_tcp("127.0.0.1:0")
            .expect("bind a loopback listener");
        let port = listener
            .local_port()
            .expect("getsockname")
            .expect("a TCP listener has a port");
        let mut client = i2pnet::api::connect_loopback_tcp(&format!("127.0.0.1:{port}"))
            .expect("connect over loopback");
        let mut server = listener.accept().expect("accept a loopback connection");
        client.write_all(b"sam").expect("write over loopback");
        let mut three = [0u8; 3];
        server.read_exact(&mut three).expect("read over loopback");
        assert_eq!(&three, b"sam");
        drop((client, server, listener));

        // Randomness (the API token and the peer id) and a clock read.
        let mut seed = [0u8; 16];
        getrandom::getrandom(&mut seed).expect("getrandom");
        assert!(seed.iter().any(|&b| b != 0), "getrandom returned nothing");
        let _ = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("read the wall clock");

        // Enough allocation to push the allocator past whatever it had mapped,
        // which is what needs `brk` or `mmap` *after* the filter is installed.
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        for i in 0..64 {
            blocks.push(vec![u8::try_from(i % 256).unwrap_or(0); 256 * 1024]);
        }
        assert_eq!(blocks.len(), 64);
        drop(blocks);

        // Sleeping is how every loop in the daemon waits.
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
