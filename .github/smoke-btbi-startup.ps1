$ErrorActionPreference = "Stop"
# Exercise the installed app, not just the compiler. No sign-in or recording.
$installer = @(Get-ChildItem "target/release/bundle/nsis/*setup.exe")
if ($installer.Count -ne 1) { throw "Expected one NSIS pilot installer" }
$installPath = Join-Path $env:RUNNER_TEMP "BTBIStartupSmoke"
$setup = Start-Process $installer[0].FullName -ArgumentList @("/S", "/D=$installPath") -Wait -PassThru
if ($setup.ExitCode -ne 0) { throw "Installer failed: $($setup.ExitCode)" }
$exe = Join-Path $installPath "meetily.exe"
if (!(Test-Path $exe)) { throw "Installed executable missing" }
$stderr = Join-Path $env:RUNNER_TEMP "btbi-startup-stderr.txt"
$app = Start-Process $exe -WorkingDirectory $installPath -RedirectStandardError $stderr -PassThru
try {
    Start-Sleep -Seconds 20
    $app.Refresh()
    if ($app.HasExited) {
        Get-Content $stderr -ErrorAction SilentlyContinue
        throw "Installed app exited during startup: $($app.ExitCode)"
    }
    if ($app.MainWindowHandle -eq 0) { throw "App remained running but no main window was observed" }
    Write-Host "Installed app remained running with a main window for 20 seconds."
} finally {
    if (!$app.HasExited) { Stop-Process -Id $app.Id -Force }
}
