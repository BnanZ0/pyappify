# MirrorChyan NSIS updates

## Configuration and prerequisites

Configure `mirrorchyan` at the root of `pyappify.yml`, alongside `name` and
`profiles`, not inside a profile:

```yaml
mirrorchyan:
  resource_id: "YOUR_RESOURCE_ID"
  stable_channel: "stable"
  # prerelease_channel: "beta"
```

`resource_id` is the resource registered with MirrorChyan. Use the resource's
actual channel names. Stable is the default; without `prerelease_channel`, the
existing automatic-prerelease preference uses stable releases, and Settings
explains this limitation. Windows and the launcher architecture are detected
automatically and sent using MirrorChyan's `win` and `x64`/`arm64` values. If no
matching platform storage exists, the launcher retries without OS and architecture
for resources uploaded as generic packages.

The selected resource must return the complete NSIS installer, not a ZIP,
incremental package, source archive, or a bare launcher executable. The installer
must retain the application identity, main executable name and installation
layout, and use the NSIS template with `/INSTALLERHELPER` support. Existing
installers without that support must be rebuilt before use with this workflow.
Publishing/uploading that installer is external to this feature; the existing packaging workflow and
pyappify-action are not changed.

Users select **Git + pip** or **Mirror酱** in Settings. Git remains the default.
Switching sources does not install immediately. Checking again updates the
available release; automatic installation follows the saved startup policy on
the next launch. The MirrorChyan mode offers the latest release in its channel,
not Git's history or downgrade list. Installers for different dependency profiles
must be supplied as matching complete setups; changing a profile through the
Git/pip setup operation is not supported in MirrorChyan mode.

## Runtime behavior

