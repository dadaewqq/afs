# Building AFS

Produces the two artifacts [INSTALL.md](INSTALL.md) expects:

- `android/app/build/outputs/apk/release/app-release.apk`
- `windows/afs-host/target/release/afs-host.exe`

The two halves are independent — nothing is shared at build time except the wire
protocol in [docs/protocol.md](docs/protocol.md), which both sides implement by
hand. You can build either alone.

---

## Prerequisites

| Tool                | Version              | Needed for  |
|---------------------|----------------------|-------------|
| Rust (MSVC toolchain) | 1.77+              | `afs-host`  |
| WinFsp **+ Developer** | 2.0+              | `afs-host`  |
| JDK                 | 17+ (21 tested)      | the APK     |
| Android SDK         | platform 36, build-tools 34+ | the APK |

No NDK. The Android side is pure Kotlin.

### Rust

Install from <https://rustup.rs>. On Windows this pulls the MSVC toolchain, which
needs the **Visual Studio Build Tools** with the *Desktop development with C++*
workload — rustup prompts for it if missing.

```powershell
rustc --version
```

### WinFsp — with the Developer component

Grab the installer from <https://winfsp.dev/rel/>. When it asks which features to
install, **enable "Developer"** as well as Core.

> This is the step that catches people out. The default Core install gives you
> `bin\winfsp-x64.dll` and nothing else. Building additionally needs
> `inc\winfsp\winfsp.h` and `lib\winfsp-x64.lib`, and without them the build dies
> with `fatal error: 'winfsp/winfsp.h' file not found` from bindgen.
>
> Running the finished binary needs only Core — end users never install Developer.

Already installed Core-only? Add the component without uninstalling:

```powershell
# elevated prompt; find the product code with the query below
msiexec /i "{YOUR-PRODUCT-CODE}" ADDLOCAL=ALL
```

```powershell
# find the product code
Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*' |
  Where-Object DisplayName -like '*WinFsp*' |
  Select-Object DisplayName, PSChildName
```

If maintenance mode does not open (the cached MSI is sometimes gone), just
re-run the downloaded `.msi` and pick **Change**.

Verify all three pieces:

```powershell
$r = 'C:\Program Files (x86)\WinFsp'
Test-Path "$r\bin\winfsp-x64.dll"      # runtime  — must be True
Test-Path "$r\inc\winfsp\winfsp.h"     # headers  — must be True
Test-Path "$r\lib\winfsp-x64.lib"      # import lib — must be True
```

### JDK and Android SDK

Easiest path is **Android Studio**, which bundles a JDK and manages the SDK.
Otherwise install a JDK 17+ and the command-line tools, then:

```powershell
sdkmanager "platforms;android-36" "build-tools;36.0.0" "platform-tools"
```

Point `ANDROID_HOME` at the SDK if it is not in the default location:

```powershell
$env:ANDROID_HOME = "$env:LOCALAPPDATA\Android\Sdk"
```

---

## Building the Windows host

```powershell
cd windows\afs-host
cargo build --release
```

Output: `target\release\afs-host.exe`. It is self-contained apart from the WinFsp
runtime DLL, which it loads at startup.

The first build is slow — `winfsp-sys` runs bindgen over the WinFsp headers.
Later builds are quick.

### Tests

```powershell
cargo test
```

Covers the wire codec (framing, round-trips, malformed input) and the translation
layer (errno→NTSTATUS, FILETIME conversion, access-mask folding). No device or
WinFsp needed — the crate is split into a library plus a thin binary precisely so
these layers are testable on their own.

### Two build details worth knowing

**`features = ["system"]` on the `winfsp` dependency is required**, and is not in
the crate's default set. Without it `winfsp_init()` compiles out the registry
lookup entirely and falls back to `LoadLibraryW("winfsp-x64.dll")` searching
`PATH` — where WinFsp's `bin` directory does not appear. The symptom is
`could not load the WinFsp DLL` on a machine where WinFsp is installed perfectly
well. Already set in `Cargo.toml`; do not remove it.

**`build.rs` is required.** The `delayload` feature (on by default via `full`)
needs `winfsp::build::winfsp_link_delayload()` called from a build script, or the
binary fails to link.

### Expected warning

```
LINK : warning LNK4199: /DELAYLOAD:winfsp-x64.dll ignored; no imports found
```

Only on the `(lib test)` and `(bin test)` targets during `cargo test`. No unit
test reaches WinFsp, so the linker strips that code and `/DELAYLOAD` has nothing
to attach to. Harmless, and left unsuppressed so genuine linker warnings still
surface. Seeing it on the *release binary* would be a real signal.

