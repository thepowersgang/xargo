use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

use rustc_version::VersionMeta;
use tempdir::TempDir;
use toml::{Table, Value};

use CompilationMode;
use cargo::{Root, Rustflags};
use errors::*;
use extensions::CommandExt;
use rustc::{Src, Sysroot, Target};
use util;
use xargo::Home;
use {cargo, xargo};

#[cfg(feature = "dev")]
fn profile() -> &'static str {
    "debug"
}

#[cfg(not(feature = "dev"))]
fn profile() -> &'static str {
    "release"
}

fn build(
    cmode: &CompilationMode,
    blueprint: Blueprint,
    ctoml: &cargo::Toml,
    home: &Home,
    rustflags: &Rustflags,
    sysroot: &Sysroot,
    hash: u64,
    verbose: bool,
) -> Result<()> {
    const TOML: &'static str = r#"
[package]
authors = ["The Rust Project Developers"]
name = "sysroot"
version = "0.0.0"
"#;

    let rustlib = home.lock_rw(cmode.triple())?;
    rustlib
        .remove_siblings_exclude(&["build"])    // exclude the build directory
        .chain_err(|| format!("couldn't clear {}", rustlib.path().display()))?;
    let dst = rustlib.parent().join("lib");
    util::mkdir(&dst)?;

    if cmode.triple().contains("pc-windows-gnu") {
        let src = &sysroot
            .path()
            .join("lib")
            .join("rustlib")
            .join(cmode.triple())
            .join("lib");

        // These are required for linking executables/dlls
        for file in ["rsbegin.o", "rsend.o", "crt2.o", "dllcrt2.o"].iter() {
            let file_src = src.join(file);
            let file_dst = dst.join(file);
            fs::copy(&file_src, &file_dst).chain_err(|| {
                format!(
                    "couldn't copy {} to {}",
                    file_src.display(),
                    file_dst.display()
                )
            })?;
        }
    }

    for (stage_num, stage) in blueprint.stages {
        let td;
        let tdp;
		// When configured to do so (in Xargo.toml) use a hard-coded and maintained build directory
        let td = if blueprint.track_sysroot {
			let mut tdp_owned = rustlib.parent().join("build");
			if ! tdp_owned.exists() {
				util::mkdir(&tdp_owned)?;
			}
			tdp_owned.push( format!("stage{}", stage_num) );
			if ! tdp_owned.exists() {
				util::mkdir(&tdp_owned)?;
			}
			tdp = tdp_owned;
			&tdp
		}
		else {
			td = TempDir::new("xargo").chain_err(|| "couldn't create a temporary directory")?;
			if env::var_os("XARGO_KEEP_TEMP").is_some() {
				tdp = td.into_path();
				&tdp
			} else {
				td.path()
			}
        };

        let mut stoml = TOML.to_owned();
        {
            let mut map = Table::new();

            map.insert("dependencies".to_owned(), Value::Table(stage.dependencies));
            if let Some(patch) = stage.patch {
                map.insert("patch".to_owned(), Value::Table(patch));
            }

            stoml.push_str(&Value::Table(map).to_string());
        }

        if let Some(profile) = ctoml.profile() {
            stoml.push_str(&profile.to_string())
        }

		{
			let inner_cargo_toml = td.join("Cargo.toml");
			if ! inner_cargo_toml.exists() {
				util::write(&inner_cargo_toml, &stoml)?;
				util::mkdir(&td.join("src"))?;
				util::write(&td.join("src/lib.rs"), "")?;
			}
			else {
				// already exists? rewrite only if there was a change.
				if util::read(&inner_cargo_toml).unwrap_or(String::new()) != stoml {
					util::write(&inner_cargo_toml, &stoml)?;
				}
			}
		}

        let stage_options = &stage.stage_options;
        let cargo = || {
            let mut cmd = Command::new("cargo");
            let mut flags = rustflags.for_xargo(home);
            // Allow a component of sysroot to not have forced unstable (for using custom std impls)
            if !stage_options.disable_staged_api {
                flags.push_str(" -Z force-unstable-if-unmarked");
            }
            if verbose {
                writeln!(io::stderr(), "+ RUSTFLAGS={:?}", flags).ok();
            }
            cmd.env("RUSTFLAGS", flags);
            cmd.env_remove("CARGO_TARGET_DIR");

            // As of rust-lang/cargo#4788 Cargo invokes rustc with a changed "current directory" so
            // we can't assume that such directory will be the same as the directory from which
            // Xargo was invoked. This is specially true when compiling the sysroot as the std
            // source is provided as a workspace and Cargo will change the current directory to the
            // root of the workspace when building one. To ensure rustc finds a target specification
            // file stored in the current directory we'll set `RUST_TARGET_PATH`  to the current
            // directory.
            if env::var_os("RUST_TARGET_PATH").is_none() {
                if let CompilationMode::Cross(ref target) = *cmode {
                    if let Target::Custom { ref json, .. } = *target {
                        cmd.env("RUST_TARGET_PATH", json.parent().unwrap());
                    }
                }
            }

            cmd.arg("build");

            match () {
                #[cfg(feature = "dev")]
                () => {}
                #[cfg(not(feature = "dev"))]
                () => {
                    cmd.arg("--release");
                }
            }
            cmd.arg("--manifest-path");
            cmd.arg(td.join("Cargo.toml"));
            cmd.args(&["--target", cmode.triple()]);

            if verbose {
                cmd.arg("-v");
            }

            cmd
        };

        for krate in stage.crates {
            cargo().arg("-p").arg(krate).run(verbose)?;
        }

        // Copy artifacts to Xargo sysroot
        util::cp_r(
            &td.join("target")
                .join(cmode.triple())
                .join(profile())
                .join("deps"),
            &dst,
        )?;
    }

    // Create hash file
    util::write(&rustlib.parent().join(".hash"), &hash.to_string())?;

    Ok(())
}

