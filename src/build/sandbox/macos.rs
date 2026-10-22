use super::*;

fn quoted(path: &Path) -> Result<String> {
    let path = path.to_str().context("sandbox paths must be UTF-8")?;
    ensure!(
        !path.chars().any(char::is_control),
        "sandbox paths cannot contain control characters"
    );
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub(super) fn command(runner: &Runner, args: &[String]) -> Result<Command> {
    let mut policy = String::from(
        r#"(version 1)
(deny default)
(allow process-exec process-fork)
(allow signal (target same-sandbox))
; An allow rule with a target filter does not close KERN_PROCARGS2 access.
; Explicitly deny inspection of processes outside this sandbox instead.
(deny process-info* (require-not (target same-sandbox)))
(allow file-read-metadata)
(allow file-read* (literal "/"))
; Arbitrary sysctls can disclose other processes' arguments and environment.
(allow sysctl-read (sysctl-name-prefix "hw.") (sysctl-name-prefix "machdep.cpu.")
  (sysctl-name "kern.argmax") (sysctl-name "kern.maxfilesperproc")
  (sysctl-name "kern.hostname") ; uname(), including Python's ctypes startup
  (sysctl-name "kern.maxproc") (sysctl-name "kern.osproductversion")
  (sysctl-name "kern.osrelease") (sysctl-name "kern.ostype")
  (sysctl-name "kern.osvariant_status") (sysctl-name "kern.osversion")
  (sysctl-name "kern.secure_kernel") (sysctl-name "kern.sysv.semmns")
  (sysctl-name "kern.usrstack64") (sysctl-name "kern.version")
  (sysctl-name "sysctl.proc_cputype") (sysctl-name "vm.loadavg"))
(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))
(allow file-read* (literal "/dev/null") (literal "/dev/random")
                  (literal "/dev/urandom") (literal "/dev/zero") (subpath "/dev/fd"))
(allow file-write* (literal "/dev/null"))
"#,
    );
    for path in &runner.readable {
        policy.push_str(&format!(
            "(allow file-read* ({} {}))\n",
            if path.is_dir() { "subpath" } else { "literal" },
            quoted(path)?
        ));
    }
    for path in [&runner.root, runner.scratch.path()] {
        let path = quoted(path)?;
        policy.push_str(&format!(
            "(allow file-read* file-write* (subpath {path}))\n"
        ));
        // Keep the directory named by the next command's policy in place. A
        // build may change its contents, never replace the policy root itself.
        policy.push_str(&format!(
            "(deny file-write-create file-write-unlink (literal {path}))\n"
        ));
        policy.push_str(&format!(
            "(allow network* (local unix-socket (subpath {path})))\n"
        ));
    }
    if runner.execution.allow_network {
        // Network permission is deliberately explicit: it includes local TCP
        // services. Filesystem Unix sockets remain confined to private scratch.
        policy.push_str(
            r#"(allow network-outbound (remote ip))
(allow mach-lookup (global-name "com.apple.bsd.dirhelper")
                   (global-name "com.apple.system.opendirectoryd.membership")
                   (global-name "com.apple.trustd.agent")
                   (global-name "com.apple.networkd")
                   (global-name "com.apple.SystemConfiguration.DNSConfiguration")
                   (global-name "com.apple.SystemConfiguration.configd")
                   (global-name "com.apple.mDNSResponder"))
(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))
(allow sysctl-read (sysctl-name-prefix "net.routetable."))
(allow network-outbound (remote unix-socket (literal "/private/var/run/mDNSResponder")))
"#,
        );
    }
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &policy, "--"]).args(args);
    Ok(command)
}
