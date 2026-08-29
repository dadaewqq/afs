//! The `winfsp` crate defaults to the `delayload` feature, which links against
//! the system WinFsp DLL lazily. That linkage has to be set up at build time,
//! and without this the binary fails to link.
fn main() {
    winfsp::build::winfsp_link_delayload();
}
