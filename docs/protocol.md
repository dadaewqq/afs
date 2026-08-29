# AFS wire protocol v1

A request/response protocol between `afs-host.exe` (Windows, WinFsp) and the
Android foreground service. One TCP connection carries many concurrent requests;
`adb forward` bridges it over USB.

All integers are **little-endian**. All paths are **UTF-8, forward-slash
separated, relative to the export root**, with no leading slash and no `.` or
`..` component. The server rejects anything else with `EINVAL` — this is the
path-traversal guard, and it is enforced server-side, never trusted from the host.

## Framing

Every message, in both directions, is:

```
u32  length     // byte count of everything AFTER this field
u32  tag        // request id, echoed in the response
u16  op         // opcode; echoed in the response
i16  status     // request: 0. response: 0 = OK, else negative errno
     payload    // op-specific, (length - 8) bytes
```

The 12-byte header is fixed. `length` caps at `max_chunk + 4096` (negotiated in
`INIT`); a larger frame is a protocol violation and the peer drops the connection.

### Pipelining

`tag` exists because WinFsp dispatches filesystem callbacks on many threads at
once. The host assigns each in-flight request a unique tag and matches responses
back by it. **Responses may return out of order.** A server that serialised
request handling would reintroduce exactly the bottleneck that makes MTP slow, so
the read loop dispatches to a worker pool and only the socket *write* is
serialised.

## Primitives

| Name    | Encoding                            |
|---------|-------------------------------------|
| `str`   | `u16` byte length, then UTF-8 bytes |
| `bytes` | `u32` byte length, then raw bytes   |
| `attr`  | 45-byte fixed struct, below         |

### `attr` (45 bytes)

```
u8   kind       // 0 = file, 1 = dir, 2 = symlink, 3 = other
u32  mode       // POSIX permission bits, informational only
u64  size
i64  mtime_ms   // Unix epoch milliseconds
i64  ctime_ms
i64  atime_ms
u64  ino        // stable within a session; synthesised from the path hash
```

Android's storage layer does not expose real inode numbers through the paths we
can reach, so `ino` is a 64-bit hash of the canonical path. It is stable for as
long as a file keeps its name, which is all WinFsp needs it for.

## Opcodes

| Code     | Name       | Request                                             | Response                                                     |
|----------|------------|-----------------------------------------------------|--------------------------------------------------------------|
| `0x0001` | `INIT`     | `u32 version, str token, u32 max_chunk`             | `u32 version, u32 max_chunk, str label, u64 total, u64 free` |
| `0x0002` | `STATFS`   | —                                                   | `u64 total, u64 free, u64 avail, u32 bsize`                  |
| `0x0010` | `GETATTR`  | `str path`                                          | `attr`                                                       |
| `0x0011` | `READDIR`  | `str path, u64 cookie, u32 max_entries`             | `u8 eof, u64 next_cookie, u32 count, [str name, attr]*`      |
| `0x0012` | `OPEN`     | `str path, u32 flags`                               | `u64 handle, attr`                                           |
| `0x0013` | `CREATE`   | `str path, u32 flags, u32 mode`                     | `u64 handle, attr`                                           |
| `0x0014` | `CLOSE`    | `u64 handle`                                        | —                                                            |
| `0x0015` | `READ`     | `u64 handle, u64 offset, u32 len`                   | `bytes`                                                      |
| `0x0016` | `WRITE`    | `u64 handle, u64 offset, bytes`                     | `u32 written`                                                |
| `0x0017` | `TRUNCATE` | `u64 handle, u64 size`                              | —                                                            |
| `0x0018` | `FSYNC`    | `u64 handle`                                        | —                                                            |
| `0x0020` | `MKDIR`    | `str path, u32 mode`                                | `attr`                                                       |
| `0x0021` | `RMDIR`    | `str path`                                          | —                                                            |
| `0x0022` | `UNLINK`   | `str path`                                          | —                                                            |
| `0x0023` | `RENAME`   | `str from, str to, u32 flags`                       | —                                                            |
| `0x0024` | `SETATTR`  | `str path, u32 mask, u32 mode, u64 size, i64 mtime` | `attr`                                                       |
| `0x0030` | `SCAN`     | `str path`                                          | —                                                            |

