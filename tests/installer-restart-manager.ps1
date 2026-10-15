param(
    [string]$MakeNsis
)

# Run with PowerShell 7 outside the filesystem sandbox: RM creates a Windows
# registry session. Every shutdown target is an owned process in this fixture.
$ErrorActionPreference = 'Stop'
if (!$MakeNsis) { $MakeNsis = Join-Path $env:LOCALAPPDATA 'tauri/NSIS/makensis.exe' }
$testRepo = Split-Path $PSScriptRoot -Parent
$testRoot = Join-Path $testRepo ('src-tauri/target/installer-rm-integration-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $testRoot | Out-Null
$testTemplate = [IO.File]::ReadAllText((Join-Path $testRepo 'src-tauri/nsis/installer.nsi'))
$testState = [regex]::Match($testTemplate, '(?s); BEGIN INSTALLER RESTART MANAGER STATE.*?; END INSTALLER RESTART MANAGER STATE').Value
$testFunctions = [regex]::Match($testTemplate, '(?s); BEGIN INSTALLER RESTART MANAGER FUNCTIONS.*?; END INSTALLER RESTART MANAGER FUNCTIONS').Value
if (!$testState -or !$testFunctions) { throw 'Could not extract installer RM implementation' }
if (!(Test-Path -LiteralPath $MakeNsis)) { throw "NSIS compiler not found: $MakeNsis" }

$testHarness = @'
Unicode true
!include LogicLib.nsh
!include Win\RestartManager.nsh
Name "Installer RM integration"
OutFile "@OUTPUT@"
RequestExecutionLevel user
SilentInstall silent
!define BUNDLEID "pyappify-rm-test"
@STATE@
@FUNCTIONS@
Function WriteReceipt
  FileOpen $9 "$EXEDIR\receipt.txt" w
  FileWriteUTF16LE /BOM $9 "error=$RestartManagerError$\r$\nstage=$RestartManagerStage$\r$\nfiles=$RestartManagerFileCount$\r$\n"
  FileWriteUTF16LE $9 "$RestartManagerMessage$\r$\n"
  FileClose $9
FunctionEnd
Function .onInstFailed
  Call WriteReceipt
FunctionEnd
Function .onInstSuccess
  Call WriteReceipt
FunctionEnd
Section
  Call KillInstallDirExecutables
SectionEnd
'@

function Build-Harness([string]$Name, [string]$Functions) {
    $testExe = Join-Path $testRoot "$Name.exe"
    $testNsi = Join-Path $testRoot "$Name.nsi"
    $testScript = $testHarness.Replace('@OUTPUT@', $testExe).Replace('@STATE@', $testState).Replace('@FUNCTIONS@', $Functions)
    [IO.File]::WriteAllText($testNsi, $testScript, [Text.UTF8Encoding]::new($false))
    & $MakeNsis /V2 /INPUTCHARSET UTF8 $testNsi | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "NSIS compilation failed: $Name" }
    return $testExe
}

function Invoke-Harness([string]$Exe, [string]$Install, [int]$ExpectedError, [int]$ExpectedFiles, [string]$ExpectedStage = '') {
    $testReceiptPath = Join-Path $testRoot 'receipt.txt'
    if (Test-Path -LiteralPath $testReceiptPath) { Remove-Item -LiteralPath $testReceiptPath }
    $testInfo = [Diagnostics.ProcessStartInfo]::new()
    $testInfo.FileName = $Exe
    $testInfo.UseShellExecute = $false
    $testInfo.CreateNoWindow = $true
    # NSIS /D must be the last argument and must not be quoted.
    $testInfo.Arguments = "/S /D=$Install"
    $testProcess = [Diagnostics.Process]::Start($testInfo)
    try {
        if (!$testProcess.WaitForExit(55000)) {
            $testProcess.Kill($true)
            throw "Installer test timed out: $Install"
        }
        $testReceipt = [IO.File]::ReadAllText((Join-Path $testRoot 'receipt.txt'))
        $testValues = @{}
        foreach ($testLine in ($testReceipt -split '\r?\n')) {
            if ($testLine -match '^([^=]+)=(.*)$') { $testValues[$Matches[1]] = $Matches[2] }
        }
        if ($testValues.error -ne [string]$ExpectedError -or $testProcess.ExitCode -ne $ExpectedError) {
            throw "Unexpected failure for ${Install}: exit=$($testProcess.ExitCode), $testReceipt"
        }
        if ([int]$testValues.files -ne $ExpectedFiles) { throw "Unexpected file count: $testReceipt" }
        if ($ExpectedStage -and $testValues.stage -ne $ExpectedStage) { throw "Unexpected error stage: $testReceipt" }
        Copy-Item -LiteralPath (Join-Path $testRoot 'receipt.txt') -Destination (Join-Path $Install 'receipt.txt')
        if ($ExpectedError) {
            if (!$testReceipt.Contains("Step: $ExpectedStage") -or !$testReceipt.Contains("Error: $ExpectedError — ")) {
                throw "Missing precise error in popup: $testReceipt"
            }
            if (!$testReceipt.Contains("Path: $Install") -or !$testReceipt.Contains('Occupiers: ') -or !$testReceipt.Contains('Reboot reasons: ')) {
                throw "Missing popup context: $testReceipt"
            }
            if ($testReceipt.Contains('Log: ') -or $testReceipt.Contains('Windows could not provide an error description.')) {
                throw "Popup did not contain the Windows error description: $testReceipt"
            }
        }
        Write-Host "PASS $(Split-Path $Install -Leaf): exit=$ExpectedError, registered=$ExpectedFiles, stage=$($testValues.stage)"
    } finally {
        $testProcess.Dispose()
    }
}

