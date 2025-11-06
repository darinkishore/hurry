use std::{
    ffi::OsStr,
    io::Cursor,
    path::{Path, PathBuf},
    process::Stdio, sync::{Arc, Mutex},
    hash::{Hash, Hasher},
};

use cargo_metadata::Message;
use clap::Args;
use color_eyre::{
    Result,
    eyre::{Context, OptionExt as _, bail},
};
use derive_more::Debug;
use futures::TryStreamExt as _;
use rustc_stable_hash::StableSipHasher128;
use tracing::{debug, info, instrument, trace, warn};
use url::Url;
use serde::{Deserialize, Serialize, de, ser};

use hurry::{
    cargo::{
        self, BuiltArtifact, CargoBuildArguments, CargoCache, DepInfo, Handles, Profile, Workspace,
    },
    fs, mk_rel_dir,
    path::{JoinWith as _, TryJoinWith as _},
    progress::TransferBar,
};

#[derive(Clone, Args, Debug)]
pub struct Options {
    /// Base URL for the Courier instance.
    #[arg(
        long = "hurry-courier-url",
        env = "HURRY_COURIER_URL",
        default_value = "https://courier.staging.corp.attunehq.com"
    )]
    #[debug("{courier_url}")]
    courier_url: Url,

    /// These arguments are passed directly to `cargo build` as provided.
    #[arg(
        num_args = ..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS",
    )]
    argv: Vec<String>,
}

#[instrument]
pub async fn exec(options: Options) -> Result<()> {
    // Parse and validate cargo build arguments.
    let args = CargoBuildArguments::from_iter(&options.argv);
    debug!(?args, "parsed cargo build arguments");

    // Open workspace.
    let workspace = Workspace::from_argv(&args)
        .await
        .context("opening workspace")?;
    let profile = args.profile().map(Profile::from).unwrap_or(Profile::Debug);

    // Compute artifact plan, which provides expected artifacts. Note that
    // because we are not actually running build scripts, these "expected
    // artifacts" do not contain fully unambiguous cache key information.
    let artifact_plan = workspace
        .artifact_plan(&profile, &args)
        .await
        .context("calculating expected artifacts")?;
    info!(target = ?artifact_plan.target, "restoring using target");

    for artifact_key in artifact_plan.artifacts {
        let artifact = BuiltArtifact::from_key(&workspace, artifact_key.clone()).await?;
        debug!(?artifact, "artifact");

        let outputs = artifact.lib_files.clone();
        let fingerprint_dir = artifact.profile_dir().try_join_dirs(&[
            String::from(".fingerprint"),
            format!(
                "{}-{}",
                artifact.package_name, artifact.library_crate_compilation_unit_hash
            ),
        ])?;
        let fingerprints = fs::walk_files(&fingerprint_dir)
            .try_collect::<Vec<_>>()
            .await?;
        let deps_dir = artifact.profile_dir().join(mk_rel_dir!("deps"));
        let dep_info_file = deps_dir.try_join_file(format!(
            "{}-{}.d",
            artifact.crate_name, artifact.library_crate_compilation_unit_hash
        ))?;

        let dep_info = DepInfo::from_file(&workspace, &dep_info_file).await?;
        // let dep_info = serde_json::to_vec(&dep_info).context("serialize DepInfo")?;

        let encoded_dep_info_file =
            fingerprint_dir.try_join_file(format!("dep-lib-{}", artifact_key.crate_name))?;
        let encoded_dep_info = {
            let bytes = fs::must_read_buffered(&encoded_dep_info_file).await?;
            EncodedDepInfo::parse(&bytes).ok_or_eyre("could not parse EncodedDepInfo")?
        };

        let fingerprint_json_file = fingerprint_dir.try_join_file(format!("lib-{}.json", artifact_key.crate_name))?;
        let fingerprint: Fingerprint = {
            let bytes = fs::must_read_buffered(&fingerprint_json_file).await?;
            serde_json::from_slice(&bytes)?
        };
        let saved_hash_file = fingerprint_dir.try_join_file(format!("lib-{}", artifact_key.crate_name))?;
        let saved_hash = fs::must_read_buffered_utf8(&saved_hash_file).await?;

        let computed_hash = hex::encode(fingerprint.hash_u64().to_le_bytes());

        debug!(
            ?outputs,
            ?fingerprints,
            ?dep_info_file,
            ?dep_info,
            ?encoded_dep_info_file,
            ?encoded_dep_info,
            ?fingerprint_json_file,
            ?fingerprint,
            ?saved_hash_file,
            ?computed_hash,
            ?saved_hash,
            "artifact files"
        );
    }
    Ok(())

    // // Initialize cache.
    // let cache = CargoCache::open(options.courier_url, workspace)
    //     .await
    //     .context("opening cache")?;

    // // Restore artifacts.
    // let artifact_count = artifact_plan.artifacts.len() as u64;
    // let progress = TransferBar::new(artifact_count, "Restoring cache");
    // cache.restore(&artifact_plan, &progress).await?;
    // drop(progress);

    // // Run build with `--message-format=json` for freshness indicators and
    // // `--verbose` for debugging information.
    // let mut argv = options.argv;
    // if !argv.contains(&String::from("--message-format=json")) {
    //     argv.push(String::from("--message-format=json"));
    // }
    // if !argv.contains(&String::from("--verbose")) {
    //     argv.push(String::from("--verbose"));
    // }
    // let handles = Handles {
    //     stdout: Stdio::piped(),
    //     stderr: Stdio::inherit(),
    // };
    // let child = cargo::invoke_with("build", &argv, [] as [(&OsStr, &OsStr); 0], handles)
    //     .await
    //     .context("build with cargo")?;
    // let output = child.wait_with_output().await?;
    // trace!(?output, "cargo output");
    // let output = Cursor::new(output.stdout);
    // let mut ok = true;
    // for message in Message::parse_stream(output) {
    //     debug!(?message, "cargo message");
    //     let message = message?;
    //     if let Message::CompilerArtifact(msg) = message
    //         && !msg.fresh
    //         && msg
    //             .package_id
    //             .repr
    //             .starts_with("registry+https://github.com/rust-lang/crates.io-index#")
    //     {
    //         // TODO: Only warn if _restored_ artifacts are not fresh.
    //         warn!("artifact {:?} is not fresh", msg.package_id);
    //         ok = false;
    //     }
    // }

    // if ok {
    //     info!("OK");
    //     Ok(())
    // } else {
    //     bail!("not all artifacts were fresh")
    // }
}

