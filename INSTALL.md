# Installing AFS

For someone who already has the two artifacts:

- `afs.apk` — the Android server
- `afs-host.exe` — the Windows client

Building them from source is [BUILD.md](BUILD.md).

Total time: about ten minutes, most of it waiting on the WinFsp installer.

---

## 1. Windows: install WinFsp

`afs-host.exe` mounts the drive through WinFsp, which ships a kernel driver.
Download the current release from <https://winfsp.dev/rel/> and run it.

**The default "Core" install is all you need.** The optional Developer component
carries headers and import libraries used only at compile time — installing it
does no harm, but do not go looking for it.

The installer needs administrator rights and may ask for a reboot. Verify:

```powershell
Test-Path 'C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll'   # must print True
```

## 2. Windows: install adb

The connection is a TCP socket tunnelled over USB by `adb forward`. `afs-host.exe`
runs adb for you — sets the tunnel up on start, tears it down on exit — but adb
itself has to exist somewhere on the machine.

You may already have it: Android Studio, scrcpy and the platform-tools package all
ship one. **It does not need to be on `PATH`** — the host looks in `PATH`,
`ANDROID_HOME` / `ANDROID_SDK_ROOT`, the default SDK location, and the winget,
scoop and chocolatey package directories.

If you have none of those, download the SDK Platform Tools from
<https://developer.android.com/tools/releases/platform-tools> and unzip it
somewhere permanent. That location is found automatically if it is the standard
SDK path; otherwise point at it directly:

```powershell
afs-host.exe --adb C:\platform-tools\adb.exe --mount Z:
```

or set `AFS_ADB` to the full path once and forget about it.

## 3. Phone: enable USB debugging

1. **Settings → About phone** → tap **Build number** seven times.
2. **Settings → System → Developer options** → enable **USB debugging**.
3. Plug the phone in. Choose **Allow** on the "Allow USB debugging?" prompt, and
   tick *Always allow from this computer*.

Confirm the PC can see it (using whichever adb you have):

```powershell
adb devices
```

One device listed as `device`. If it says `unauthorized`, the prompt on the phone
has not been accepted yet. If nothing is listed, try a different cable — charge-only
cables are common and carry no data lines.

## 4. Phone: install the app

```powershell
adb install afs.apk
```

Or copy the APK to the phone and open it, allowing installation from unknown
sources when prompted.

## 5. Phone: grant all-files access

Open **AFS Server** and tap **Grant all-files access**.

This opens a full-screen system Settings page rather than a normal permission
dialog — Android does not allow one-tap granting for this one. Find **AFS Server**
in the list and turn the toggle on, then press Back.

The app's status line should now read `All-files access: granted`.

> **What this does not cover:** `/Android/data` and `/Android/obb` stay
> inaccessible even with this granted. Android 11 walled them off and no
> permission reopens them. See [docs/limits.md](docs/limits.md).

## 6. Phone: start the server

Tap **Start server**. A persistent notification appears reading
*AFS server running*.

The server listens on `127.0.0.1:8577` — loopback only, reachable solely through
the USB cable. It is not exposed to any Wi-Fi network the phone is on.

## 7. Windows: mount

```powershell
afs-host.exe --mount Z:
```

That is the whole command. It finds adb, sets up the tunnel, reads the token off
the device, connects and mounts:

```
[INFO  afs_host::adb] adb forward tcp:8577 -> tcp:8577
[INFO  afs_host] connecting to 127.0.0.1:8577
[INFO  afs_host] connected: volume 'Android', 464.1 GiB total, 246.3 GiB free
[INFO  afs_host::winfsp_glue] mounted at Z: — press Ctrl+C to unmount
```