fn old_hash(cmode: &CompilationMode, home: &Home) -> Result<Option<u64>> {
    // FIXME this should be `lock_ro`
    let lock = home.lock_rw(cmode.triple())?;
    let hfile = lock.parent().join(".hash");

    if hfile.exists() {
        Ok(util::read(&hfile)?.parse().ok())
    } else {
        Ok(None)
    }
}

/// Computes the hash of the would-be target sysroot
///
/// This information is used to compute the hash
///
/// - Dependencies in `Xargo.toml` for a specific target
/// - RUSTFLAGS / build.rustflags / target.*.rustflags
/// - The target specification file, is any
/// - `[profile.release]` in `Cargo.toml`
/// - `rustc` commit hash
fn hash(
    cmode: &CompilationMode,
    blueprint: &Blueprint,
    rustflags: &Rustflags,
    ctoml: &cargo::Toml,
    meta: &VersionMeta,
) -> Result<u64> {
    let mut hasher = DefaultHasher::new();

    blueprint.hash(&mut hasher);

    rustflags.hash(&mut hasher);

    cmode.hash(&mut hasher)?;

    if let Some(profile) = ctoml.profile() {
        profile.hash(&mut hasher);
    }

    if let Some(ref hash) = meta.commit_hash {
        hash.hash(&mut hasher);
    }

    Ok(hasher.finish())
}

