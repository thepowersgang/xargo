use std::path::{Display, Path, PathBuf};
use std::process::ExitStatus;
use std::{env, mem};
use std::io::{self, Write};

use toml::Value;
use rustc_version::VersionMeta;

use CompilationMode;
use cargo::{Config, Root, Rustflags, Subcommand};
use cli::Args;
use errors::*;
use extensions::CommandExt;
use flock::{FileLock, Filesystem};
use {cargo, util};

pub fn run(
    args: &Args,
    cmode: &CompilationMode,
    rustflags: Rustflags,
    home: &Home,
    meta: &VersionMeta,
    config: Option<&Config>,
    verbose: bool,
) -> Result<ExitStatus> {
    let mut cmd = cargo::command();
    cmd.args(args.all());

    if args.subcommand() == Some(Subcommand::Doc) {
        cmd.env(
            "RUSTDOCFLAGS",
            cargo::rustdocflags(config, cmode.triple())?.for_xargo(home),
        );
    }

    let flags = rustflags.for_xargo(home);
    if verbose {
        writeln!(io::stderr(), "+ RUSTFLAGS={:?}", flags).ok();
    }
    cmd.env("RUSTFLAGS", flags);

    let locks = (home.lock_ro(&meta.host), home.lock_ro(cmode.triple()));

    let status = cmd.run_and_get_status(verbose)?;

    mem::drop(locks);

    Ok(status)
}

pub struct Home {
    path: Filesystem,
}

impl Home {
    pub fn display(&self) -> Display {
        self.path.display()
    }

    fn path(&self, triple: &str) -> Filesystem {
        self.path.join("lib/rustlib").join(triple)
    }

    pub fn lock_ro(&self, triple: &str) -> Result<FileLock> {
        let fs = self.path(triple);

        fs.open_ro(".sentinel", &format!("{}'s sysroot", triple))
            .chain_err(|| {
                format!("couldn't lock {}'s sysroot as read-only", triple)
            })
    }

    pub fn lock_rw(&self, triple: &str) -> Result<FileLock> {
        let fs = self.path(triple);

        fs.open_rw(".sentinel", &format!("{}'s sysroot", triple))
            .chain_err(|| {
                format!("couldn't lock {}'s sysroot as read-only", triple)
            })
    }
}

pub fn home(cmode: &CompilationMode) -> Result<Home> {
    let mut p = if let Some(h) = env::var_os("XARGO_HOME") {
        PathBuf::from(h)
    } else {
        dirs::home_dir()
            .ok_or_else(|| "couldn't find your home directory. Is $HOME set?")?
            .join(".xargo")
    };

    if cmode.is_native() {
        p.push("HOST");
    }

    Ok(Home {
        path: Filesystem::new(p),
    })
}

pub struct Toml {
    table: Value,
}

impl Toml {
    /// Returns the `dependencies` part of `Xargo.toml`
    pub fn dependencies(&self) -> Option<&Value> {
        self.table.lookup("dependencies")
    }

    /// Returns the `target.{}.dependencies` part of `Xargo.toml`
    pub fn target_dependencies(&self, target: &str) -> Option<&Value> {
        self.table
            .lookup(&format!("target.{}.dependencies", target))
    }

    /// Rreturns the `xargo.track-sysroot` option, that controls if the sysroot's build state is maintained and always rebuilt
    pub fn track_sysroot(&self) -> Option<&Value> {
        self.table
            .lookup("xargo.track-sysroot")
    }

    pub fn stage_options(&self, stage_num: i64) -> Option<&Value> {
        self.table
            .lookup(&format!("xargo.stage{}", stage_num))
    }

    /// Returns the `patch` part of `Xargo.toml`
    pub fn patch(&self) -> Option<&Value> {
        self.table.lookup("patch")
    }
}

/// Returns the closest directory containing a 'Xargo.toml' and the parsed
/// content of this 'Xargo.toml'
pub fn toml(root: &Root) -> Result<(Option<&Path>, Option<Toml>)> {
    if let Some(p) = util::search(root.path(), "Xargo.toml") {
        Ok((Some(p), util::parse(&p.join("Xargo.toml")).map(|t| Some(Toml { table: t }))?))
    }
    else {
        Ok((None, None))
    }
}