`Z:\` is now a normal drive. Explorer, `dir`, VLC, git, ffmpeg — anything that
opens files can use it.

**Leave the window open.** Ctrl+C unmounts the drive and removes the port forward.

Pick another letter if Z: is taken (`afs-host.exe --mount Y:`).

---

## Everyday use

Once installed, reconnecting is:

1. Plug in the phone.
2. Open the app, tap **Start server**.
3. `afs-host.exe --mount Z:`

Unplugging the cable unmounts the drive automatically — the host notices the
connection drop and cleans up rather than leaving a dead drive letter that hangs
every process which touches it. Ctrl+C does the same thing deliberately, and also
removes the `adb forward` it created.

### Options

| Flag           | Effect                                                     |
|----------------|------------------------------------------------------------|
| `--mount Z:`   | Drive letter, or a path to mount into a folder             |
| `--adb <path>` | Use a specific adb instead of searching for one            |
| `--port N`     | Tunnel port, both ends (default 8577)                      |
| `--token <t>`  | Supply the token instead of reading it from the device     |
| `--cache-ms N` | Metadata cache lifetime, default 15000 (see below)         |
| `--no-forward` | Skip `adb forward` — the tunnel is already set up elsewhere |
| `--selftest`   | Exercise the protocol without mounting                     |
| `-v`           | Debug logging, including where adb was found               |

`AFS_ADB` is honoured as an alternative to `--adb`.

### Tuning `--cache-ms`

Windows may reuse cached file attributes and directory listings for this long
before asking again. It is the main speed/freshness dial.

- **Browsing photos or media, or any big folder:** raise it. `--cache-ms 30000`
  makes an image viewer's next/previous essentially instant, because the folder
  is not re-read on every keypress.
- **Actively changing files on the phone while the drive is mounted:** lower it.
  `--cache-ms 1000` surfaces changes within a second.

The cost of a high value is only staleness from the **phone's** side: a photo
taken while the drive is mounted may take that long to appear in Explorer.
Nothing is lost or corrupted, and anything you create, delete, rename or write
from Windows drops the affected listing immediately — so the drive never looks
stale as a result of your own actions, whatever the TTL.

## What to expect

Read [docs/limits.md](docs/limits.md) before forming expectations about speed.
The short version:

**Big-file copies may not be much faster than MTP.** Most phones, including many
with USB-C connectors, only wire USB 2.0 to the port, which caps the link near
40 MB/s — and MTP already runs at 15–35 MB/s. Check yours:

```powershell
adb shell "cat /sys/class/udc/*/current_speed"
```

`super-speed` or better means real headroom. `high-speed` means USB 2.0 and a
hard ceiling no protocol can lift.

**What improves regardless of link speed:**

- **Random access.** Seek, partial reads, in-place writes. Play a video straight
  off the phone and scrub through it; edit a file in place without a full
  download-and-re-upload cycle. MTP cannot do any of this.
- **A real drive letter.** Every application can open `Z:\...` because it goes
  through the normal file API. MTP is a shell namespace extension, invisible to
  everything except Explorer and a handful of programs written against it.
- **Folders full of small files.** Directory listings arrive in batches and
  requests are pipelined, instead of MTP's per-object round trips.
- Atomic renames, correct file sizes, honest timestamps.

---

## Troubleshooting

### "could not load the WinFsp DLL"

WinFsp is not installed, or the install is broken. Re-check:

```powershell
Test-Path 'C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll'
```

If that is `True` and the error persists, the binary was built without the
`system` feature — see [BUILD.md](BUILD.md).

### "could not find adb"

No adb was found in any of the searched locations. Install the platform-tools
(step 2), or point at an existing one:

```powershell
afs-host.exe --adb C:\path\to\adb.exe --mount Z:
```

Run with `-v` to see exactly where it looked.

### "no token at /sdcard/.afs/token on the device"

The app has not been run yet, or all-files access is not granted — the token file
is written on first launch. Open the app, grant access, then retry.

To check by hand:

```powershell
adb shell cat /sdcard/.afs/token
```

A 48-character hex string. If it says *No such file*, open the app once.

### "connect failed" / nothing listening on 127.0.0.1:8577

The forward is set up but nothing is answering on the phone's end, which means the
server is not running. Open the app and tap **Start server** — check for the
notification — then retry.

### The app crashes or the notification vanishes

Android may have killed the service under memory pressure. Reopen the app and
start it again. If it happens repeatedly, exempt the app from battery
optimisation: **Settings → Apps → AFS Server → Battery → Unrestricted**.

### Files copied to the phone do not appear in the Gallery

Give it a second — media scanning is debounced. If they still do not appear,
the file landed somewhere the Gallery does not index; try `DCIM/` or `Pictures/`.

### Browsing a folder is slow

Run with `-v` and watch for lines like:

```
enumerated 2143 entries in 'DCIM/Camera' in 1.2s
```

That is the cost of reading the folder from the phone. If it appears **once** and
browsing is fast afterwards, the cache is doing its job — raise `--cache-ms` to
widen the window. If it appears on *every* navigation, the cache is expiring
between them; raise `--cache-ms` substantially (`30000`).

If a single enumeration is itself slow, the folder is simply large. The phone's
storage layer charges per file and there is no way around that on the first read
of a folder — only on the repeats.

```powershell
net use Z: /delete
```

If that fails, unplugging the phone and running `afs-host.exe` once more to let
it exit cleanly usually clears it.

### `dir Z:\` repeats the same folder forever

Fixed — that was a bug in the directory-entry encoding. If you see it, your
`afs-host.exe` predates the fix; rebuild or get a newer binary.
