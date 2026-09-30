param(
    [string]$SetupPath,
    [string]$Version = 'v0.0.2',
    [switch]$Install,
    [switch]$Automatic
)

$ErrorActionPreference = 'Stop'
$taskRepo = Split-Path $PSScriptRoot -Parent
$taskTarget = Join-Path $taskRepo 'src-tauri/target'
$taskBinary = Join-Path $taskTarget 'debug/pyappify.exe'
$taskGenerated = Join-Path $taskTarget 'debug/nsis/x64'
if ($Version -notmatch '^v?\d+\.\d+\.\d+(?:[-.](?:alpha|beta|rc)(?:\.\d+)?)?$') { throw 'Use a target version such as v0.0.2.' }
if ($Install -and $Automatic) { throw 'Install mode is manual: omit -Automatic and use the launcher Install button to test the installation UI.' }

# tauri dev writes to the same EXE path. Build through Tauri before copying it
# so the test launcher embeds the frontend instead of connecting to devUrl.
$taskCli = Join-Path $taskRepo 'node_modules/@tauri-apps/cli/tauri.js'
if (!(Test-Path -LiteralPath $taskCli)) { throw 'Install the project dependencies first (pnpm install).' }
Write-Output 'Building the standalone debug launcher and setup for local testing...'
Push-Location -LiteralPath $taskRepo
try {
    & node $taskCli build --debug --bundles nsis --ci
    if ($LASTEXITCODE -ne 0) { throw 'The standalone debug build failed; no test launcher was started.' }
} finally {
    Pop-Location
}
if (!(Test-Path -LiteralPath $taskBinary)) { throw 'The standalone debug build did not produce the launcher EXE.' }

$taskId = [Guid]::NewGuid().ToString('N')
$taskRoot = Join-Path $taskTarget "local-setup-test-$taskId"
$taskInstall = Join-Path $taskRoot 'dev_cwd'
$taskWorking = Join-Path $taskInstall 'data/apps/local-setup-sample/working'
$taskPayload = Join-Path $taskRoot 'payload'
New-Item -ItemType Directory -Path $taskInstall, $taskPayload | Out-Null
$taskLauncher = Join-Path $taskInstall 'pyappify.exe'
Copy-Item -LiteralPath $taskBinary -Destination $taskLauncher
$taskPolicy = if ($Automatic) { 'AUTO_UPDATE' } else { 'MANUAL_UPDATE' }
$taskInstalled = if ($Install) { 'false' } else { 'true' }
$taskCurrentVersion = if ($Install) { 'null' } else { 'v0.0.1' }
$taskYaml = @"
name: local-setup-sample
update_source: mirrorchyan
update_method: $taskPolicy
auto_start: false
installed: $taskInstalled
current_version: $taskCurrentVersion
profiles:
  - name: release
    main_script: old_main.py
    requires_python: '3.12'
"@
[IO.File]::WriteAllText((Join-Path $taskInstall 'pyappify.yml'), $taskYaml)
if (!$Install) {
    New-Item -ItemType Directory -Path $taskWorking | Out-Null
    [IO.File]::WriteAllText((Join-Path $taskWorking 'pyappify.yml'), $taskYaml)
    [IO.File]::WriteAllText((Join-Path $taskWorking 'setup-test-version.txt'), 'v0.0.1')
    [IO.File]::WriteAllText((Join-Path $taskWorking 'settings.local.txt'), 'keep this user setting')
}

