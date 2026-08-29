//! Locating `adb` and managing the USB port forward.
//!
//! `adb` is the transport: the Android server binds loopback on the phone, and
//! `adb forward` bridges a loopback port on this PC to it over USB. Both ends of
//! that are handled here so the user never has to run adb by hand.
//!
//! Discovery matters more than it looks. The SDK does not add itself to `PATH`,
//! and a lot of people acquire adb sideways — bundled with scrcpy, via winget,
//! chocolatey or scoop — so "adb is installed" and "adb is on `PATH`" are quite
//! different claims. We check the places it actually lands.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const ADB_EXE: &str = if cfg!(windows) { "adb.exe" } else { "adb" };

/// Path on the device where the app writes its auth token.
const TOKEN_PATH: &str = "/sdcard/.afs/token";

/// Is this a file we can actually execute as adb?
fn runnable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    Command::new(path)
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Every place worth looking, in preference order.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    // 1. PATH — the happy case.
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            out.push(dir.join(ADB_EXE));
        }
    }

    // 2. The SDK, wherever the environment says it is.
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(sdk) = env::var_os(var) {
            out.push(PathBuf::from(sdk).join("platform-tools").join(ADB_EXE));
        }
    }

    // 3. The SDK's default location, which is where Android Studio puts it and
    //    where it stays for most people who never touch the environment vars.
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        out.push(
            local
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join(ADB_EXE),
        );

        // 4. winget packages. scrcpy ships its own adb and is a very common way
        //    to end up with one; the version directory is unpredictable, so scan.
        out.extend(scan_for_adb(
            &local.join("Microsoft").join("WinGet").join("Packages"),
            3,
        ));
    }

    if let Some(userprofile) = env::var_os("USERPROFILE") {
        let home = PathBuf::from(userprofile);
        // 5. Scoop.
        out.push(
            home.join("scoop")
                .join("apps")
                .join("adb")
                .join("current")
                .join(ADB_EXE),
        );
        out.push(home.join("scoop").join("shims").join(ADB_EXE));
    }

    // 6. Chocolatey.
    out.push(PathBuf::from(r"C:\ProgramData\chocolatey\bin").join(ADB_EXE));

    // 7. A system-wide SDK install.
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = env::var_os(var) {
            out.push(
                PathBuf::from(pf)
                    .join("Android")
                    .join("android-sdk")
                    .join("platform-tools")
                    .join(ADB_EXE),
            );
        }
    }

    out
}

/// Breadth-limited hunt for `adb.exe` under `root`.
///
/// Depth-capped and bailing at the first hit per branch: this runs on every
/// start, and walking an unbounded tree to find a file that is usually elsewhere
/// would be a poor trade.
fn scan_for_adb(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if depth == 0 || !root.is_dir() {
        return found;
    }

    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };

    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.file_name().is_some_and(|n| n == ADB_EXE) {
            found.push(path);
        }
    }

    for dir in subdirs {
        found.extend(scan_for_adb(&dir, depth - 1));
    }
    found
}

/// Finds a usable adb, or explains where it looked.
pub fn find_adb(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if runnable(p) {
            return Ok(p.to_path_buf());
        }
        bail!("--adb does not point at a runnable adb: {}", p.display());
    }

    if let Some(v) = env::var_os("AFS_ADB") {
        let p = PathBuf::from(v);
        if runnable(&p) {
            return Ok(p);
        }
        log::warn!("AFS_ADB is set but not runnable: {}", p.display());
    }

    for cand in candidates() {
        if runnable(&cand) {
            log::debug!("found adb at {}", cand.display());
            return Ok(cand);
        }
    }

    bail!(
        "could not find adb.\n\
         \n\
         Looked on PATH, in ANDROID_HOME / ANDROID_SDK_ROOT, the default SDK\n\
         location, and winget / scoop / chocolatey package directories.\n\
         \n\
         Install the Android SDK Platform Tools from\n\
         https://developer.android.com/tools/releases/platform-tools\n\
         then either add it to PATH, or point at it directly:\n\
         \n\
             afs-host.exe --adb C:\\path\\to\\adb.exe --mount Z:\n\
         \n\
         or set AFS_ADB to its full path."
    )
}

fn run(adb: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(adb)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {}", adb.display()))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("adb {} failed: {}", args.join(" "), err);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Reads the shared token the app generated on first run.
pub fn read_token(adb: &Path) -> Result<String> {
    let token = run(adb, &["shell", "cat", TOKEN_PATH])?;

    // adb shell reports a missing file on stdout with a zero exit status, so the
    // command "succeeding" is not enough to go on.
    if token.is_empty() || token.contains("No such file") {
        bail!(
            "no token at {TOKEN_PATH} on the device.\n\
             Open the AFS Server app once, grant all-files access, and start the server."
        );
    }
    Ok(token)
}

/// An `adb forward` that cleans up after itself.
pub struct Forward {
    adb: PathBuf,
    port: u16,
    /// False when the forward was already there, so Drop leaves it alone.
    owned: bool,
    removed: bool,
}

impl Forward {
    /// Creates `tcp:<port>` → `tcp:<port>`, or adopts an existing one.
    pub fn create(adb: &Path, port: u16) -> Result<Self> {
        let spec = format!("tcp:{port}");

        // Someone may already have set this up by hand, or left one behind from a
        // previous run. Adopting rather than recreating means we do not tear down
        // a forward we did not establish when we exit.
        let existing = run(adb, &["forward", "--list"]).unwrap_or_default();
        let owned = !existing
            .lines()
            .any(|l| l.split_whitespace().any(|f| f == spec));

        if owned {
            run(adb, &["forward", &spec, &spec])?;
            log::info!("adb forward {spec} -> {spec}");
        } else {
            log::info!("adb forward {spec} already present, reusing it");
        }

        Ok(Forward {
            adb: adb.to_path_buf(),
            port,
            owned,
            removed: false,
        })
    }

    /// Tears the forward down. Idempotent, and safe to call before Drop.
    pub fn remove(&mut self) {
        if self.removed || !self.owned {
            return;
        }
        self.removed = true;

        let spec = format!("tcp:{}", self.port);
        match run(&self.adb, &["forward", "--remove", &spec]) {
            Ok(_) => log::info!("removed adb forward {spec}"),
            // Worth a line but not worth failing over: the device being unplugged
            // is the usual cause, and it takes the forward with it anyway.
            Err(e) => log::debug!("could not remove adb forward {spec}: {e}"),
        }
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.remove();
    }
}
