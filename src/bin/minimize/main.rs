//! `cargo upgrade`
#![warn(
    missing_docs,
    missing_debug_implementations,
    missing_copy_implementations,
    trivial_casts,
    trivial_numeric_casts,
    unsafe_code,
    unstable_features,
    unused_import_braces,
    unused_qualifications
)]

#[macro_use]
extern crate error_chain;

use crate::errors::*;
use cargo_edit::{
    find, get_minimal_dependency, manifest_from_pkgid, registry_url,
    update_registry_index, CrateName, Dependency, LocalManifest,
};
use failure::Fail;
use semver::VersionReq;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use structopt::StructOpt;
use termcolor::{BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};
use url::Url;

mod errors {
    error_chain! {
        links {
            CargoEditLib(::cargo_edit::Error, ::cargo_edit::ErrorKind);
        }
        foreign_links {
            CargoMetadata(::failure::Compat<::cargo_metadata::Error>);
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(bin_name = "cargo")]
enum Command {
    /// Upgrade dependencies as specified in the local manifest file (i.e. Cargo.toml).
    #[structopt(name = "minimize")]
    #[structopt(
        //TODO fix text
        after_help = "This command differs from `cargo update`, which updates the dependency versions recorded in the
local lock file (Cargo.lock).

If `<dependency>`(s) are provided, only the specified dependencies will be upgraded. The version to
upgrade to for each can be specified with e.g. `docopt@0.8.0` or `serde@>=0.9,<2.0`.

Dev, build, and all target dependencies will also be upgraded. Only dependencies from crates.io are
supported. Git/path dependencies will be ignored.

All packages in the workspace will be upgraded if the `--workspace` flag is supplied. The `--workspace` flag may
be supplied in the presence of a virtual manifest.

If the '--to-lockfile' flag is supplied, all dependencies will be upgraded to the currently locked
version as recorded in the Cargo.lock file. This flag requires that the Cargo.lock file is
up-to-date. If the lock file is missing, or it needs to be updated, cargo-upgrade will exit with an
error. If the '--to-lockfile' flag is supplied then the network won't be accessed."
    )]
    Upgrade(Args),
}

#[derive(Debug, StructOpt)]
struct Args {
    /// Crates to be upgraded.
    dependency: Vec<String>,

    /// Path to the manifest to upgrade
    #[structopt(
        long = "manifest-path",
        value_name = "path",
        conflicts_with = "pkgid"
    )]
    manifest_path: Option<PathBuf>,

    /// Package id of the crate to add this dependency to.
    #[structopt(
        long = "package",
        short = "p",
        value_name = "pkgid",
        conflicts_with = "path",
        conflicts_with = "all",
        conflicts_with = "workspace"
    )]
    pkgid: Option<String>,

    /// Upgrade all packages in the workspace.
    #[structopt(
        long = "all",
        help = "[deprecated in favor of `--workspace`]",
        conflicts_with = "workspace",
        conflicts_with = "pkgid"
    )]
    all: bool,

    /// Upgrade all packages in the workspace.
    #[structopt(
        long = "workspace",
        conflicts_with = "all",
        conflicts_with = "pkgid"
    )]
    workspace: bool,

    // TODO probably doesn't make sense for minimize
    /// Include prerelease versions when fetching from crates.io (e.g. 0.6.0-alpha').
    #[structopt(long = "allow-prerelease")]
    allow_prerelease: bool,

    /// Print changes to be made without making them.
    #[structopt(long = "dry-run")]
    dry_run: bool,

    // TODO remove
    /// Only update a dependency if the new version is semver incompatible.
    #[structopt(long = "skip-compatible", conflicts_with = "to_lockfile")]
    skip_compatible: bool,

    // TODO probably doesn't make sense for minimize
    /// Run without accessing the network
    #[structopt(long = "offline")]
    pub offline: bool,
}

/// A collection of manifests.
struct Manifests(Vec<(LocalManifest, cargo_metadata::Package)>);

/// Helper function to check whether a `cargo_metadata::Dependency` is a version dependency.
fn is_version_dep(dependency: &cargo_metadata::Dependency) -> bool {
    match dependency.source {
        // This is the criterion cargo uses (in `SourceId::from_url`) to decide whether a
        // dependency has the 'registry' kind.
        Some(ref s) => s.splitn(2, '+').next() == Some("registry"),
        _ => false,
    }
}

fn deprecated_message(message: &str) -> Result<()> {
    let bufwtr = BufferWriter::stderr(ColorChoice::Always);
    let mut buffer = bufwtr.buffer();
    buffer
        .set_color(ColorSpec::new().set_fg(Some(Color::Red)).set_bold(true))
        .chain_err(|| "Failed to set output colour")?;
    writeln!(&mut buffer, "{}", message)
        .chain_err(|| "Failed to write dry run message")?;
    buffer
        .set_color(&ColorSpec::new())
        .chain_err(|| "Failed to clear output colour")?;
    bufwtr
        .print(&buffer)
        .chain_err(|| "Failed to print dry run message")
}