$taskIsFixture = [string]::IsNullOrWhiteSpace($SetupPath)
if ($taskIsFixture) {
    $taskSource = Join-Path $taskGenerated 'installer.nsi'
    if (!(Test-Path -LiteralPath $taskSource)) { throw 'Build the NSIS debug bundle first.' }
    $taskScript = [IO.File]::ReadAllText($taskSource)
    if (!$taskScript.Contains('/INSTALLERHELPER')) { throw 'The generated NSIS template is outdated. Rebuild the debug bundle.' }
    # Give the complete generated setup its own Windows registration. Its install
    # sections and Restart Manager behavior remain those of the actual template.
    $taskScript = [regex]::Replace($taskScript, '(?m)^!define PRODUCTNAME .*$', "!define PRODUCTNAME `"pyappify-local-test-$taskId`"")
    $taskScript = [regex]::Replace($taskScript, '(?m)^!define BUNDLEID .*$', "!define BUNDLEID `"com.pyappify.localtest.$taskId`"")
    $taskScript = [regex]::Replace($taskScript, '(?m)^!define MANUFACTURER .*$', '!define MANUFACTURER "PyAppifyLocalTests"')
    $taskSetup = Join-Path $taskRoot 'local-test-setup.exe'
    $taskScript = [regex]::Replace($taskScript, '(?m)^!define OUTFILE .*$', "!define OUTFILE `"$taskSetup`"")
    [IO.File]::WriteAllText((Join-Path $taskPayload 'setup-test-version.txt'), $Version)
    $taskPayloadYaml = $taskYaml.Replace('old_main.py', 'new_main.py').Replace("installed: $taskInstalled", 'installed: true').Replace("current_version: $taskCurrentVersion", "current_version: $Version")
    [IO.File]::WriteAllText((Join-Path $taskPayload 'pyappify.yml'), $taskPayloadYaml)
    $taskResources = @"
  CreateDirectory "`$INSTDIR\data\apps\local-setup-sample\working"
  File /a "/oname=data\apps\local-setup-sample\working\setup-test-version.txt" "$taskPayload\setup-test-version.txt"
  File /a "/oname=data\apps\local-setup-sample\working\pyappify.yml" "$taskPayload\pyappify.yml"
"@
    $taskScript = $taskScript.Replace('  ; Copy external binaries', $taskResources + "`r`n  ; Copy external binaries")
    foreach ($taskInclude in @('utils.nsh', 'FileAssociation.nsh')) {
        $taskScript = $taskScript.Replace("!include `"$taskInclude`"", "!include `"$taskGenerated\$taskInclude`"")
    }
    $taskNsi = Join-Path $taskRoot 'local-test.nsi'
    [IO.File]::WriteAllText($taskNsi, $taskScript, [Text.UTF8Encoding]::new($false))
    & "$env:LOCALAPPDATA\tauri\NSIS\makensis.exe" /V2 /INPUTCHARSET UTF8 $taskNsi
    if ($LASTEXITCODE -ne 0) { throw 'Local full setup compilation failed.' }
} else {
    $taskSetup = (Resolve-Path -LiteralPath $SetupPath).Path
}

$taskHash = (Get-FileHash -LiteralPath $taskLauncher).Hash
$taskStartInfo = [Diagnostics.ProcessStartInfo]::new()
$taskStartInfo.FileName = $taskLauncher
$taskStartInfo.WorkingDirectory = $taskRoot
$taskStartInfo.UseShellExecute = $false
$taskStartInfo.CreateNoWindow = $true
$taskStartInfo.Environment['PYAPPIFY_LOCAL_INSTALLER'] = $taskSetup
$taskStartInfo.Environment['PYAPPIFY_LOCAL_INSTALLER_VERSION'] = $Version
$taskProcess = [Diagnostics.Process]::Start($taskStartInfo)
$taskReport = [ordered]@{
    test_directory = $taskRoot
    install_directory = $taskInstall
    setup = $taskSetup
    version = $Version
    launcher_pid = $taskProcess.Id
    launcher_sha256_before = $taskHash
    fixture = $taskIsFixture
    mode = if ($Install) { 'install' } else { 'upgrade' }
}
$taskReportPath = Join-Path $taskRoot 'test-report.json'
[IO.File]::WriteAllText($taskReportPath, ($taskReport | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
Write-Output "Test directory: $taskRoot"
Write-Output "Local setup: $taskSetup"
Write-Output "Launcher PID: $($taskProcess.Id)"
if (!$Automatic) {
    if ($Install) {
        Write-Output "In the launcher click Install to test setup $Version in the application card."
        Write-Output 'The application starts uninstalled, with no current version or existing working resources.'
    } else {
        Write-Output "In the launcher use the existing Upgrade button to update to $Version."
    }
    Write-Output 'The test runs in its own directory. No MirrorChyan API call or download is made.'
    return
}

$taskAppJson = Join-Path $taskInstall 'data/apps/local-setup-sample/app.json'
$taskDeadline = [DateTime]::UtcNow.AddSeconds(120)
try {
    while ([DateTime]::UtcNow -lt $taskDeadline) {
        $taskProcess.Refresh()
        if ($taskProcess.HasExited) { throw 'The original launcher exited during the local update.' }
        if (Test-Path -LiteralPath $taskAppJson) {
            $taskApp = [IO.File]::ReadAllText($taskAppJson) | ConvertFrom-Json
            if ($taskApp.update_state -eq 'failed' -or $taskApp.update_error) { throw "Local update failed: $($taskApp.update_error)" }
            if ($taskApp.current_version -eq $Version -and $taskApp.update_state -eq 'idle') { break }
        }
        Start-Sleep -Milliseconds 250
    }
    if (!$taskApp -or $taskApp.current_version -ne $Version -or $taskApp.update_state -ne 'idle') { throw 'Timed out waiting for the local update result.' }
    $taskHashAfter = (Get-FileHash -LiteralPath $taskLauncher).Hash
    if ($taskHashAfter -ne $taskHash) { throw 'The launcher EXE was replaced.' }
    if ($taskIsFixture) {
        if ([IO.File]::ReadAllText((Join-Path $taskWorking 'setup-test-version.txt')) -ne $Version) { throw 'Setup resource was not updated.' }
        if ($taskApp.profiles[0].main_script -ne 'new_main.py') { throw 'The launcher did not reload the newly installed profile.' }
    }
    if ([IO.File]::ReadAllText((Join-Path $taskWorking 'settings.local.txt')) -ne 'keep this user setting') { throw 'A user file was overwritten.' }
    $taskProcess.Refresh()
    if ($taskProcess.HasExited) { throw 'The launcher restarted instead of remaining open.' }
    $taskReport['launcher_sha256_after'] = $taskHashAfter
    $taskReport['result'] = 'passed'
    $taskReport['current_version'] = $taskApp.current_version
    $taskReport['profile_main_script'] = $taskApp.profiles[0].main_script
    [IO.File]::WriteAllText($taskReportPath, ($taskReport | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
    Write-Output 'PASSED: same launcher process, unchanged launcher EXE, confirmed update version, preserved user settings.'
    Write-Output "Report: $taskReportPath"
} catch {
    $taskReport['result'] = 'failed'
    $taskReport['error'] = $_.Exception.Message
    [IO.File]::WriteAllText($taskReportPath, ($taskReport | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
    throw
} finally {
    # Automated verification owns this test process only. Manual runs remain open.
    if (!$taskProcess.HasExited) { $taskProcess.Kill() }
    $taskProcess.Dispose()
}
