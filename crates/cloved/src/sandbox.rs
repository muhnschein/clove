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
//! Two mechanisms, both best-effort:
//!
//! - **Landlock** confines the filesystem to the paths handed in. The ABI is
//!   negotiated by the `landlock` crate's best-effort compatibility mode, so an
//!   older kernel gets the subset it understands rather than an error. Landlock
//!   ABI 4 also covers outbound TCP, which is how the SAM port becomes the only
//!   address this process may connect to.
//! - **seccomp** installs an *allowlist* over the syscalls in `ALLOWED`:
//!   anything not on it returns `EPERM`, and `socket(2)` is further restricted
//!   by address family to `AF_UNIX`/`AF_INET`/`AF_INET6`.
//!
//! This was a deny list until it was not enough. The argument for one was that
//! an allowlist over a threaded Rust process with an allocator underneath is a
//! list that breaks in the field on a kernel or libc nobody tested — which is
//! true, and is a reason to build the list carefully rather than a reason to
//! enumerate the attacker's options instead. A deny list can only ever name what
//! somebody thought of, and `io_uring` is the standing proof: it submits
//! operations that kernel workers perform *without* re-checking the submitter's
//! filter, so three syscall numbers absent from a deny list hand back most of
//! what the rest of it took away. An allowlist has no such gap by construction —
//! a capability nobody enumerated is denied rather than granted.
//!
//! `ALLOWED` is not a guess. It was measured: the daemon was run under `strace`
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
    /// Directories it may read and write beneath (state, downloads, the
    /// control socket's directory so the socket can be unlinked at exit).
    pub(crate) read_write: &'a [&'a Path],
    /// Paths it may only read.
    pub(crate) read_only: &'a [&'a Path],
    /// The one TCP port it may connect to: the SAM bridge. `None` when the
    /// port could not be determined, in which case outbound TCP is left alone
    /// rather than guessed at — a wrong guess is a daemon that cannot reach
    /// its router.
    pub(crate) connect_tcp: Option<u16>,
}

/// Drop the capabilities the daemon no longer needs, and describe what
/// actually happened in one line for the operator.
///
/// Never fails: an unavailable or partial mechanism is reported, not raised.
pub(crate) fn enter_post_init(limits: &Limits) -> String {
    let fs = landlock_restrict(limits);
    let syscalls = seccomp_restrict();
    format!("sandbox: {fs}; {syscalls}")
}

#[cfg(not(target_os = "linux"))]
fn landlock_restrict(_limits: &Limits) -> String {
    "landlock unavailable (not Linux)".to_owned()
}