const CURRENT_ENCODED_DEP_INFO_VERSION: u8 = 1;

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone)]
pub enum DepInfoPathType {
    /// src/, e.g. src/lib.rs
    PackageRootRelative,
    /// {build-dir}/debug/deps/lib...
    /// or an absolute path /.../sysroot/...
    BuildRootRelative,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct EncodedDepInfo {
    pub files: Vec<(DepInfoPathType, PathBuf, Option<(u64, String)>)>,
    pub env: Vec<(String, Option<String>)>,
}

impl EncodedDepInfo {
    pub fn parse(mut bytes: &[u8]) -> Option<EncodedDepInfo> {
        let bytes = &mut bytes;
        read_magic_marker(bytes)?;
        let version = read_u8(bytes)?;
        if version != CURRENT_ENCODED_DEP_INFO_VERSION {
            return None;
        }

        let nfiles = read_usize(bytes)?;
        let mut files = Vec::with_capacity(nfiles);
        for _ in 0..nfiles {
            let ty = match read_u8(bytes)? {
                0 => DepInfoPathType::PackageRootRelative,
                1 => DepInfoPathType::BuildRootRelative,
                _ => return None,
            };
            let path_bytes = read_bytes(bytes)?;
            let path = paths__bytes2path(path_bytes).ok()?;
            let has_checksum = read_bool(bytes)?;
            let checksum_info = has_checksum
                .then(|| {
                    let file_len = read_u64(bytes);
                    let checksum_string = read_bytes(bytes)
                        .map(Vec::from)
                        .and_then(|v| String::from_utf8(v).ok());
                    file_len.zip(checksum_string)
                })
                .flatten();
            files.push((ty, path, checksum_info));
        }

        let nenv = read_usize(bytes)?;
        let mut env = Vec::with_capacity(nenv);
        for _ in 0..nenv {
            let key = str::from_utf8(read_bytes(bytes)?).ok()?.to_string();
            let val = match read_u8(bytes)? {
                0 => None,
                1 => Some(str::from_utf8(read_bytes(bytes)?).ok()?.to_string()),
                _ => return None,
            };
            env.push((key, val));
        }
        return Some(EncodedDepInfo { files, env });

        /// See [`EncodedDepInfo`] for why a magic marker exists.
        fn read_magic_marker(bytes: &mut &[u8]) -> Option<()> {
            let _size = read_usize(bytes)?;
            let path_type = read_u8(bytes)?;
            if path_type != u8::MAX {
                // Old depinfo. Give up parsing it.
                None
            } else {
                Some(())
            }
        }

        fn read_usize(bytes: &mut &[u8]) -> Option<usize> {
            let ret = bytes.get(..4)?;
            *bytes = &bytes[4..];
            Some(u32::from_le_bytes(ret.try_into().unwrap()) as usize)
        }

        fn read_u64(bytes: &mut &[u8]) -> Option<u64> {
            let ret = bytes.get(..8)?;
            *bytes = &bytes[8..];
            Some(u64::from_le_bytes(ret.try_into().unwrap()))
        }

        fn read_bool(bytes: &mut &[u8]) -> Option<bool> {
            read_u8(bytes).map(|b| b != 0)
        }

        fn read_u8(bytes: &mut &[u8]) -> Option<u8> {
            let ret = *bytes.get(0)?;
            *bytes = &bytes[1..];
            Some(ret)
        }

        fn read_bytes<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
            let n = read_usize(bytes)? as usize;
            let ret = bytes.get(..n)?;
            *bytes = &bytes[n..];
            Some(ret)
        }
    }

    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut ret = Vec::new();
        let dst = &mut ret;