fn dry_run_message() -> Result<()> {
    let bufwtr = BufferWriter::stdout(ColorChoice::Always);
    let mut buffer = bufwtr.buffer();
    buffer
        .set_color(ColorSpec::new().set_fg(Some(Color::Cyan)).set_bold(true))
        .chain_err(|| "Failed to set output colour")?;
    write!(&mut buffer, "Starting dry run. ")
        .chain_err(|| "Failed to write dry run message")?;
    buffer
        .set_color(&ColorSpec::new())
        .chain_err(|| "Failed to clear output colour")?;
    writeln!(&mut buffer, "Changes will not be saved.")
        .chain_err(|| "Failed to write dry run message")?;
    bufwtr
        .print(&buffer)
        .chain_err(|| "Failed to print dry run message")
}

impl Manifests {
    /// Get all manifests in the workspace.
    fn get_all(manifest_path: &Option<PathBuf>) -> Result<Self> {
        let mut cmd = cargo_metadata::MetadataCommand::new();
        cmd.no_deps();
        if let Some(path) = manifest_path {
            cmd.manifest_path(path);
        }
        let result = cmd.exec().map_err(|e| {
            Error::from(e.compat())
                .chain_err(|| "Failed to get workspace metadata")
        })?;
        result
            .packages
            .into_iter()
            .map(|package| {
                Ok((
                    LocalManifest::try_new(Path::new(&package.manifest_path))?,
                    package,
                ))
            })
            .collect::<Result<Vec<_>>>()
            .map(Manifests)
    }

    fn get_pkgid(pkgid: &str) -> Result<Self> {
        let package = manifest_from_pkgid(pkgid)?;
        let manifest =
            LocalManifest::try_new(Path::new(&package.manifest_path))?;
        Ok(Manifests(vec![(manifest, package)]))
    }

    /// Get the manifest specified by the manifest path. Try to make an educated guess if no path is
    /// provided.
    fn get_local_one(manifest_path: &Option<PathBuf>) -> Result<Self> {
        let resolved_manifest_path: String =
            find(&manifest_path)?.to_string_lossy().into();

        let manifest = LocalManifest::find(&manifest_path)?;

        let mut cmd = cargo_metadata::MetadataCommand::new();
        cmd.no_deps();
        if let Some(path) = manifest_path {
            cmd.manifest_path(path);
        }
        let result = cmd.exec().map_err(|e| {
            Error::from(e.compat()).chain_err(|| "Invalid manifest")
        })?;
        let packages = result.packages;
        let package = packages
            .iter()
            .find(|p| p.manifest_path.to_string_lossy() == resolved_manifest_path)
            // If we have successfully got metadata, but our manifest path does not correspond to a
            // package, we must have been called against a virtual manifest.
            .chain_err(|| {
                "Found virtual manifest, but this command requires running against an \
                 actual package in this workspace. Try adding `--workspace`."
            })?;

        Ok(Manifests(vec![(manifest, package.to_owned())]))
    }

    /// Get the the combined set of dependencies to upgrade. If the user has specified
    /// per-dependency desired versions, extract those here.
    // TODO Decide what to do here, does minimizing a single dep make sense?
    //   Yes, but giving a version number does not
    //   Remove `is_version_dep` filter and related
    fn get_dependencies(
        &self,
        only_update: Vec<String>,
    ) -> Result<DesiredUpgrades> {
        // Map the names of user-specified dependencies to the (optionally) requested version.
        let selected_dependencies = only_update
            .into_iter()
            .map(|name| {
                if let Some(dependency) =
                    CrateName::new(&name).parse_as_version()?
                {
                    Ok((
                        dependency.name.clone(),
                        dependency.version().map(String::from),
                    ))
                } else {
                    Ok((name, None))
                }
            })
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(DesiredUpgrades(
            self.0
                .iter()
                .flat_map(|&(_, ref package)| package.dependencies.clone())
                .filter(is_version_dep)
                .filter_map(|dependency| {
                    let is_prerelease =
                        dependency.req.to_string().contains('-');
                    if selected_dependencies.is_empty() {
                        // User hasn't asked for any specific dependencies to be upgraded,
                        // so upgrade all the dependencies.
                        let mut dep = Dependency::new(&dependency.name);
                        if let Some(rename) = dependency.rename {
                            dep = dep.set_rename(&rename);
                        }
                        Some((
                            dep,
                            UpgradeMetadata {
                                registry: dependency.registry,
                                version: None,
                                versionreq: dependency.req,
                                is_prerelease,
                            },
                        ))
                    } else {
                        // User has asked for specific dependencies. Check if this
                        // dependency was specified, populating the registry from
                        // the lockfile metadata.
                        match selected_dependencies.get(&dependency.name) {
                            Some(version) => Some((
                                Dependency::new(&dependency.name),
                                UpgradeMetadata {
                                    registry: dependency.registry,
                                    version: version.clone(),
                                    versionreq: dependency.req,
                                    is_prerelease,
                                },
                            )),
                            None => None,
                        }
                    }
                })
                .collect(),
        ))
    }

