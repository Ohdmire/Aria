param([int[]]$Pids = @(0))
$samples = Get-Counter -Counter '\GPU Process Memory(*)\Local Usage','\GPU Process Memory(*)\Non Local Usage' -SampleInterval 1 -MaxSamples 1
$byProc = @{}
foreach ($s in $samples.CounterSamples) {
    if ($s.InstanceName -match '^pid_(\d+)_(.*)$') {
        $pid2 = [int]$Matches[1]; $phys = $Matches[2]
        $key = $pid2
        if (-not $byProc.ContainsKey($key)) { $byProc[$key] = [ordered]@{ Local = [uint64]0; Shared = [uint64]0 } }
        if ($s.Path -like '*local usage*' -and $s.Path -notlike '*non local*') { $byProc[$key].Local += [uint64]$s.CookedValue }
        elseif ($s.Path -like '*non local usage*') { $byProc[$key].Shared += [uint64]$s.CookedValue }
    }
}
foreach ($k in $byProc.Keys) {
    $p = Get-Process -Id $k -ErrorAction SilentlyContinue
    if ($p -and ($p.ProcessName -match 'aria' -or ($Pids -contains $k))) {
        '{0,-8} pid={1,-7} local={2,8:N1} MB  shared(non-local)={3,8:N1} MB' -f $p.ProcessName, $k, ($byProc[$k].Local/1MB), ($byProc[$key].Shared/1MB)
    }
}
