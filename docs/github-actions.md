# Build this fork with GitHub Actions

The **Build AFS** workflow builds both components on every push to `main`.
It can also be started from **Actions → Build AFS → Run workflow**.

Successful runs provide two downloads under **Artifacts**:

- `afs-windows-x64`: `afs-host.exe`, installation instructions, license,
  SHA-256 checksum and source commit.
- `afs-android-signed`: an installable release APK, signing certificate details,
  SHA-256 checksum and source commit.

Artifacts are retained for 30 days. Run the workflow again to rebuild them.
Builds do not install anything on a phone or publish a GitHub Release.

## Build checks

The Windows job installs WinFsp 2.1 including its developer SDK, runs
`cargo test --locked`, builds the release executable and checks `--help`.
The Android job uses JDK 21, Gradle 8.14.3 and Android SDK 36, builds the release
APK, runs Android lint and verifies the APK signature.

The Gradle version matches the upstream wrapper properties. CI installs it
explicitly because the upstream repository does not include a complete wrapper.

## APK signing

The Android job requires these repository Actions secrets:

- `AFS_KEYSTORE_BASE64`: base64-encoded PKCS12 signing keystore.
- `AFS_KEYSTORE_PASSWORD`: the keystore and key password.

The key alias is `afs`. Preserve the same key between builds so Android can
install later builds over existing installations. Keep a private backup of the
keystore and password; never commit them or add them to build artifacts.

## Trying a build

Install the APK, open **AFS Server**, grant all-files access and start the server.
On Windows, install the WinFsp runtime and Android platform-tools, then run:

```powershell
afs-host.exe --mount L:
```

Choose an unused drive letter. Follow `INSTALL.md` for permissions and connection
details. A successful CI build checks compilation and packaging; device behavior
and transfer speed still need a connected-phone test.