    /// Upgrade the manifests on disk following the previously-determined upgrade schema.
    fn upgrade(
        self,
        upgraded_deps: &ActualUpgrades,
        dry_run: bool,
        skip_compatible: bool,
    ) -> Result<()> {
        if dry_run {
            dry_run_message()?;
        }

        for (mut manifest, package) in self.0 {
            println!("{}:", package.name);

            for (dep, version) in &upgraded_deps.0 {
                let mut new_dep =
                    Dependency::new(&dep.name).set_version(version);
                if let Some(rename) = dep.rename() {
                    new_dep = new_dep.set_rename(&rename);
                }
                manifest.upgrade(&new_dep, dry_run, skip_compatible)?;
            }
        }

        Ok(())
    }
}

// Some metadata about the dependency
// we're trying to upgrade.
struct UpgradeMetadata {
    registry: Option<String>,
    // `Some` if the user has specified an explicit
    // version to upgrade to.
    versionreq: VersionReq,
    version: Option<String>,
    is_prerelease: bool,
}

/// The set of dependencies to be upgraded, alongside the registries returned from cargo metadata, and
/// the desired versions, if specified by the user.
struct DesiredUpgrades(HashMap<Dependency, UpgradeMetadata>);

/// The complete specification of the upgrades that will be performed. Map of the dependency names
/// to the new versions.
#[derive(Debug)]
struct ActualUpgrades(HashMap<Dependency, String>);

impl DesiredUpgrades {
    /// Transform the dependencies into their upgraded forms. If a version is specified, all
    /// dependencies will get that version.
    fn get_upgraded(
        self,
        allow_prerelease: bool,
        manifest_path: &Path,
    ) -> Result<ActualUpgrades> {
        self.0
            .into_iter()
            .map(
                |(
                    dep,
                    UpgradeMetadata {
                        registry,
                        version,
                        versionreq,
                        is_prerelease,
                    },
                )| {
                    if let Some(v) = version {
                        Ok((dep, v))
                    } else {
                        let registry_url = match registry {
                            Some(x) => Some(Url::parse(&x).map_err(|_| {
                                ErrorKind::CargoEditLib(
                                    ::cargo_edit::ErrorKind::InvalidCargoConfig,
                                )
                            })?),
                            None => None,
                        };
                        let allow_prerelease =
                            allow_prerelease || is_prerelease;
                        get_minimal_dependency(
                            &dep.name,
                            &versionreq,
                            allow_prerelease,
                            manifest_path,
                            &registry_url,
                        )
                        .map(|new_dep| {
                            (
                                dep,
                                new_dep
                                    .version()
                                    .expect("Invalid dependency type")
                                    .to_string(),
                            )
                        })
                        .chain_err(|| "Failed to get new version")
                    }
                },
            )
            .collect::<Result<_>>()
            .map(ActualUpgrades)
    }
}

/// Main processing function. Allows us to return a `Result` so that `main` can print pretty error
/// messages.
fn process(args: Args) -> Result<()> {
    let Args {
        dependency,
        manifest_path,
        pkgid,
        all,
        allow_prerelease,
        dry_run,
        skip_compatible,
        workspace,
        ..
    } = args;

    if all {
        deprecated_message(
            "The flag `--all` has been deprecated in favor of `--workspace`",
        )?;
    }

    let all = workspace || all;

    if !args.offline && std::env::var("CARGO_IS_TEST").is_err()
    {
        let url = registry_url(&find(&manifest_path)?, None)?;
        update_registry_index(&url)?;
    }

    let manifests = if all {
        Manifests::get_all(&manifest_path)
    } else if let Some(ref pkgid) = pkgid {
        Manifests::get_pkgid(pkgid)
    } else {
        Manifests::get_local_one(&manifest_path)
    }?;

    let existing_dependencies = manifests.get_dependencies(dependency)?;

    // Update indices for any alternative registries, unless
    // we're offline.
    if !args.offline && std::env::var("CARGO_IS_TEST").is_err() {
        for registry_url in existing_dependencies
            .0
                .values()
                .filter_map(|UpgradeMetadata { registry, .. }| {
                    registry.as_ref()
                })
        .collect::<HashSet<_>>()
        {
            update_registry_index(&Url::parse(registry_url).map_err(
                    |_| {
                        ErrorKind::CargoEditLib(
                            ::cargo_edit::ErrorKind::InvalidCargoConfig,
                            )
                    },
                    )?)?;
        }
    }

    // get minimized versions
    let minimized_dependencies = existing_dependencies
        .get_upgraded(allow_prerelease, &find(&manifest_path)?)?;

    // TODO remove skip_compatible flag
    // downgrade to exact version
    manifests.upgrade(&minimized_dependencies, dry_run, skip_compatible)
}

fn main() {
    let args: Command = Command::from_args();
    let Command::Upgrade(args) = args;

    if let Err(err) = process(args) {
        eprintln!("Command failed due to unhandled error: {}\n", err);

        for e in err.iter().skip(1) {
            eprintln!("Caused by: {}", e);
        }

        if let Some(backtrace) = err.backtrace() {
            eprintln!("Backtrace: {:?}", backtrace);
        }

        process::exit(1);
    }
}
