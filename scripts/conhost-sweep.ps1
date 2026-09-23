# Sweeps orphaned headless conhost.exe processes (incident 2026-09-20).
#
# A ConPTY master that is never released via ClosePseudoConsole leaves a
# `conhost.exe --headless` behind after its creator exits; each orphan spins
# at up to ~40% of a core indefinitely (17 of them saturated a 16-thread
# machine). The only safe kill criterion is the pair: the command line
# contains --headless AND the parent process is gone. Anything else (IDE pty
# hosts, terminal windows, services) has a live parent and is left alone.
#
# Runs as the scheduled task "WaveCode conhost sweep" (every 15 min); it logs
# only when it actually killed something:
#   %USERPROFILE%\.conhost-watch\sweep.log

$ErrorActionPreference = 'SilentlyContinue'

$orphans = Get-CimInstance Win32_Process -Filter 'Name="conhost.exe"' |
    Where-Object { $_.CommandLine -match '--headless' } |
    Where-Object { -not (Get-CimInstance Win32_Process -Filter ("ProcessId=" + $_.ParentProcessId)) }

$killed = @()
foreach ($o in $orphans) {
    if (Stop-Process -Id $o.ProcessId -Force -PassThru) {
        $killed += $o.ProcessId
    }
}

if ($killed.Count -gt 0) {
    $log = Join-Path $HOME '.conhost-watch\sweep.log'
    New-Item -ItemType Directory -Force -Path (Split-Path $log) | Out-Null
    Add-Content -Path $log -Value (
        "{0} swept {1} orphaned headless conhost: {2}" -f `
            (Get-Date -Format s), $killed.Count, ($killed -join ',')
    )
}