$testExe = Build-Harness 'setup' $testFunctions
foreach ($testName in @('empty', 'empty with spaces', '空目录', 'data-only', 'native-files')) {
    $testInstall = Join-Path $testRoot $testName
    New-Item -ItemType Directory -Path $testInstall | Out-Null
    $testCount = 0
    if ($testName -eq 'data-only') {
        [IO.File]::WriteAllText((Join-Path $testInstall 'main.py'), 'fixture')
        [IO.File]::WriteAllText((Join-Path $testInstall 'settings.json'), '{}')
    }
    if ($testName -eq 'native-files') {
        $testNested = Join-Path $testInstall 'nested/深层'
        New-Item -ItemType Directory -Path $testNested | Out-Null
        # Cross multiple 256-file batches and ignore non-native extensions.
        foreach ($testIndex in 1..600) {
            $testExtension = @('ExE', 'DLL', 'pYd')[$testIndex % 3]
            [IO.File]::WriteAllText((Join-Path $testNested "$testIndex.$testExtension"), 'unoccupied fixture')
            [IO.File]::WriteAllText((Join-Path $testNested "$testIndex.exe.config"), 'ignored')
        }
        $testCount = 600
    }
    Invoke-Harness $testExe $testInstall 0 $testCount
}

# Trigger a real Windows API error without changing production code: only the
# test build receives an invalid session handle in RmRegisterResources.
$testFaultFunctions = $testFunctions.Replace('::RmRegisterResources(i $RestartManagerSession,', '::RmRegisterResources(i -1,')
if ($testFaultFunctions -eq $testFunctions) { throw 'Could not instrument registration failure' }
$testFaultExe = Build-Harness 'setup-invalid-session' $testFaultFunctions
$testFaultInstall = Join-Path $testRoot 'registration-error'
New-Item -ItemType Directory -Path $testFaultInstall | Out-Null
[IO.File]::WriteAllText((Join-Path $testFaultInstall 'fixture.exe'), 'fixture')
Invoke-Harness $testFaultExe $testFaultInstall 6 0 'RmRegisterResources'

$testLinkInstall = Join-Path $testRoot 'junction-error'
New-Item -ItemType Directory -Path $testLinkInstall | Out-Null
New-Item -ItemType Junction -Path (Join-Path $testLinkInstall 'link') -Target (Join-Path $testRoot 'native-files') | Out-Null
Invoke-Harness $testExe $testLinkInstall 50 0 'Validate installation path (link/junction)'

# The real installer protects itself even when it is inside the installation
# directory and would otherwise be among the registered executable resources.
$testSelfInstall = Join-Path $testRoot 'installer-in-install-dir'
New-Item -ItemType Directory -Path $testSelfInstall | Out-Null
$testSelfExe = Join-Path $testSelfInstall 'setup.exe'
Copy-Item -LiteralPath $testExe -Destination $testSelfExe
# Keep a separate native resource so this case exercises an actual RM session.
[IO.File]::WriteAllText((Join-Path $testSelfInstall 'unused.dll'), 'fixture')
# This harness writes beside its EXE; preserve the same receipt location.
$testSelfProcess = Start-Process -FilePath $testSelfExe -ArgumentList "/S /D=$testSelfInstall" -WindowStyle Hidden -PassThru
if (!$testSelfProcess.WaitForExit(20000)) { $testSelfProcess.Kill($true); throw 'Self-protection test timed out' }
if ($testSelfProcess.ExitCode -ne 0) { throw 'Setup did not survive its own RM check' }
$testSelfReceipt = [IO.File]::ReadAllText((Join-Path $testSelfInstall 'receipt.txt'))
if (!$testSelfReceipt.Contains('files=1')) { throw "Setup executable was not excluded: $testSelfReceipt" }
$testSelfProcess.Dispose()
Write-Host 'PASS setup inside install directory: setup survives, its own EXE excluded'

