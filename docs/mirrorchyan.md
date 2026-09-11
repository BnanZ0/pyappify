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
layout, and include a launcher with this integration. Publishing/uploading that
installer is external to this feature; the existing packaging workflow and
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
handle ensure NSIS starts only after the original launcher exits. Closing the
launcher before committing does not start the installer.

The helper runs setup with `/P /UPDATE /UPDATERPID=<pid> /D=<original directory>`.
`/D=` is last and follows NSIS's unquoted-directory convention. The passive
installer displays progress and skips normal wizard and language-selection
pages. UAC and exceptional installation-error dialogs may still appear. Resource
copying and user-file handling are exactly those of the existing installer.

The helper waits for the installer exit code, records the result, and starts the
launcher once. It does not pass `/R`, so NSIS does not start a second copy. The
new launcher restores the saved source, update policy, auto-start preference and
profile, confirms the application version, and skips automatic updating for that
launch. Python application startup still follows the saved auto-start preference.

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
the update has finished and the launcher has restarted.

- Download/preparation failures leave the current application usable.
- UAC cancellation or failure to launch setup reopens the launcher with an error
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
   wizard choices are required, and the launcher restarts exactly once.
3. Check the new launcher/application versions, preferences, expected user files
   and Python startup. Verify there was no Git or pip operation in the update.
4. Test automatic update while an application is running and an application
   started externally during download. Installation must wait for it to stop.
5. Test an expired CDK, interrupted download, denied UAC, cancelled setup, file
   locking, installation failure and reopening the launcher during installation.
   Errors must not be reported as success or trigger repeated automatic installs.
6. Check Git remains the default for an old YAML/config and that switching back
   resumes the existing repository repair, fetch, checkout, and pip behavior.

A successful NSIS build verifies the template, not this two-version upgrade
scenario. Real acceptance requires the actual resource ID, CDK and full setups.