Version checks use the [official MirrorChyan API](https://github.com/MirrorChyan/docs).
They work without a CDK. Downloads require an API-provided download URL, normally
requiring a valid CDK. API failures never silently switch to Git.

Manual updates download first, then use the launcher's existing application-stop
mechanism. Automatic startup updates wait until the application stops, including
when the launcher is minimized to the tray. They recheck before installation; if
an application starts during download, installation is deferred instead of
stopping that application. Concurrent updates are rejected.

Downloads use HTTPS with bounded redirects and timeouts. A link returning
401/403/410 is refreshed once. Incomplete transfers, invalid PE headers, HTML/ZIP
responses, and mismatched API `sha256` values are rejected before execution. A PE
check is a format check, not a publisher-signature check; when the API supplies
no checksum, the download relies on HTTPS and the configured resource.

Download progress is sent through `installer-update-progress` with application
name, phase, byte count and optional total size. Git/pip log parsing is not used
for this progress bar.

The launcher copies itself into a unique Windows temporary directory and starts
that copy with `--installer-helper`. This internal mode runs before Tauri or
single-instance initialization. A ready/commit handshake and a Windows process
handle ensure NSIS starts only after the original launcher commits while it is
still running. Closing the launcher before committing does not start the installer.

The helper runs setup with
`/S /UPDATE /INSTALLERHELPER /LAUNCHER="<original executable name>" /D=<original directory>`.
`/D=` is last and follows NSIS's unquoted-directory convention. The installer
runs silently, without wizard, progress, language-selection or error dialogs.
Windows may still request UAC approval. In helper mode NSIS skips the launcher
EXE when registering files with Restart Manager, protects the launcher from
shutdown, and skips checking, overwriting or removing the launcher EXE. Resource
copying and user-file handling otherwise follow the existing installer.

The helper waits for the installer exit code and records the result. The original
launcher stays open, waits asynchronously for the helper, and displays completion
or failure in its existing console. Neither the helper nor NSIS restarts it.
On success the launcher reloads profiles and metadata from the installed YAML,
preserves preferences, and confirms the application version. Automatic startup
updates then continue with the saved Python auto-start preference.

MirrorChyan updates deliberately preserve the existing launcher binary. Deliver
launcher code or embedded UI changes through a normal complete setup instead.

## Storage and recovery

The CDK is DPAPI-encrypted for the current Windows account, outside the directory
overwritten by setup:

`%LOCALAPPDATA%\PyAppify\updates\<installation-path-hash>\cdk.bin`

It is not included in YAML, App payloads, transaction files or logs. Settings only
reads whether a key is stored and offers save/clear operations. Moving the
installation or changing Windows accounts requires saving the CDK again.

The same private directory contains `pending.json`, pointing to a transaction in
`%TEMP%\pyappify-update-<random>`. That transaction stores paths, version,
preferences and installation result, but no CDK or download URL. Temporary
installers remain available for retry/manual repair; they may be removed after
the update has finished.

- Download/preparation failures leave the current application usable.
- UAC cancellation or failure to launch setup leaves the launcher open with an error
  without advancing the version.
- An installer failure or interrupted installation blocks application startup
  until repaired; no automatic rollback is claimed. Check for updates and retry,
  or rerun the downloaded complete setup.
- Missing/corrupt transaction results are treated as an unconfirmed installation,
  not a successful update.
- MirrorChyan startup does not fetch Git, replay pip recovery markers or delete
  the application directory when Python is missing. Missing Python is reported as
  requiring the complete setup.
- Returning to Git first verifies the existing checkout against the recorded
  installed version. Failure leaves MirrorChyan selected and does not overwrite
  application files. This does not reconstruct missing repositories or fix
  arbitrary manual edits to installed files.

## Validation

### Local setup testing (Windows debug builds)

Local testing uses a script and the launcher's existing **Install** or **Upgrade** button.
Debug builds can replace the MirrorChyan release lookup and download with a
local setup EXE through environment variables; no CDK or API request is needed.
Application stopping, helper, silent NSIS installation and receipt handling
follow the ordinary update flow.

Run this from the repository root:

```powershell
.\scripts\start-local-setup-test.ps1
```

To test the installation page instead, run:

```powershell
.\scripts\start-local-setup-test.ps1 -Install
```

This starts with `installed: false`, no current version and no working resources.
Click the existing **Install** button to run `setup_app` with the local setup.
MirrorChyan installation and updating use the same inline log and result area
in the application card, without opening a separate console page.
The default mode starts installed at
`v0.0.1` for testing upgrades. Both modes use separate test directories.

The script first runs a Tauri debug NSIS build, embedding the frontend in the
launcher. A `tauri dev` build can overwrite the same debug EXE with a launcher
that connects to `devUrl`, so the script rebuilds before each test and requires
no running development server. It then uses that launcher and full NSIS template to
prepare a separate `src-tauri/target/local-setup-test-<id>/dev_cwd` directory and
test setup. It gives the setup its own Windows application registration. In the
opened launcher, use the existing **Upgrade** button to update to `v0.0.2`.
The expected changes are a confirmed `v0.0.2` version, a new resource marker at
`data/apps/local-setup-sample/working/setup-test-version.txt`, and a refreshed
profile pointing to `new_main.py`. The launcher PID, launcher EXE checksum and
`settings.local.txt` must remain unchanged. This small fixture exercises updating;
it does not include a runnable Python environment.

Use `-SetupPath 'D:\path\your-new-setup.exe' -Version v1.2.3` to select another
complete setup instead. That setup must retain the integration and application
layout expected by the launcher. `-Automatic` runs an automatic startup update,
verifies the result, writes `test-report.json`, and closes only its test launcher.
It cannot be combined with `-Install`, which requires the manual installation UI.
Without that switch the launcher remains open for manual testing.

The script sets `PYAPPIFY_LOCAL_INSTALLER` and
`PYAPPIFY_LOCAL_INSTALLER_VERSION` only for the child debug launcher, so its
initial version lookup is also local. These overrides are ignored in release
builds.

### Build checks and full upgrade acceptance

Run `cargo test --manifest-path src-tauri/Cargo.toml --lib`, `pnpm build`, and
`pnpm exec tauri build --debug --bundles nsis --ci` (or the normal release build).
Tests use loopback HTTP fixtures, in-memory DPAPI encryption, and isolated
temporary files; they do not require a real CDK or install over a real app.

Before enabling a real resource, test two complete versions in a Windows VM:

1. Install the old version into a path with spaces. Save a CDK, source, profile
   and auto-start preferences. Place representative user settings in the same
   locations used by the application.
2. Trigger a manual update while the application is running. Confirm download
   happens first, the application stops, setup uses the original directory, no
   installer GUI appears, and the launcher window and process remain open.
3. Check the updated application version, unchanged launcher binary, preferences,
   expected user files and Python startup. Verify there was no Git or pip operation
   in the update.
4. Test automatic update while an application is running and an application
   started externally during download. Installation must wait for it to stop.
5. Test an expired CDK, interrupted download, denied UAC, cancelled setup, file
   locking, installation failure and reopening the launcher during installation.
   Errors must not be reported as success or trigger repeated automatic installs.
6. Check Git remains the default for an old YAML/config and that switching back
   resumes the existing repository repair, fetch, checkout, and pip behavior.

A successful NSIS build verifies the template, not this two-version upgrade
scenario. Real acceptance requires the actual resource ID, CDK and full setups.
