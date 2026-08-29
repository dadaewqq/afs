use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Result};
use clap::Parser;

// The modules live in the library half of this crate (src/lib.rs), so they are
// public API rather than binary-internal dead code.
use afs_host::adb;
use afs_host::client::{self, Client};
use afs_host::proto::open_flag;
use afs_host::winfsp_glue;

#[derive(Parser, Debug)]
#[command(
    name = "afs-host",
    version,
    about = "Mounts an Android device's storage as a Windows drive"
)]
struct Args {
    /// Drive letter or directory to mount on, e.g. Z: — omit with --selftest.
    #[arg(long)]
    mount: Option<String>,

    /// Local port to tunnel over USB.
    #[arg(long, default_value_t = 8577)]
    port: u16,

    /// Auth token. Omit to read it from the device.
    #[arg(long)]
    token: Option<String>,

    /// Path to adb. Omit to discover it automatically.
    #[arg(long)]
    adb: Option<PathBuf>,

    /// How long Windows may cache metadata and directory listings, in ms.
    ///
    /// Only bounds how long a change made *on the phone* goes unnoticed —
    /// anything written from Windows invalidates the cache immediately. Raise it
    /// for browsing large folders; lower it if the phone is actively writing.
    #[arg(long, default_value_t = 15000)]
    cache_ms: u32,

    /// Skip `adb forward` — for when the tunnel is already set up by other means.
    #[arg(long)]
    no_forward: bool,

    /// Connect and exercise the protocol instead of mounting.
    #[arg(long)]
    selftest: bool,

    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    env_logger::Builder::new()
        .filter_level(if args.verbose {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .init();

    if args.mount.is_none() && !args.selftest {
        bail!("--mount is required unless --selftest is given");
    }

    let adb_path = adb::find_adb(args.adb.as_deref())?;
    log::debug!("using adb at {}", adb_path.display());

    // Declared before `client` so it is torn down after it, and held for the whole
    // run so Drop removes the forward on every exit path — including the `?`
    // returns below and a Ctrl+C that unblocks the mount loop.
    let _forward = if args.no_forward {
        None
    } else {
        Some(adb::Forward::create(&adb_path, args.port)?)
    };

    let token = match &args.token {
        Some(t) => t.clone(),
        None => adb::read_token(&adb_path)?,
    };

    let addr = format!("127.0.0.1:{}", args.port);
    log::info!("connecting to {addr}");
    let client = Arc::new(
        Client::connect(&addr, &token).map_err(|e| anyhow::anyhow!("connect failed: {e}"))?,
    );
    log::info!(
        "connected: volume '{}', {:.1} GiB total, {:.1} GiB free",
        client.volume_label,
        client.total_bytes as f64 / (1 << 30) as f64,
        client.free_bytes as f64 / (1 << 30) as f64,
    );

    if args.selftest {
        return selftest(&client);
    }

    let mount = args.mount.expect("checked above");

    // Ctrl+C must not kill the process outright: that would skip both the unmount
    // and the forward teardown, leaving a dead drive letter that hangs anything
    // touching it. Flip a flag instead and let the mount loop wind down normally.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = Arc::clone(&shutdown);
        if let Err(e) = ctrlc::set_handler(move || {
            log::info!("interrupt received — unmounting");
            shutdown.store(true, Ordering::SeqCst);
        }) {
            log::warn!("could not install the Ctrl+C handler: {e}");
        }
    }

    let opts = winfsp_glue::MountOptions {
        mount_point: mount,
        cache_ms: args.cache_ms,
    };
    winfsp_glue::mount(client, &opts, shutdown)
}

/// Exercises the protocol end to end without WinFsp.
///
/// If this passes and a mounted drive misbehaves, the bug is in the WinFsp glue
/// rather than the wire — which is the most useful thing to know when something
/// is wrong.
fn selftest(client: &Client) -> Result<()> {
    let err = |e: client::AfsError| anyhow::anyhow!("{e}");

    println!("\n== statfs ==");
    let (total, free, avail, bsize) = client.statfs().map_err(err)?;
    println!("  total={total} free={free} avail={avail} bsize={bsize}");

    println!("\n== readdir of root ==");
    let entries = client.readdir("").map_err(err)?;
    println!("  {} entries", entries.len());
    for (name, attr) in entries.iter().take(10) {
        let kind = if attr.is_dir() { "dir " } else { "file" };
        println!("  {kind} {:>12}  {name}", attr.size);
    }
    if entries.len() > 10 {
        println!("  ... and {} more", entries.len() - 10);
    }

    println!("\n== write / read / verify ==");
    let path = ".afs/selftest.bin";
    let payload: Vec<u8> = (0..256u32).flat_map(|i| i.to_le_bytes()).collect();

    let (h, _) = client
        .create(
            path,
            open_flag::WRITE | open_flag::CREATE | open_flag::TRUNC,
            0o644,
        )
        .map_err(err)?;
    let n = client.write(h, 0, &payload).map_err(err)?;
    client.fsync(h).map_err(err)?;
    client.close(h).map_err(err)?;
    println!("  wrote {n} bytes to {path}");

    let (h, attr) = client.open(path, open_flag::READ).map_err(err)?;
    let back = client.read(h, 0, payload.len() as u32).map_err(err)?;
    client.close(h).map_err(err)?;

    if back != payload {
        bail!(
            "read-back mismatch: wrote {} bytes, got {}",
            payload.len(),
            back.len()
        );
    }
    println!(
        "  read back {} bytes, contents match (size={})",
        back.len(),
        attr.size
    );

    println!("\n== partial write at offset (the thing MTP cannot do) ==");
    let (h, _) = client
        .open(path, open_flag::READ | open_flag::WRITE)
        .map_err(err)?;
    client.write(h, 512, b"PATCHED!").map_err(err)?;
    let patched = client.read(h, 512, 8).map_err(err)?;
    client.close(h).map_err(err)?;
    if patched != b"PATCHED!" {
        bail!("in-place patch failed: got {patched:?}");
    }
    println!("  patched 8 bytes at offset 512 without rewriting the file");

    println!("\n== sequential read throughput ==");
    let chunk = client.max_chunk();
    let (h, _) = client.open(path, open_flag::READ).map_err(err)?;
    let start = Instant::now();
    let mut total_read = 0u64;
    for _ in 0..64 {
        let b = client.read(h, 0, chunk).map_err(err)?;
        total_read += b.len() as u64;
    }
    client.close(h).map_err(err)?;
    let secs = start.elapsed().as_secs_f64();
    println!(
        "  64 round trips, {:.1} KiB, {:.1} req/s",
        total_read as f64 / 1024.0,
        64.0 / secs
    );
    println!("  (small file, so this measures round-trip latency, not bandwidth)");

    println!("\n== cleanup ==");
    client.unlink(path).map_err(err)?;
    println!("  removed {path}");

    println!("\nAll self-tests passed.");
    Ok(())
}