# A registered DLL loaded by setup must never cause setup to terminate itself.
$testSelfHoldInstall = Join-Path $testRoot 'setup-holds-native-module'
New-Item -ItemType Directory -Path $testSelfHoldInstall | Out-Null
$testNsisModule = if (Test-Path "$env:SystemRoot/SysWOW64/version.dll") { "$env:SystemRoot/SysWOW64/version.dll" } else { "$env:SystemRoot/System32/version.dll" }
Copy-Item -LiteralPath $testNsisModule -Destination (Join-Path $testSelfHoldInstall 'self.dll')
$testSelfHoldPrologue = @'
  System::Call 'kernel32::LoadLibraryW(w "$INSTDIR\self.dll") p .r0'
  ${If} $0 = 0
    SetErrorLevel 10
    Abort
  ${EndIf}
  retry_kill_install_dir_executables:
'@
$testSelfHoldFunctions = $testFunctions.Replace('  retry_kill_install_dir_executables:', $testSelfHoldPrologue)
$testSelfHoldExe = Build-Harness 'setup-self-hold' $testSelfHoldFunctions
Invoke-Harness $testSelfHoldExe $testSelfHoldInstall 0 1
Write-Host 'PASS setup holding a registered DLL: protected setup survives'

# A small owned Windows process loads both DLL and PYD resources. No existing
# application is registered or terminated by this test.
$testLockedInstall = Join-Path $testRoot 'owned-native-occupier'
New-Item -ItemType Directory -Path $testLockedInstall | Out-Null
$testLockerSource = Join-Path $testRoot 'locker.cs'
[IO.File]::WriteAllText($testLockerSource, @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;
class Locker {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr LoadLibraryW(string path);
    static void Main(string[] args) {
        for (int i = 1; i < args.Length; ++i)
            if (LoadLibraryW(args[i]) == IntPtr.Zero) Environment.Exit(10);
        File.WriteAllText(args[0], "ready");
        Thread.Sleep(120000);
    }
}
'@)
$testLockerExe = Join-Path $testLockedInstall 'locker.ExE'
$testCompiler = Join-Path $env:SystemRoot 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
& $testCompiler /nologo /target:exe "/out:$testLockerExe" $testLockerSource
if ($LASTEXITCODE -ne 0) { throw 'Could not compile owned file occupier' }
$testMarker = Join-Path $testRoot 'locker-ready'
$testModules = @((Join-Path $testLockedInstall 'native.DLL'), (Join-Path $testLockedInstall 'native.PyD'))
foreach ($testModule in $testModules) { Copy-Item -LiteralPath "$env:SystemRoot/System32/version.dll" -Destination $testModule }
$testLockerInfo = [Diagnostics.ProcessStartInfo]::new()
$testLockerInfo.FileName = $testLockerExe
$testLockerInfo.UseShellExecute = $false
$testLockerInfo.CreateNoWindow = $true
foreach ($testArgument in (@($testMarker) + $testModules)) { $testLockerInfo.ArgumentList.Add($testArgument) }
$testLocker = [Diagnostics.Process]::Start($testLockerInfo)
try {
    $testDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while (!(Test-Path -LiteralPath $testMarker)) {
        if ($testLocker.HasExited -or [DateTime]::UtcNow -gt $testDeadline) { throw 'Owned occupier did not become ready' }
        Start-Sleep -Milliseconds 100
    }
    Invoke-Harness $testExe $testLockedInstall 0 3
    if (!$testLocker.WaitForExit(3000)) { throw 'RM force shutdown did not stop the owned occupier' }
    Write-Host 'PASS force shutdown of owned process with loaded EXE, DLL and PYD'
} finally {
    if (!$testLocker.HasExited) { $testLocker.Kill($true) }
    $testLocker.Dispose()
}
Write-Host "Results: $testRoot"
