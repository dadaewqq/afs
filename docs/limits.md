# Known limits

Written down deliberately. Most of these are not bugs to be fixed later — they
are consequences of choosing a stock, unrooted device, and it is cheaper to know
them now than to discover them halfway through.

## `/Android/data` and `/Android/obb` are unreachable

`MANAGE_EXTERNAL_STORAGE` grants everything under `/storage/emulated/0` **except**
these two directories. Android 11 walled them off specifically, and the "all files
access" permission does not reopen them. There is no workaround at this privilege
level.

If reaching them becomes a requirement, the options are:

- **Shizuku** — delegates the ADB shell UID to the app. Reaches both directories,
  no root, but the user must re-arm it after every reboot.
- **Root** — reaches everything, obviously.

The `FsOps` layer takes its root as a `File`, so a privileged backend would slot in
behind the same interface without disturbing the protocol or the host.

## The USB link is probably the real ceiling

Run `tools/afs-diag.ps1`. If it reports `high-speed`, the phone negotiated USB 2.0
and the link caps around 40 MB/s — many phones wire only USB 2.0 to the connector
even when it is USB-C. MTP already achieves 15–35 MB/s on large files, so **this
project will not make big-file copies dramatically faster on such a device.**

What it does deliver regardless of link speed:

- Random access — seek, partial read, in-place write. MTP has none of these, and
  it is why you cannot open a file on the phone directly in an editor or player.
- A real drive letter. Every application can use `Z:\...` because it goes through
  the normal filesystem API; MTP is a shell namespace extension, so `CreateFile`
  never sees it.
- Small-file throughput, via batched `READDIR` and pipelined requests, instead of
  MTP's per-object round trips.
- Atomic rename, real file sizes, honest timestamps.

Set expectations accordingly, and lead with the semantics rather than the MB/s.

## `/storage/emulated/0` is itself a FUSE mount

Shared storage on modern Android goes through a FUSE daemon that enforces scoped
storage per-app. Every read and write we do pays that overhead before touching
the real filesystem. It is measurable, it applies to MTP equally, and there is
nothing to be done about it from userspace.

## MediaStore needs telling

Raw writes are invisible to the Gallery and every other media app until a rescan.
`MediaScan` handles this by scanning written paths on handle close, debounced.
MTP does not need it because it writes through `MediaProvider`.

Consequence: a file copied to the phone appears in the filesystem instantly but
may take up to a second to show in the Gallery. Deleted files rely on the same
mechanism to have their stale rows dropped.

## adb is a hard dependency

The transport is a TCP socket bridged by `adb forward`. That means USB debugging
must be on and the host authorised. It also means:

- The `adb` binary must be on `PATH` on the PC.
- Only one adb server can own the device; a running Android Studio or scrcpy is
  fine (they share the server), but a second adb of a different version is not.
- Unplugging the cable drops the connection. The host reports
  `STATUS_DEVICE_NOT_CONNECTED` so Windows treats it as a removed device rather
  than a corrupt one, but anything mid-copy fails.

Moving to a raw USB bulk endpoint (AOA) later would remove the adb dependency at
the cost of a WinUSB driver INF. The protocol is transport-agnostic; only
`Client::connect` would change.

## Cross-volume rename fails

`File.renameTo` cannot move between volumes, and the server refuses rather than
silently doing a non-atomic copy-and-delete that would stall the request for the
duration of a large file. Explorer falls back to its own copy loop, which at least
shows a progress bar.

## No symlinks, no ACLs, no alternate data streams

The server reports a synthetic POSIX mode and the host maps only the read-only
bit. Windows security descriptors are not modelled; every file is presented as
world-accessible to the mounting user. This is normal for a WinFsp filesystem
backing a device that has no meaningful concept of Windows ACLs.

## Single connection

One host, one connection, one handle table. A second `afs-host.exe` against the
same device gets its own session and its own handles, which works but means two
mounts do not share caches. Nothing enforces exclusivity.

## The token is a speed bump, not a security model

It stops an unrelated local process from wandering onto the adb-forwarded port.
It does not protect against anyone who can already run adb against the device —
by design, since that is the trust boundary we inherit. Shared storage has no
real permission bits to lean on.