        write_magic_marker(dst);
        dst.push(CURRENT_ENCODED_DEP_INFO_VERSION);

        write_usize(dst, self.files.len());
        for (ty, file, checksum_info) in self.files.iter() {
            match ty {
                DepInfoPathType::PackageRootRelative => dst.push(0),
                DepInfoPathType::BuildRootRelative => dst.push(1),
            }
            write_bytes(dst, paths__path2bytes(file)?);
            write_bool(dst, checksum_info.is_some());
            if let Some((len, checksum)) = checksum_info {
                write_u64(dst, *len);
                write_bytes(dst, checksum);
            }
        }

        write_usize(dst, self.env.len());
        for (key, val) in self.env.iter() {
            write_bytes(dst, key);
            match val {
                None => dst.push(0),
                Some(val) => {
                    dst.push(1);
                    write_bytes(dst, val);
                }
            }
        }
        return Ok(ret);

        /// See [`EncodedDepInfo`] for why a magic marker exists.
        ///
        /// There is an assumption that there is always at least a file.
        fn write_magic_marker(dst: &mut Vec<u8>) {
            write_usize(dst, 1);
            dst.push(u8::MAX);
        }

        fn write_bytes(dst: &mut Vec<u8>, val: impl AsRef<[u8]>) {
            let val = val.as_ref();
            write_usize(dst, val.len());
            dst.extend_from_slice(val);
        }

        fn write_usize(dst: &mut Vec<u8>, val: usize) {
            dst.extend(&u32::to_le_bytes(val as u32));
        }

        fn write_u64(dst: &mut Vec<u8>, val: u64) {
            dst.extend(&u64::to_le_bytes(val));
        }

        fn write_bool(dst: &mut Vec<u8>, val: bool) {
            dst.push(u8::from(val));
        }
    }
}

/// Converts UTF-8 bytes to a path.
pub fn paths__path2bytes(path: &Path) -> Result<&[u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::prelude::*;
        Ok(path.as_os_str().as_bytes())
    }
    #[cfg(windows)]
    {
        match path.as_os_str().to_str() {
            Some(s) => Ok(s.as_bytes()),
            None => Err(anyhow::format_err!(
                "invalid non-unicode path: {}",
                path.display()
            )),
        }
    }
}