/// Confine the filesystem — and, on ABI 4 and up, outbound TCP.
///
/// The access sets are written against ABI 5; best-effort compatibility trims
/// them to what the running kernel supports and surfaces the result in the
/// returned status, which is why the caller logs it instead of ignoring it.
#[cfg(target_os = "linux")]
#[allow(clippy::needless_pass_by_value)] // the landlock builders consume self
fn landlock_restrict(limits: &Limits) -> String {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, NetPort, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, Scope, path_beneath_rules,
    };

    const ABI_TARGET: ABI = ABI::V5;

    let restrict = || -> Result<RulesetStatus, landlock::RulesetError> {
        let mut ruleset = Ruleset::default().handle_access(AccessFs::from_all(ABI_TARGET))?;
        if limits.connect_tcp.is_some() {
            ruleset = ruleset.handle_access(AccessNet::ConnectTcp)?;
        }
        // Ignored below ABI 6. Neither is something the daemon does: it talks
        // to its CLI over a pathname socket, and signals nothing.
        ruleset = ruleset.scope(Scope::AbstractUnixSocket | Scope::Signal)?;

        let mut created = ruleset
            .create()?
            .add_rules(path_beneath_rules(
                limits.read_write,
                AccessFs::from_all(ABI_TARGET),
            ))?
            .add_rules(path_beneath_rules(
                limits.read_only,
                AccessFs::from_read(ABI_TARGET),
            ))?;
        if let Some(port) = limits.connect_tcp {
            created = created.add_rule(NetPort::new(port, AccessNet::ConnectTcp))?;
        }
        Ok(created.restrict_self()?.ruleset)
    };

    match restrict() {
        Ok(RulesetStatus::FullyEnforced) => "landlock enforced".to_owned(),
        Ok(RulesetStatus::PartiallyEnforced) => {
            "landlock partially enforced (kernel supports an older ABI)".to_owned()
        }
        Ok(RulesetStatus::NotEnforced) => {
            "landlock unavailable (kernel too old or disabled)".to_owned()
        }
        Err(e) => format!("landlock not applied ({e})"),
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
fn seccomp_restrict() -> String {
    "seccomp unavailable (unsupported platform)".to_owned()
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

    /// Everything the daemon still needs after initialisation, named rather than
    /// numbered so the list reads as the policy it is. Anything absent returns
    /// `EPERM`.
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
        // --- Memory. The allocator's, mostly.
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
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
        libc::SYS_ioctl,
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

    /// Build the program: everything in [`ALLOWED`] (plus [`ALLOWED_LEGACY`])
    /// unconditionally, and `socket(2)` only for the three address families the
    /// client speaks. Everything else is `EPERM`.
    ///
    /// What this denies is now everything nobody thought to allow, which
    /// includes — and is no longer limited to — the groups the old deny list
    /// named: exec, `ptrace`, module and BPF loading, mount and namespace
    /// manipulation, kernel replacement, the keyring and tracing interfaces, and
    /// `io_uring`, whose absence from that list was the reason for this change.
    fn build() -> Result<Vec<seccompiler::sock_filter>, seccompiler::Error> {
        let arch = TargetArch::try_from(std::env::consts::ARCH)?;
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        for &nr in ALLOWED.iter().chain(ALLOWED_LEGACY) {
            // An empty rule vector matches the syscall whatever its arguments.
            rules.insert(nr, Vec::new());
        }
        // Conditions within one rule are ANDed; the rules for one syscall are
        // ORed. So three one-condition rules read "the family is one of these
        // three" — and, since the match action is `Allow` and the default is
        // `EPERM`, any other family is refused.
        let family = |af: libc::c_int| {
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                af.try_into().unwrap_or(u64::MAX),
            )?])
        };
        rules.insert(
            libc::SYS_socket,
            vec![
                family(libc::AF_UNIX)?,
                family(libc::AF_INET)?,
                family(libc::AF_INET6)?,
            ],
        );
        let filter = SeccompFilter::new(
            rules,
            // Mismatch — not on the list — is refused.
            SeccompAction::Errno(u32::try_from(libc::EPERM).unwrap_or(1)),
            // Match is allowed.
            SeccompAction::Allow,
            arch,
        )?;
        Ok(filter.try_into()?)
    }

    /// Install the deny filter on every thread, and say what happened.
    ///
    /// `EPERM`, not `SIGSYS`: the filter is a backstop behind Layers 1 and 3,
    /// and a refused syscall that shows up in a log is worth more than a
    /// corpse. Nothing on the list has a legitimate caller in this process, so
    /// none of it is a call that "might" happen and must be tolerated.
    pub(super) fn restrict() -> String {
        match build() {
            Ok(program) => match seccompiler::apply_filter_all_threads(&program) {
                Ok(()) => "seccomp filter installed".to_owned(),
                Err(e) => format!("seccomp filter not installed ({e})"),
            },
            Err(e) => format!("seccomp filter not built ({e})"),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The point of the exercise: after `enter_post_init`, the daemon cannot
    /// exec and cannot write outside the directories it was given, while a
    /// write inside them still works.
    ///
    /// Restriction is irreversible and process-wide, so this runs in a child
    /// process — `cargo test` would otherwise confine the whole test binary
    /// and every later test with it. Each mechanism is asserted only when the
    /// running kernel actually applied it, so the test proves confinement
    /// where it exists instead of failing on a kernel that has neither.
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

        let allowed: &[&Path] = &[&dir];
        let line = enter_post_init(&Limits {
            read_write: allowed,
            read_only: &[],
            connect_tcp: None,
        });

        let landlocked = line.contains("landlock enforced") || line.contains("partially enforced");
        let seccomped = line.contains("seccomp filter installed");
        if !landlocked && !seccomped {
            println!("SKIP {line}");
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
            // And the daemon's own work still runs. This is the half an
            // allowlist can get wrong, and the half a deny list never could.
            exercise_the_daemons_syscalls(&dir);
        }
        println!("CONFINED {line}");
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
    fn exercise_the_daemons_syscalls(dir: &Path) {
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

        // The control socket's transport, in the shape the daemon really uses
        // it: bind a pathname socket, connect, accept. Deliberately *not*
        // `UnixStream::pair`, which is `socketpair(2)` — a call `cloved` never
        // makes, so it is not on the allowlist, and the first version of this
        // test failed here for exactly that reason. The test's job is to
        // exercise what the daemon does, not to widen the filter to fit it.
        let sock_path = dir.join("api.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&sock_path).expect("bind a unix socket");
        // No `set_permissions` here, though `ApiListener::bind_unix` does one:
        // that runs during initialisation, before the filter, so `chmod` is not
        // a post-init capability and is not on the allowlist. The trace agrees —
        // it saw `chmod` only before the cut.
        let mut client =
            std::os::unix::net::UnixStream::connect(&sock_path).expect("connect to a unix socket");
        let (mut server, _) = listener.accept().expect("accept a unix connection");
        client.write_all(b"ping").expect("write to a unix socket");
        let mut got = [0u8; 4];
        server
            .read_exact(&mut got)
            .expect("read from a unix socket");
        assert_eq!(&got, b"ping");
        drop((client, server, listener));
        std::fs::remove_file(&sock_path).expect("unlink the unix socket");

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
