# AFS — Android files as a real Windows drive

Replaces MTP with a filesystem protocol Windows can mount as a drive letter.
An Android foreground service exposes shared storage over a compact binary
protocol; a Rust host on Windows presents it through WinFsp as `Z:\`.

Stock Android, no root. `adb forward` carries the socket over USB.

```
  ANDROID                                    WINDOWS
 ┌────────────────────────┐                 ┌──────────────────────────┐
 │ AfsService             │                 │ afs-host.exe             │
 │  (foreground,          │                 │                          │
 │   specialUse)          │                 │  winfsp_glue             │
 │    │                   │                 │      │                   │
 │  FileServer            │                 │  fs (NTSTATUS, FILETIME) │
 │  127.0.0.1:8577        │◄──adb forward──►│      │                   │
 │    │                   │    over USB     │  client (multiplexed)    │
 │  Session ×N  ──┐       │                 │      │                   │
 │    │           │       │                 │  proto                   │
 │  FsOps ────────┘       │                 └──────────────────────────┘
 │    │                   │                            │
 │  /storage/emulated/0   │                          Z:\
 │  + MediaStore rescan   │                 Explorer, VLC, git, ffmpeg
 └────────────────────────┘
```

## Status

| Component                        | State                                        |
|----------------------------------|----------------------------------------------|
| Wire protocol spec               | Complete — `docs/protocol.md`                |
| Android service (Kotlin)         | Complete                                     |
| Rust codec + client              | Complete, unit-tested                        |
| Rust translation layer           | Complete, unit-tested                        |
| WinFsp binding                   | Implemented against winfsp 0.13.1            |

`afs-host --selftest` exercises the whole protocol against a real phone without
mounting, which is the fastest way to tell a wire problem from a WinFsp problem.

**Licensing:** winfsp-rs is GPL-3.0 and WinFsp itself is GPLv3 (its author offers
a commercial licence). Linking against them makes `afs-host` GPLv3-derived — fine
for a personal tool, worth knowing before building anything redistributable.

## What this actually buys you

Read `docs/limits.md` before investing further, particularly the USB-speed
section. Short version: if your phone negotiates USB 2.0 — most do, including
USB-C ones — large-file copies will not be much faster than MTP, because MTP is
already near the link ceiling. The real wins are structural:

- **Random access.** Seek, partial read, in-place write. MTP cannot do any of it.
- **A real drive letter.** `CreateFile` works, so every application can open files
  on the phone. MTP is a shell namespace extension, invisible to normal file APIs.
- **Small files.** Batched directory listings and pipelined requests instead of
  MTP's per-object round trips.
- Atomic rename, honest sizes and timestamps, no phantom copies.

## Prerequisites

Present on this machine already: **adb**, **Java 21**, **Android SDK**
(platforms 35/36/36.1, build-tools to 37).

Still needed:

| Tool   | Why                        | Install                                       |
|--------|----------------------------|-----------------------------------------------|
| Rust   | builds `afs-host`          | <https://rustup.rs>                           |
| WinFsp | only for the mount step    | <https://winfsp.dev> — not needed for selftest |

## Build and run

> Nothing below has been compiled or executed — please run it yourself.

### 1. Android app

Open `android/` in Android Studio and let it sync (it will fetch the Gradle
wrapper jar, which is not checked in). Then build and install, or from a shell
once the wrapper exists:

```powershell
cd android
.\gradlew installDebug
```

The AGP/Kotlin/Gradle versions in `android/build.gradle.kts` and
`gradle/wrapper/gradle-wrapper.properties` are a coherent recent set but are not
verified against what your Studio ships — let Studio bump them if it offers.

On the phone: open the app, tap **Grant all-files access** (this opens a Settings
page — the app has to be found in a list, there is no one-tap grant), then
**Start server**.

### 2. Check the link

```powershell
.\tools\afs-diag.ps1
```

Checks the device, the negotiated USB speed, the app state, the token and the
port forward. The USB speed line is the one that sets expectations.

### 3. Self-test the protocol

```powershell
cd windows\afs-host
cargo test                 # codec + translation unit tests, no device needed
cargo run -- --selftest
```

The self-test sets up the forward, fetches the token over adb, connects, then
exercises statfs, a root listing, write/read/verify, an **in-place patch at
offset 512** (the thing MTP structurally cannot do), and a round-trip latency
measurement. It cleans up after itself.

If this passes, the Android half and the wire are correct — any later mount
misbehaviour is in the WinFsp glue.

### 4. Mount

```powershell
cargo run --release -- --mount Z:
```

#### "WinFsp is not installed" when it plainly is

The `winfsp` crate needs `features = ["system"]`, which is **not** in its default
set. Without it `winfsp_init()` compiles the registry lookup out entirely and
falls back to `LoadLibraryW("winfsp-x64.dll")` against `PATH`, where WinFsp's
`bin` directory does not appear. With it, the crate reads
`HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir` and loads
`<InstallDir>\bin\winfsp-x64.dll` — where the installer actually puts it.

Already set in `Cargo.toml`. Noted here because the failure looks like a broken
WinFsp install and is nothing of the sort.

#### Expected warning during `cargo test`

```
LINK : warning LNK4199: /DELAYLOAD:winfsp-x64.dll ignored; no imports found
```

Only on the `(lib test)` and `(bin test)` targets. No unit test calls into WinFsp,
so the linker dead-strips `winfsp_glue` entirely, leaving nothing for the
`/DELAYLOAD` directive to apply to. It is not suppressed deliberately — an
`#[allow(linker_messages)]` would also hide genuine linker warnings later.