pub fn update(
    cmode: &CompilationMode,
    home: &Home,
    root: &Root,
    rustflags: &Rustflags,
    meta: &VersionMeta,
    src: &Src,
    sysroot: &Sysroot,
    verbose: bool,
) -> Result<()> {
    let ctoml = cargo::toml(root)?;
    let xtoml = xargo::toml(root)?;

    let blueprint = Blueprint::from(xtoml.as_ref(), cmode.triple(), root, &src)?;

    let hash = hash(cmode, &blueprint, rustflags, &ctoml, meta)?;

	// If the build directory is present in the blueprint, unconditionally try to recompile
    if blueprint.track_sysroot || old_hash(cmode, home)? != Some(hash) {
        build(
            cmode,
            blueprint,
            &ctoml,
            home,
            rustflags,
            sysroot,
            hash,
            verbose,
        )?;
    }

    // copy host artifacts into the sysroot, if necessary
    if cmode.is_native() {
        return Ok(());
    }

    let lock = home.lock_rw(&meta.host)?;
    let hfile = lock.parent().join(".hash");

	// TODO: What if the build did anything?
    let hash = meta.commit_hash.as_ref().map(|s| &**s).unwrap_or("");
    if hfile.exists() {
        if util::read(&hfile)? == hash {
            return Ok(());
        }
    }

    lock.remove_siblings()
        .chain_err(|| format!("couldn't clear {}", lock.path().display()))?;
    let dst = lock.parent().join("lib");
    util::mkdir(&dst)?;
    util::cp_r(
        &sysroot
            .path()
            .join("lib/rustlib")
            .join(&meta.host)
            .join("lib"),
        &dst,
    )?;

    let bin_dst = lock.parent().join("bin");
    util::mkdir(&bin_dst)?;
    util::cp_r(
        &sysroot
            .path()
            .join("lib/rustlib")
            .join(&meta.host)
            .join("bin"),
        &bin_dst,
    )?;

    util::write(&hfile, hash)?;

    Ok(())
}

/// Per stage dependencies
#[derive(Debug)]
pub struct Stage {
    crates: Vec<String>,
    dependencies: Table,
    patch: Option<Table>,
    stage_options: StageOptions,
}
#[derive(Debug)]
pub struct StageOptions {
    disable_staged_api: bool,
}
impl Default for StageOptions {
    fn default() -> Self {
        StageOptions {
            disable_staged_api: false,
        }
    }
}

/// A sysroot that will be built in "stages"
#[derive(Debug)]
pub struct Blueprint {
	track_sysroot: bool,
    stages: BTreeMap<i64, Stage>,
}

impl Blueprint {
    fn new() -> Self {
        Blueprint {
			track_sysroot: false,
            stages: BTreeMap::new(),
        }
    }