/// Converts UTF-8 bytes to a path.
pub fn paths__bytes2path(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::prelude::*;
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    #[cfg(windows)]
    {
        use std::str;
        match str::from_utf8(bytes) {
            Ok(s) => Ok(PathBuf::from(s)),
            Err(..) => Err(anyhow::format_err!("invalid non-unicode path")),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Hash of the version of `rustc` used.
    rustc: u64,
    /// Sorted list of cfg features enabled.
    features: String,
    /// Sorted list of all the declared cfg features.
    declared_features: String,
    /// Hash of the `Target` struct, including the target name,
    /// package-relative source path, edition, etc.
    target: u64,
    /// Hash of the [`Profile`], [`CompileMode`], and any extra flags passed via
    /// `cargo rustc` or `cargo rustdoc`.
    ///
    /// [`Profile`]: crate::core::profiles::Profile
    /// [`CompileMode`]: crate::core::compiler::CompileMode
    profile: u64,
    /// Hash of the path to the base source file. This is relative to the
    /// workspace root for path members, or absolute for other sources.
    path: u64,
    /// Fingerprints of dependencies.
    deps: Vec<DepFingerprint>,
    /// Information about the inputs that affect this Unit (such as source
    /// file mtimes or build script environment variables).
    local: Mutex<Vec<LocalFingerprint>>,
    /// Cached hash of the [`Fingerprint`] struct. Used to improve performance
    /// for hashing.
    #[serde(skip)]
    memoized_hash: Mutex<Option<u64>>,
    /// RUSTFLAGS/RUSTDOCFLAGS environment variable value (or config value).
    rustflags: Vec<String>,
    /// Hash of various config settings that change how things are compiled.
    config: u64,
    /// The rustc target. This is only relevant for `.json` files, otherwise
    /// the metadata hash segregates the units.
    compile_kind: u64,
    /// Description of whether the filesystem status for this unit is up to date
    /// or should be considered stale.
    // #[serde(skip)]
    // fs_status: FsStatus,
    /// Files, relative to `target_root`, that are produced by the step that
    /// this `Fingerprint` represents. This is used to detect when the whole
    /// fingerprint is out of date if this is missing, or if previous
    /// fingerprints output files are regenerated and look newer than this one.
    #[serde(skip)]
    outputs: Vec<PathBuf>,
}

impl Fingerprint {
    fn new() -> Fingerprint {
        Fingerprint {
            rustc: 0,
            target: 0,
            profile: 0,
            path: 0,
            features: String::new(),
            declared_features: String::new(),
            deps: Vec::new(),
            local: Mutex::new(Vec::new()),
            memoized_hash: Mutex::new(None),
            rustflags: Vec::new(),
            config: 0,
            compile_kind: 0,
            // fs_status: FsStatus::Stale,
            outputs: Vec::new(),
        }
    }

    /// For performance reasons fingerprints will memoize their own hash, but
    /// there's also internal mutability with its `local` field which can
    /// change, for example with build scripts, during a build.
    ///
    /// This method can be used to bust all memoized hashes just before a build
    /// to ensure that after a build completes everything is up-to-date.
    pub fn clear_memoized(&self) {
        *self.memoized_hash.lock().unwrap() = None;
    }

    fn hash_u64(&self) -> u64 {
        if let Some(s) = *self.memoized_hash.lock().unwrap() {
            return s;
        }
        let ret = util__hash_u64(self);
        *self.memoized_hash.lock().unwrap() = Some(ret);
        ret
    }
}

impl Hash for Fingerprint {
    fn hash<H: Hasher>(&self, h: &mut H) {
        let Fingerprint {
            rustc,
            ref features,
            ref declared_features,
            target,
            path,
            profile,
            ref deps,
            ref local,
            config,
            compile_kind,
            ref rustflags,
            ..
        } = *self;
        let local = local.lock().unwrap();
        (
            rustc,
            features,
            declared_features,
            target,
            path,
            profile,
            &*local,
            config,
            compile_kind,
            rustflags,
        )
            .hash(h);

        h.write_usize(deps.len());
        for DepFingerprint {
            pkg_id,
            name,
            public,
            fingerprint,
            only_requires_rmeta: _, // static property, no need to hash
        } in deps
        {
            pkg_id.hash(h);
            name.hash(h);
            public.hash(h);
            // use memoized dep hashes to avoid exponential blowup
            h.write_u64(fingerprint.hash_u64());
        }
    }
}


pub fn util__hash_u64<H: Hash>(hashable: H) -> u64 {
    let mut hasher = StableSipHasher128::new();
    hashable.hash(&mut hasher);
    Hasher::finish(&hasher)
}

#[derive(Debug, Clone)]
struct DepFingerprint {
    /// The hash of the package id that this dependency points to
    pkg_id: u64,
    /// The crate name we're using for this dependency, which if we change we'll
    /// need to recompile!
    name: String,
    /// Whether or not this dependency is flagged as a public dependency or not.
    public: bool,
    /// Whether or not this dependency is an rmeta dependency or a "full"
    /// dependency. In the case of an rmeta dependency our dependency edge only
    /// actually requires the rmeta from what we depend on, so when checking
    /// mtime information all files other than the rmeta can be ignored.
    only_requires_rmeta: bool,
    /// The dependency's fingerprint we recursively point to, containing all the
    /// other hash information we'd otherwise need.
    fingerprint: Arc<Fingerprint>,
}

impl Serialize for DepFingerprint {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        (
            &self.pkg_id,
            &self.name,
            &self.public,
            &self.fingerprint.hash_u64(),
        )
            .serialize(ser)
    }
}

impl<'de> Deserialize<'de> for DepFingerprint {
    fn deserialize<D>(d: D) -> Result<DepFingerprint, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let (pkg_id, name, public, hash) = <(u64, String, bool, u64)>::deserialize(d)?;
        Ok(DepFingerprint {
            pkg_id,
            name: name.into(),
            public,
            fingerprint: Arc::new(Fingerprint {
                memoized_hash: Mutex::new(Some(hash)),
                ..Fingerprint::new()
            }),
            // This field is never read since it's only used in
            // `check_filesystem` which isn't used by fingerprints loaded from
            // disk.
            only_requires_rmeta: false,
        })
    }
}