### Why `READDIR` returns attributes inline

This is the single biggest win over MTP. Windows' `FindFirstFile` / `FindNextFile`
hands back a full `WIN32_FIND_DATA` per entry, so a directory listing needs name
*and* attributes together. Returning names alone would force one `GETATTR`
round-trip per file — the N+1 pattern that makes MTP collapse to a few hundred
KB/s on directories full of small files. Batching name+attr means listing a
5,000-file folder is a handful of round trips instead of 5,000.

`cookie` is 0 on the first call and echoed from `next_cookie` thereafter. The
server snapshots the directory on the first call and holds it against the cookie,
so a listing stays consistent even if the directory changes mid-enumeration.

### `OPEN` / `CREATE` flags

| Bit | Meaning  |
|-----|----------|
| 1   | `READ`   |
| 2   | `WRITE`  |
| 4   | `APPEND` |
| 8   | `CREATE` |
| 16  | `EXCL`   |
| 32  | `TRUNC`  |

### `SETATTR` mask

| Bit | Field   |
|-----|---------|
| 1   | `mode`  |
| 2   | `size`  |
| 4   | `mtime` |

### `SCAN`

Has no POSIX equivalent and exists purely for Android. Writes that land on
`/storage/emulated/0` through raw file I/O are invisible to `MediaStore`, so the
Gallery and every other media app will not see them until a rescan. The server
fires `MediaScannerConnection.scanFile()` automatically when a handle that was
opened for writing is closed; `SCAN` lets the host force it for a path
explicitly. MTP gets this for free because it writes through `MediaProvider` —
we have to do it by hand.

## Status codes

Negative errno. The host maps these to `NTSTATUS`.

| Value | Name        | NTSTATUS                       |
|-------|-------------|--------------------------------|
| `0`   | OK          | `STATUS_SUCCESS`               |
| `-1`  | `EPERM`     | `STATUS_ACCESS_DENIED`         |
| `-2`  | `ENOENT`    | `STATUS_OBJECT_NAME_NOT_FOUND` |
| `-5`  | `EIO`       | `STATUS_IO_DEVICE_ERROR`       |
| `-9`  | `EBADF`     | `STATUS_INVALID_HANDLE`        |
| `-13` | `EACCES`    | `STATUS_ACCESS_DENIED`         |
| `-17` | `EEXIST`    | `STATUS_OBJECT_NAME_COLLISION` |
| `-20` | `ENOTDIR`   | `STATUS_NOT_A_DIRECTORY`       |
| `-21` | `EISDIR`    | `STATUS_FILE_IS_A_DIRECTORY`   |
| `-22` | `EINVAL`    | `STATUS_INVALID_PARAMETER`     |
| `-28` | `ENOSPC`    | `STATUS_DISK_FULL`             |
| `-39` | `ENOTEMPTY` | `STATUS_DIRECTORY_NOT_EMPTY`   |
| `-95` | `ENOTSUP`   | `STATUS_NOT_SUPPORTED`         |

## Handshake and authentication

`INIT` must be the first message on a connection; anything else is refused with
`EPERM`. It carries a token that the app generates on first run and stores in
`/sdcard/.afs/token`.

The token matters because `adb forward` opens a listening socket on the Windows
machine's loopback interface, and **any local process can connect to it** — the
forward carries no identity of its own. The token narrows that to callers who can
already read the file over adb, which is the same trust boundary as USB debugging
itself. The server binds `127.0.0.1` only, so nothing off-device can reach it
even while the phone is on Wi-Fi.

A version mismatch in `INIT` is fatal: the server replies with its own version and
closes. There is no negotiation beyond `max_chunk`, which resolves to
`min(host_request, server_limit)`.