    fn from(toml: Option<&xargo::Toml>, target: &str, root: &Root, src: &Src) -> Result<Self> {
        let deps = match (
            toml.and_then(|t| t.dependencies()),
            toml.and_then(|t| t.target_dependencies(target)),
        ) {
            (Some(value), Some(tvalue)) => {
                let mut deps = value
                    .as_table()
                    .cloned()
                    .ok_or_else(|| format!("Xargo.toml: `dependencies` must be a table"))?;

                let more_deps = tvalue.as_table().ok_or_else(|| {
                    format!(
                        "Xargo.toml: `target.{}.dependencies` must be \
                         a table",
                        target
                    )
                })?;
                for (k, v) in more_deps {
                    if deps.insert(k.to_owned(), v.clone()).is_some() {
                        Err(format!(
                            "found duplicate dependency name {}, \
                             but all dependencies must have a \
                             unique name",
                            k
                        ))?
                    }
                }

                deps
            }
            (Some(value), None) | (None, Some(value)) => if let Some(table) = value.as_table() {
                table.clone()
            } else {
                Err(format!(
                    "Xargo.toml: target.{}.dependencies must be \
                     a table",
                    target
                ))?
            },
            (None, None) => {
                // If no dependencies were listed, we assume `core` and `compiler_builtins` as the
                // dependencies
                let mut t = BTreeMap::new();
                let mut core = BTreeMap::new();
                core.insert("stage".to_owned(), Value::Integer(0));
                t.insert("core".to_owned(), Value::Table(core));
                let mut cb = BTreeMap::new();
                cb.insert(
                    "features".to_owned(),
                    Value::Array(vec![Value::String("mem".to_owned())]),
                );
                cb.insert("stage".to_owned(), Value::Integer(1));
                t.insert(
                    "compiler_builtins".to_owned(),
                    Value::Table(cb),
                );
                t
            }
        };

        let mut blueprint = Blueprint::new();
		blueprint.track_sysroot = if let Some(v) = toml.and_then(|t| t.track_sysroot()) {
			if let &Value::Boolean(track_sysroot) = v {
				 track_sysroot
			}
			else {
                Err(format!(
                    "Xargo.toml: xargo.track-sysroot must be a boolean"
                ))?
			}
		}
		else {
			false
		};
        for (k, v) in deps {
            if let Value::Table(mut map) = v {
                let stage = if let Some(value) = map.remove("stage") {
                    value
                        .as_integer()
                        .ok_or_else(|| format!("dependencies.{}.stage must be an integer", k))?
                } else {
                    0
                };

                if let Some(path) = map.get_mut("path") {
                    let p = PathBuf::from(path.as_str()
                        .ok_or_else(|| format!("dependencies.{}.path must be a string", k))?);

                    if !p.is_absolute() {
                        *path = Value::String(
                            root.path()
                                .join(&p)
                                .canonicalize()
                                .chain_err(|| format!("couldn't canonicalize {}", p.display()))?
                                .display()
                                .to_string(),
                        );
                    }
                }

                if !map.contains_key("path") && !map.contains_key("git") {
                    // No path and no git given.  This might be in the sysroot, but if we don't find it there we assume it comes from crates.io.
                    let path = src.path().join(format!("lib{}", k));
                    if path.exists() {
                        map.insert("path".to_owned(), Value::String(path.display().to_string()));
                    }
                }

                blueprint.push(stage, k, map, src);
            } else {
                Err(format!(
                    "Xargo.toml: target.{}.dependencies.{} must be \
                     a table",
                    target, k
                ))?
            }
        }

        for (&stage_num, stage_info) in &mut blueprint.stages {
            if let Some(value) = toml.and_then(|t| t.stage_options(stage_num))
            {
                let stage_options = value
                    .as_table()
                    .ok_or_else(|| format!("Xargo.toml: `xargo.stage{}` must be a table", stage_num))
                    ?;
                stage_info.stage_options.disable_staged_api = if let Some(v) = stage_options.get("disable-staged-api") {
                        v.as_bool().ok_or_else(|| format!("Xargo.toml: `xargo.stage{}` must be a boolean", stage_num))?
                    }
                    else {
                        false
                    };
            }
        }

        Ok(blueprint)
    }

    fn push(&mut self, stage: i64, krate: String, toml: Table, src: &Src) {
        let stage = self.stages.entry(stage).or_insert_with(|| Stage {
            crates: vec![],
            dependencies: Table::new(),
            patch: {
                let rustc_std_workspace_core = src.path().join("tools/rustc-std-workspace-core");
                if rustc_std_workspace_core.exists() {
                    // For a new stage, we also need to compute the patch section of the toml
                    fn make_singleton_map(key: &str, val: Value) -> Table {
                        let mut map = Table::new();
                        map.insert(key.to_owned(), val);
                        map
                    }
                    Some(make_singleton_map("crates-io", Value::Table(
                        make_singleton_map("rustc-std-workspace-core", Value::Table(
                            make_singleton_map("path", Value::String(
                                rustc_std_workspace_core.display().to_string()
                            ))
                        ))
                    )))
                } else {
                    // an old rustc, doesn't need a rustc_std_workspace_core
                    None
                }
            },
            stage_options: Default::default(),
        });

        stage.dependencies.insert(krate.clone(), Value::Table(toml));
        stage.crates.push(krate);
    }

    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        for stage in self.stages.values() {
            for (k, v) in stage.dependencies.iter() {
                k.hash(hasher);
                v.to_string().hash(hasher);
            }
        }
    }
}