#[derive(Debug, Serialize, Deserialize, Hash)]
enum LocalFingerprint {
    /// This is a precalculated fingerprint which has an opaque string we just
    /// hash as usual. This variant is primarily used for rustdoc where we
    /// don't have a dep-info file to compare against.
    ///
    /// This is also used for build scripts with no `rerun-if-*` statements, but
    /// that's overall a mistake and causes bugs in Cargo. We shouldn't use this
    /// for build scripts.
    Precalculated(String),

    /// This is used for crate compilations. The `dep_info` file is a relative
    /// path anchored at `target_root(...)` to the dep-info file that Cargo
    /// generates (which is a custom serialization after parsing rustc's own
    /// `dep-info` output).
    ///
    /// The `dep_info` file, when present, also lists a number of other files
    /// for us to look at. If any of those files are newer than this file then
    /// we need to recompile.
    ///
    /// If the `checksum` bool is true then the `dep_info` file is expected to
    /// contain file checksums instead of file mtimes.
    CheckDepInfo { dep_info: PathBuf, checksum: bool },

    /// This represents a nonempty set of `rerun-if-changed` annotations printed
    /// out by a build script. The `output` file is a relative file anchored at
    /// `target_root(...)` which is the actual output of the build script. That
    /// output has already been parsed and the paths printed out via
    /// `rerun-if-changed` are listed in `paths`. The `paths` field is relative
    /// to `pkg.root()`
    ///
    /// This is considered up-to-date if all of the `paths` are older than
    /// `output`, otherwise we need to recompile.
    RerunIfChanged {
        output: PathBuf,
        paths: Vec<PathBuf>,
    },

    /// This represents a single `rerun-if-env-changed` annotation printed by a
    /// build script. The exact env var and value are hashed here. There's no
    /// filesystem dependence here, and if the values are changed the hash will
    /// change forcing a recompile.
    RerunIfEnvChanged { var: String, val: Option<String> },
}

// #[derive(Clone, Default, Debug)]
// pub enum FsStatus {
//     /// This unit is to be considered stale, even if hash information all
//     /// matches.
//     #[default]
//     Stale,

//     /// File system inputs have changed (or are missing), or there were
//     /// changes to the environment variables that affect this unit. See
//     /// the variants of [`StaleItem`] for more information.
//     StaleItem(StaleItem),

//     /// A dependency was stale.
//     StaleDependency {
//         name: InternedString,
//         dep_mtime: FileTime,
//         max_mtime: FileTime,
//     },

//     /// A dependency was stale.
//     StaleDepFingerprint { name: InternedString },

//     /// This unit is up-to-date. All outputs and their corresponding mtime are
//     /// listed in the payload here for other dependencies to compare against.
//     UpToDate { mtimes: HashMap<PathBuf, FileTime> },
// }