---

## Building the Android app

### With Android Studio

Open the `android/` folder, let it sync (it downloads the Gradle wrapper JAR,
which is not checked in), then **Build → Build Bundle(s) / APK(s) → Build APK(s)**.

### From the command line

Needs the wrapper JAR, so run the Studio sync once first — or generate it with a
system Gradle:

```powershell
cd android
gradle wrapper          # only if gradle/wrapper/gradle-wrapper.jar is missing
.\gradlew assembleRelease
```

Output: `app\build\outputs\apk\release\app-release-unsigned.apk`.

For a debug build straight onto a connected phone:

```powershell
.\gradlew installDebug
```

### Signing a release APK

An unsigned APK will not install. For personal use, a self-signed key is fine:

```powershell
keytool -genkey -v -keystore afs.keystore -alias afs `
        -keyalg RSA -keysize 2048 -validity 10000

& "$env:LOCALAPPDATA\Android\Sdk\build-tools\36.0.0\apksigner.bat" sign `
    --ks afs.keystore `
    --out afs.apk `
    app\build\outputs\apk\release\app-release-unsigned.apk
```

Keep the keystore. Android refuses to upgrade an app in place if the signing key
changes — you would have to uninstall first, losing the token file.

### Toolchain versions

Pinned in `android/build.gradle.kts` and `gradle/wrapper/gradle-wrapper.properties`:

| Component            | Version |
|----------------------|---------|
| Android Gradle Plugin| 8.13.0  |
| Kotlin               | 2.2.0   |
| Gradle               | 8.14.3  |
| compileSdk / targetSdk | 36    |
| minSdk               | 30      |

`minSdk = 30` (Android 11) is deliberate: `MANAGE_EXTERNAL_STORAGE` is an API 30
permission and the whole access model depends on it, so supporting less would
mean an app that cannot do its job.

If Android Studio offers to upgrade AGP or Gradle, let it — these are a coherent
recent set, not a hard requirement.

---

## Verifying a build without mounting

The fastest way to confirm the Android half and the wire protocol independently
of WinFsp:

```powershell
cd windows\afs-host
cargo run -- --selftest
```

Sets up the tunnel, reads the token over adb, connects, then exercises `statfs`,
a root listing, write/read/verify, an **in-place patch at offset 512** (the thing
MTP structurally cannot do), and a round-trip latency measurement — then cleans
up after itself.

If the self-test passes but a mounted drive misbehaves, the bug is in the WinFsp
glue, not the protocol. That split is the main reason the self-test exists.

There is also a pre-flight script:

```powershell
.\tools\afs-diag.ps1
```

Checks adb, the device, the **negotiated USB link speed**, the app state, the
token, and the port forward. The USB speed line is the one that sets performance
expectations — see [docs/limits.md](docs/limits.md).

---

## Licensing

winfsp-rs is GPL-3.0 and WinFsp itself is GPLv3 (its author offers a commercial
licence). Linking against them makes `afs-host.exe` GPLv3-derived. Fine for a
personal tool; worth settling before distributing anything built on it.

The Android app has no such constraint — it depends only on AndroidX and the
Kotlin stdlib.

---

## Layout

```
docs/
  protocol.md      wire format, opcodes, status codes, rationale
  limits.md        honest constraints — read before optimising
android/
  app/src/main/kotlin/dev/androidfiles/afs/
    proto/         Protocol.kt (constants), Wire.kt (codec)
    fs/            Paths.kt (traversal guard), Handles.kt, FsOps.kt, MediaScan.kt
    net/           FileServer.kt, Session.kt, AfsService.kt, TokenStore.kt
    ui/            MainActivity.kt
windows/afs-host/
  build.rs             delayload linkage
  src/lib.rs           library root — proto + client are reusable alone
  src/proto.rs         codec mirror of the Kotlin side + tests
  src/client.rs        multiplexed client, reader thread, tag matching
  src/fs.rs            errno→NTSTATUS, FILETIME, access masks + tests
  src/winfsp_glue.rs   FileSystemContext impl, VolumeParams, mount loop
  src/main.rs          thin binary: CLI, adb token fetch, self-test
tools/
  afs-diag.ps1     pre-flight checks incl. USB link speed
  afs-connect.ps1  forward + run, one command
```

Changing the wire protocol means editing **both** `proto/Protocol.kt` and
`src/proto.rs`. They are hand-mirrored; nothing generates one from the other.
