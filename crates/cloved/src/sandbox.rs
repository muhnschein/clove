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
//! - **seccomp** installs a *deny* filter over the syscalls listed in
//!   `DENIED`, plus `socket(2)` for any address family outside
//!   `AF_UNIX`/`AF_INET`/`AF_INET6`. A deny filter, not an allowlist: an
//!   allowlist over a threaded Rust process with an allocator underneath is a
//!   list that breaks in the field on a kernel or libc we did not test, and
//!   SCOPE §5 names exactly the capabilities to drop. Layer 3's systemd
//!   `SystemCallFilter=` is where the allowlist lives, written by people who
//!   can see the deployment.
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

    /// Syscalls the daemon has no business making after initialisation, named
    /// rather than numbered so the list reads as the policy it is.
    ///
    /// The groups are SCOPE §5's: exec, ptrace, module and BPF loading, mount
    /// and namespace manipulation, kernel replacement, and the keyring and
    /// tracing interfaces that are a standard sandbox-escape shopping list.
    const DENIED: &[libc::c_long] = &[
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_bpf,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_open_by_handle_at,
        libc::SYS_perf_event_open,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_keyctl,
    ];

    /// Build the program: every syscall in [`DENIED`] unconditionally, plus
    /// `socket(2)` for address families the client never speaks.
    fn build() -> Result<Vec<seccompiler::sock_filter>, seccompiler::Error> {
        let arch = TargetArch::try_from(std::env::consts::ARCH)?;
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        for &nr in DENIED {
            // An empty rule vector matches the syscall unconditionally.
            rules.insert(nr, Vec::new());
        }
        // The conditions within one rule are ANDed, so this reads "the family
        // is none of these three".
        let family = |af: libc::c_int| {
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                af.try_into().unwrap_or(u64::MAX),
            )
        };
        rules.insert(
            libc::SYS_socket,
            vec![SeccompRule::new(vec![
                family(libc::AF_UNIX)?,
                family(libc::AF_INET)?,
                family(libc::AF_INET6)?,
            ])?],
        );
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(u32::try_from(libc::EPERM).unwrap_or(1)),
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
        }
        println!("CONFINED {line}");
    }
}
