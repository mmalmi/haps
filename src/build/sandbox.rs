//! Execution permissions are supplied by the caller, never by a recipe or reviewer.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[derive(Clone, Copy, Debug, Default)]
pub struct Execution {
    pub allow_host: bool,
    pub allow_network: bool,
}

impl Execution {
    pub fn preflight(self) -> Result<()> {
        ensure!(
            !(self.allow_host && self.allow_network),
            "choose either --allow-host-build or --allow-build-network"
        );
        if !self.allow_host {
            ensure!(
                backend().is_some_and(|path| path.is_file()),
                "build isolation is unavailable. Linux needs bubblewrap at /usr/bin/bwrap with unprivileged namespaces; macOS needs /usr/bin/sandbox-exec. Other platforms are not supported yet. To run a trusted build without isolation, explicitly use --allow-host-build"
            );
            #[cfg(target_os = "linux")]
            linux::preflight()?;
        }
        Ok(())
    }

    pub fn description(self) -> &'static str {
        if self.allow_host {
            "Host build: no execution isolation; inherited host permissions and environment."
        } else if self.allow_network {
            "Isolated files and environment; network access explicitly allowed, including host/local-network services."
        } else {
            "Isolated files and environment; network disabled."
        }
    }
}

fn backend() -> Option<&'static Path> {
    if cfg!(target_os = "linux") {
        Some(Path::new("/usr/bin/bwrap"))
    } else if cfg!(target_os = "macos") {
        Some(Path::new("/usr/bin/sandbox-exec"))
    } else {
        None
    }
}

pub(super) struct Runner {
    root: PathBuf,
    scratch: tempfile::TempDir,
    readable: Vec<PathBuf>,
    execution: Execution,
    user_home: Option<PathBuf>,
}

impl Runner {
    pub(super) fn new(root: &Path, execution: Execution) -> Result<Self> {
        execution.preflight()?;
        let root = root.canonicalize()?;
        let scratch = tempfile::Builder::new()
            .prefix("build-sandbox-")
            .tempdir_in(root.parent().context("build checkout needs a parent")?)?;
        for path in ["home", "tmp", "cache", "cargo", "config", "data"] {
            fs::create_dir(scratch.path().join(path))?;
        }
        let user_home = dirs::home_dir();
        let mut readable = Vec::new();
        // Installed tools are a trusted baseline. Never expose the whole home,
        // /etc, /var, /run, /tmp, or an arbitrary directory taken from PATH.
        let system = [
            "/usr/bin",
            "/usr/sbin",
            "/usr/lib",
            "/usr/lib64",
            "/usr/libexec",
            "/usr/share",
            "/usr/include",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/opt/hostedtoolcache",
            "/etc/ld.so.cache",
            "/etc/ld.so.conf",
            "/etc/alternatives",
            "/etc/passwd",
            "/etc/group",
            "/etc/nsswitch.conf",
            "/etc/localtime",
            "/etc/hosts",
            "/etc/resolv.conf",
            "/etc/services",
            "/etc/protocols",
            "/etc/ssl/certs",
            "/etc/ssl/cert.pem",
            "/etc/ssl/openssl.cnf",
            "/etc/pki/tls/certs",
            "/etc/pki/tls/openssl.cnf",
            "/etc/pki/ca-trust/extracted",
            "/etc/makepkg.conf",
            "/etc/makepkg.conf.d",
            "/etc/pacman.conf",
            "/var/lib/pacman/local",
            "/System/Library",
            "/System/Volumes/Preboot/Cryptexes/OS/System/Library",
            "/System/Volumes/Preboot/Cryptexes/OS/usr/lib",
            "/Library/Developer/CommandLineTools",
            "/Library/Apple",
            "/Library/Preferences/com.apple.dt.Xcode.plist",
            "/Library/Preferences/.GlobalPreferences.plist",
            "/Applications/Xcode.app/Contents",
            "/private/var/db/xcode_select_link",
            "/private/var/db/timezone",
            "/private/etc/ssl/certs",
            "/private/etc/ssl/cert.pem",
            "/private/etc/ssl/openssl.cnf",
            "/private/etc/hosts",
            "/private/etc/resolv.conf",
            "/private/etc/services",
            "/private/etc/protocols",
            "/private/etc/localtime",
        ];
        for path in system {
            add_readable(&mut readable, Path::new(path));
        }
        for prefix in ["/usr/local", "/opt/homebrew"] {
            for part in [
                "bin", "sbin", "lib", "libexec", "share", "include", "Cellar", "opt",
            ] {
                add_readable(&mut readable, &Path::new(prefix).join(part));
            }
        }
        if let Some(home) = &user_home {
            for part in [".cargo/bin", ".rustup/toolchains", ".rustup/settings.toml"] {
                add_readable(&mut readable, &home.join(part));
            }
        }
        Ok(Self {
            root,
            scratch,
            readable,
            execution,
            user_home,
        })
    }