It cannot mask a broken link: a binary actually missing its WinFsp imports fails
with unresolved externals rather than a warning. Seeing LNK4199 on the *release
binary* line would be a real signal, since `mount()` is reachable from `main`.

## Layout

```
docs/
  protocol.md      wire format, opcodes, status codes, rationale
  limits.md        honest constraints — read this
android/
  app/src/main/kotlin/dev/androidfiles/afs/
    proto/         Protocol.kt (constants), Wire.kt (codec)
    fs/            Paths.kt (traversal guard), Handles.kt, FsOps.kt, MediaScan.kt
    net/           FileServer.kt, Session.kt, AfsService.kt, TokenStore.kt
    ui/            MainActivity.kt
windows/afs-host/
  src/lib.rs           library root — proto + client are reusable on their own
  src/proto.rs         codec mirror + tests
  src/client.rs        multiplexed client, reader thread, tag matching
  src/fs.rs            errno→NTSTATUS, FILETIME, open flags + tests
  src/adb.rs           adb discovery, port forward with automatic teardown
  src/winfsp_glue.rs   FileSystemContext impl, VolumeParams, mount loop
  src/main.rs          thin binary: CLI, Ctrl+C handling, self-test
tools/
  afs-diag.ps1     pre-flight checks incl. USB link speed
  afs-connect.ps1  forward + run, one command
```

## Design notes worth knowing

**Pipelining is the point.** WinFsp dispatches callbacks from many threads. The
protocol tags every request so replies can return out of order; the Android side
dispatches to a worker pool and serialises only the socket write. Serialising
anywhere in that chain would reintroduce exactly the bottleneck that makes MTP
slow.

**`READDIR` returns attributes inline.** Windows' `FindFirstFile` wants
name+attributes together. Returning names alone would force one `GETATTR` per
file — the N+1 pattern behind MTP's collapse on folders full of small files.

**One `stat()` per entry, not nine.** `/storage/emulated/0` is a FUSE mount, so
every `File.exists()`, `.isDirectory()`, `.length()`, `.lastModified()` and
`.canWrite()` is a separate trip through the FUSE daemon. Spelled that way a
2,000-file listing cost ~18,000 FUSE operations — seconds of latency. The server
uses a single `Files.readAttributes` instead, which is one `stat()` carrying
kind, size and all three timestamps.

**Caching is the other half.** `dir_info_timeout` lets WinFsp answer repeat
enumerations without reaching us at all — which is what makes next/previous in an
image viewer fast, since viewers re-read the folder on every navigation. Tunable
with `--cache-ms`; the only cost of a high value is how long a change made on the
phone takes to appear.

**Library plus thin binary.** `proto` and `client` are a complete,
transport-agnostic implementation of the protocol, independent of WinFsp and of
the mount host. Keeping them in the library half means they are usable by
integration tests or any other front end — and it is why the crate compiled clean
while the WinFsp binding was still a stub, instead of needing `#[allow(dead_code)]`
scattered over the parts that had no consumer yet.

**Loopback only.** The server binds `127.0.0.1`, so it is reachable solely through
`adb forward`. Binding `0.0.0.0` would put all of shared storage on every Wi-Fi
network the phone joins.

**adb is driven, not required of the user.** The host finds adb itself — `PATH`,
`ANDROID_HOME`, the default SDK path, winget/scoop/chocolatey — then creates the
forward on start and removes it on exit. It adopts an existing forward rather than
recreating one, so it never tears down a tunnel it did not set up. Ctrl+C is
trapped for the same reason: an abrupt exit would skip both the unmount and the
teardown, leaving a dead drive letter behind.

**`specialUse`, not `dataSync` or `connectedDevice`.** `dataSync` fits
semantically but Android 15 caps it at 6 hours per 24h and then force-stops the
service, dropping a mounted drive out from under Explorer mid-copy.
`connectedDevice` looks like the natural fit and is not: the platform requires one
of BLUETOOTH_*/NFC/UWB/CHANGE_WIFI_STATE/CHANGE_NETWORK_STATE or a claimed
`UsbDevice`, none of which we hold — our device is behind an adb socket the
platform cannot see — and it throws `SecurityException` from `startForeground()`.
`specialUse` is the category for a legitimate long-running case matching none of
the predefined types; it has no timeout and needs no permission we don't use.

**MediaStore rescan on close.** Raw writes to shared storage are invisible to the
Gallery until scanned. MTP gets this free by writing through `MediaProvider`; we
have to do it by hand, debounced so a 500-photo copy does not fire 500 scans.