    fn clean_environment(&self, command: &mut Command) -> Result<()> {
        let scratch = self.scratch.path();
        // Keep only PATH entries covered by installed-tool roots; never expose
        // credentials, proxy variables, loader overrides or agent/control sockets.
        let mut paths = Vec::new();
        if let Some(path) = std::env::var_os("PATH") {
            for entry in std::env::split_paths(&path) {
                if entry.is_absolute()
                    && entry.canonicalize().ok().is_some_and(|p| {
                        self.readable
                            .iter()
                            .any(|allowed| allowed.is_dir() && p.starts_with(allowed))
                    })
                {
                    paths.push(entry);
                }
            }
        }
        paths.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        command
            .env_clear()
            .env("PATH", std::env::join_paths(paths)?)
            .env("HOME", scratch.join("home"))
            .env("TMPDIR", scratch.join("tmp"))
            .env("TMP", scratch.join("tmp"))
            .env("TEMP", scratch.join("tmp"))
            .env("XDG_CACHE_HOME", scratch.join("cache"))
            .env("XDG_CONFIG_HOME", scratch.join("config"))
            .env("XDG_DATA_HOME", scratch.join("data"))
            .env("CARGO_HOME", scratch.join("cargo"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LANG", "C.UTF-8");
        if let Some(home) = &self.user_home {
            command
                .env("RUSTUP_HOME", home.join(".rustup"))
                .env("RUSTUP_AUTO_INSTALL", "0");
        }
        Ok(())
    }

    fn command(&self, args: &[String]) -> Result<Command> {
        ensure!(!args.is_empty(), "empty build command");
        let mut command = if self.execution.allow_host {
            let mut command = Command::new(&args[0]);
            command.args(&args[1..]);
            command
        } else {
            let mut command = self.isolated_command(args)?;
            self.clean_environment(&mut command)?;
            command
        };
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(std::io::stderr());
        Ok(command)
    }

    fn isolated_command(&self, args: &[String]) -> Result<Command> {
        #[cfg(target_os = "linux")]
        {
            linux::command(self, args)
        }
        #[cfg(target_os = "macos")]
        {
            macos::command(self, args)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = args;
            anyhow::bail!("build isolation is unavailable on this platform")
        }
    }

    pub(super) fn run(&self, args: &[String]) -> Result<()> {
        let status = self
            .command(args)?
            .status()
            .context("could not start build; no host fallback was attempted")?;
        ensure!(
            status.success(),
            "build failed: {}. No broader permissions were granted. If downloads are required, review them and explicitly use --allow-build-network. Missing tools must be installed separately; --allow-host-build is only for trusted builds that require host access",
            args[0].escape_debug()
        );
        Ok(())
    }
}

fn add_readable(paths: &mut Vec<PathBuf>, path: &Path) {
    if path.exists() {
        if !paths.iter().any(|p| path.starts_with(p)) {
            paths.push(path.to_owned());
        }
        if let Ok(resolved) = path.canonicalize()
            && !paths.iter().any(|p| resolved.starts_with(p))
        {
            paths.push(resolved);
        }
    }
}
