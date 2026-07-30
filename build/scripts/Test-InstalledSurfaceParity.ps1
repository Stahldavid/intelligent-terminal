[CmdletBinding()]
param(
    [string]$InstallRoot = "$env:LOCALAPPDATA\Programs\IntelligentTerminal",

    [ValidateRange(5, 180)]
    [int]$TimeoutSeconds = 45,

    [switch]$IncludeManagedAgent,

    [string]$ComputeTarget = 'local',

    [string]$Agent = 'codex',

    [string]$SurfaceProfile = 'Command Prompt',

    [string]$SurfaceCommand = 'cmd.exe /d /k',

    [ValidateRange(0, 120)]
    [int]$DebugDelaySeconds = 0,

    [switch]$InnerProbe,

    [string]$OutputPath
)

$ErrorActionPreference = 'Stop'

$terminalPath = Join-Path $InstallRoot 'WindowsTerminal.exe'
$wtcliPath = Join-Path $InstallRoot 'wtcli.exe'
$wtaPath = Join-Path $InstallRoot 'wta.exe'

foreach ($requiredPath in $terminalPath, $wtcliPath) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required installed component not found: $requiredPath"
    }
}
if ($IncludeManagedAgent -and -not (Test-Path -LiteralPath $wtaPath -PathType Leaf)) {
    throw "Managed-agent verification requires the installed WTA component: $wtaPath"
}

function Invoke-WtcliJson {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)

    $raw = @(& $wtcliPath --json @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "wtcli $($Arguments -join ' ') failed: $($raw -join [Environment]::NewLine)"
    }
    return (($raw -join [Environment]::NewLine) | ConvertFrom-Json)
}

function Get-ProtocolPane {
    param(
        [Parameter(Mandatory = $true)]$Envelope,
        [Parameter(Mandatory = $true)][uint32]$PaneId,
        [switch]$Active
    )

    $matches = @($Envelope.panes | Where-Object { [uint32]$_.pane_id -eq $PaneId })
    if ($Active) {
        $matches = @($matches | Where-Object { $_.is_active })
    }
    return $matches | Select-Object -First 1
}

function Test-SameGuid {
    param([string]$Left, [string]$Right)

    return [guid]::Parse($Left) -eq [guid]::Parse($Right)
}

if ($InnerProbe) {
    if ([string]::IsNullOrWhiteSpace($OutputPath)) {
        throw '-OutputPath is required with -InnerProbe.'
    }
    if ([string]::IsNullOrWhiteSpace($env:WT_COM_CLSID)) {
        throw 'The probe must run inside an Intelligent Terminal surface.'
    }

    $createdSurfaceId = $null
    $createdDuplicateSurfaceId = $null
    $createdManagedSurfaceId = $null
    $baseSurfaceId = $null
    $evidence = [ordered]@{
        success = $false
        protocol_version = $null
        base_surface_id = $null
        created_surface_id = $null
        duplicate_surface_id = $null
        managed_surface_id = $null
        pane_id = $null
        surface_count_before = $null
        surface_count_after_create = $null
        surface_count_after_duplicate = $null
        surface_count_after_managed = $null
        surface_count_after_cleanup = $null
        profile_after_create = $null
        managed_binding_id = $null
        managed_binding_state = $null
        managed_binding_removed = $null
        managed_leases_released = $null
        error = $null
    }

    try {
        if ($DebugDelaySeconds -gt 0) {
            Start-Sleep -Seconds $DebugDelaySeconds
        }
        $info = Invoke-WtcliJson info
        if (-not $info.connected -or $info.protocol_version -ne '3.1') {
            throw "Expected connected protocol 3.1, received '$($info.protocol_version)'."
        }
        $evidence.protocol_version = $info.protocol_version

        $active = Invoke-WtcliJson active-pane
        $baseSurfaceId = [string]$active.surface_session_id
        if ([string]::IsNullOrWhiteSpace($baseSurfaceId) -or
            [guid]::Parse($baseSurfaceId) -eq [guid]::Empty) {
            throw 'The probe surface has no stable surface_session_id.'
        }

        $paneId = [uint32]$active.pane_id
        $windowId = [uint64]$active.window_id
        $tabId = [uint32]$active.tab_id
        $evidence.base_surface_id = $baseSurfaceId
        $evidence.pane_id = $paneId

        $beforeEnvelope = Invoke-WtcliJson list-panes --window-id $windowId --tab-id $tabId
        $beforePane = Get-ProtocolPane -Envelope $beforeEnvelope -PaneId $paneId
        if (-not $beforePane) {
            throw "Could not find pane $paneId before surface creation."
        }
        $evidence.surface_count_before = [uint32]$beforePane.surface_count

        $surfaceArgs = @('new-surface', '--target', $baseSurfaceId)
        if (-not [string]::IsNullOrWhiteSpace($SurfaceProfile)) {
            $surfaceArgs += @('--profile', $SurfaceProfile)
        }
        if (-not [string]::IsNullOrWhiteSpace($SurfaceCommand)) {
            $surfaceArgs += @('--command', $SurfaceCommand)
        }
        $created = Invoke-WtcliJson @surfaceArgs
        $createdSurfaceId = [string]$created.session_id
        if ([string]::IsNullOrWhiteSpace($createdSurfaceId) -or
            [guid]::Parse($createdSurfaceId) -eq [guid]::Empty -or
            (Test-SameGuid $createdSurfaceId $baseSurfaceId)) {
            throw 'new-surface did not return a distinct stable surface ID.'
        }
        $evidence.created_surface_id = $createdSurfaceId

        $afterCreateEnvelope = Invoke-WtcliJson list-panes --window-id $windowId --tab-id $tabId
        $afterCreatePane = Get-ProtocolPane -Envelope $afterCreateEnvelope -PaneId $paneId -Active
        if (-not $afterCreatePane) {
            throw "Could not find pane $paneId after surface creation."
        }
        $evidence.surface_count_after_create = [uint32]$afterCreatePane.surface_count
        $evidence.profile_after_create = [string]$afterCreatePane.profile
        if ($evidence.surface_count_after_create -ne ($evidence.surface_count_before + 1)) {
            throw 'The pane-local surface count did not increase by exactly one.'
        }
        if (-not (Test-SameGuid ([string]$afterCreatePane.surface_session_id) $createdSurfaceId)) {
            throw 'The newly created surface did not become the active surface.'
        }

        Invoke-WtcliJson focus-pane --target $baseSurfaceId | Out-Null
        $focusedBase = Invoke-WtcliJson active-pane
        if (-not (Test-SameGuid ([string]$focusedBase.surface_session_id) $baseSurfaceId)) {
            throw 'Focus did not return to the original surface.'
        }

        Invoke-WtcliJson focus-pane --target $createdSurfaceId | Out-Null
        $focusedCreated = Invoke-WtcliJson active-pane
        if (-not (Test-SameGuid ([string]$focusedCreated.surface_session_id) $createdSurfaceId)) {
            throw 'Focus did not switch to the created surface.'
        }

        # Exercise the pane-local primary + path separately from profile menu
        # creation. It must duplicate settings into a new PTY and must never
        # reattach/move the active ContentId.
        $duplicated = Invoke-WtcliJson new-surface --target $baseSurfaceId
        $createdDuplicateSurfaceId = [string]$duplicated.session_id
        if ([string]::IsNullOrWhiteSpace($createdDuplicateSurfaceId) -or
            [guid]::Parse($createdDuplicateSurfaceId) -eq [guid]::Empty -or
            (Test-SameGuid $createdDuplicateSurfaceId $baseSurfaceId) -or
            (Test-SameGuid $createdDuplicateSurfaceId $createdSurfaceId)) {
            throw 'The primary duplicate-surface path reused an existing terminal session.'
        }
        $evidence.duplicate_surface_id = $createdDuplicateSurfaceId

        $afterDuplicateEnvelope = Invoke-WtcliJson list-panes --window-id $windowId --tab-id $tabId
        $afterDuplicatePane = Get-ProtocolPane -Envelope $afterDuplicateEnvelope -PaneId $paneId -Active
        if (-not $afterDuplicatePane) {
            throw "Could not find pane $paneId after duplicate surface creation."
        }
        $evidence.surface_count_after_duplicate = [uint32]$afterDuplicatePane.surface_count
        if ($evidence.surface_count_after_duplicate -ne ($evidence.surface_count_before + 2)) {
            throw 'The primary duplicate path did not add exactly one pane-local surface.'
        }
        if (-not (Test-SameGuid ([string]$afterDuplicatePane.surface_session_id) $createdDuplicateSurfaceId)) {
            throw 'The primary duplicate path did not activate its new terminal session.'
        }

        if ($IncludeManagedAgent) {
            $managed = Invoke-WtcliJson new-agent-surface `
                --target $baseSurfaceId `
                --compute-target $ComputeTarget `
                --agent $Agent `
                --background
            $createdManagedSurfaceId = [string]$managed.session_id
            if ([string]::IsNullOrWhiteSpace($createdManagedSurfaceId) -or
                [guid]::Parse($createdManagedSurfaceId) -eq [guid]::Empty) {
                throw 'new-agent-surface did not return a stable surface ID.'
            }
            $evidence.managed_surface_id = $createdManagedSurfaceId
            if ((Test-SameGuid $createdManagedSurfaceId $baseSurfaceId) -or
                (Test-SameGuid $createdManagedSurfaceId $createdSurfaceId) -or
                (Test-SameGuid $createdManagedSurfaceId $createdDuplicateSurfaceId)) {
                throw 'The Managed Agent Surface reused an existing terminal session.'
            }

            $afterManagedEnvelope = Invoke-WtcliJson list-panes --window-id $windowId --tab-id $tabId
            $afterManagedPane = Get-ProtocolPane -Envelope $afterManagedEnvelope -PaneId $paneId
            if (-not $afterManagedPane) {
                throw "Could not find pane $paneId after Managed Agent Surface creation."
            }
            $evidence.surface_count_after_managed = [uint32]$afterManagedPane.surface_count
            if ($evidence.surface_count_after_managed -ne ($evidence.surface_count_before + 3)) {
                throw 'Managed Agent Surface creation did not add exactly one pane-local surface.'
            }

            $bindingsRaw = @(& $wtaPath compute binding list --json 2>&1)
            if ($LASTEXITCODE -ne 0) {
                throw "Could not inspect the Compute Store: $($bindingsRaw -join [Environment]::NewLine)"
            }
            $binding = @(($bindingsRaw -join [Environment]::NewLine | ConvertFrom-Json) |
                Where-Object {
                    $_.kind -eq 'managed_agent' -and
                    (Test-SameGuid ([string]$_.surface_id) $createdManagedSurfaceId)
                }) | Select-Object -First 1
            if (-not $binding) {
                throw 'The Managed Agent Surface was not projected into the canonical Compute Store.'
            }
            $evidence.managed_binding_id = [string]$binding.binding_id
            $evidence.managed_binding_state = [string]$binding.state
        }

        if ($createdManagedSurfaceId) {
            $managedBindingId = [string]$evidence.managed_binding_id
            Invoke-WtcliJson kill-pane --target $createdManagedSurfaceId | Out-Null
            $createdManagedSurfaceId = $null

            # Lifecycle cleanup is asynchronous through surface_closed. Wait
            # for the canonical store rather than accepting an optimistic UI
            # close that leaves target capacity leased until TTL expiry.
            $cleanupDeadline = (Get-Date).AddSeconds(15)
            do {
                $bindingsRaw = @(& $wtaPath compute binding list --json 2>&1)
                if ($LASTEXITCODE -ne 0) {
                    throw "Could not verify managed binding cleanup: $($bindingsRaw -join [Environment]::NewLine)"
                }
                $remainingBinding = @(($bindingsRaw -join [Environment]::NewLine | ConvertFrom-Json) |
                    Where-Object { $_.binding_id -eq $managedBindingId }) | Select-Object -First 1

                $leasesRaw = @(& $wtaPath compute lease list --json 2>&1)
                if ($LASTEXITCODE -ne 0) {
                    throw "Could not verify managed lease cleanup: $($leasesRaw -join [Environment]::NewLine)"
                }
                $activeManagedLeases = @(($leasesRaw -join [Environment]::NewLine | ConvertFrom-Json) |
                    Where-Object {
                        $_.subject_id -eq $managedBindingId -and
                        $_.state -eq 'active'
                    })
                if (-not $remainingBinding -and $activeManagedLeases.Count -eq 0) {
                    break
                }
                Start-Sleep -Milliseconds 250
            } while ((Get-Date) -lt $cleanupDeadline)

            $evidence.managed_binding_removed = -not [bool]$remainingBinding
            $evidence.managed_leases_released = $activeManagedLeases.Count -eq 0
            if (-not $evidence.managed_binding_removed -or -not $evidence.managed_leases_released) {
                throw 'Closing the Managed Agent Surface left a binding or active compute lease behind.'
            }
        }
        if ($createdDuplicateSurfaceId) {
            Invoke-WtcliJson kill-pane --target $createdDuplicateSurfaceId | Out-Null
            $createdDuplicateSurfaceId = $null
        }
        Invoke-WtcliJson kill-pane --target $createdSurfaceId | Out-Null
        $createdSurfaceId = $null

        $afterCleanupEnvelope = Invoke-WtcliJson list-panes --window-id $windowId --tab-id $tabId
        $afterCleanupPane = Get-ProtocolPane -Envelope $afterCleanupEnvelope -PaneId $paneId
        if (-not $afterCleanupPane) {
            throw "Could not find pane $paneId after cleanup."
        }
        $evidence.surface_count_after_cleanup = [uint32]$afterCleanupPane.surface_count
        if ($evidence.surface_count_after_cleanup -ne $evidence.surface_count_before) {
            throw 'Surface cleanup did not restore the original pane-local count.'
        }

        $evidence.success = $true
    }
    catch {
        $evidence.error = $_.Exception.Message
        # Preserve the post-failure topology as diagnostic evidence. A surface
        # factory can fail after mutating the pane (for example while focus or
        # process metadata is still initializing); recording the canonical
        # pane state distinguishes that partial-commit bug from a rejected
        # request without weakening cleanup or the pass criteria.
        try {
            if ($windowId -and $tabId -ne $null -and $paneId -ne $null) {
                $failureEnvelope = Invoke-WtcliJson list-panes --window-id $windowId --tab-id $tabId
                $failurePane = Get-ProtocolPane -Envelope $failureEnvelope -PaneId $paneId
                if ($failurePane) {
                    $evidence.surface_count_after_failure = [uint32]$failurePane.surface_count
                    $evidence.surface_id_after_failure = [string]$failurePane.surface_session_id
                    $evidence.profile_after_failure = [string]$failurePane.profile
                }
            }
        }
        catch {
            $evidence.failure_topology_error = $_.Exception.Message
        }
    }
    finally {
        foreach ($surface in @($createdManagedSurfaceId, $createdDuplicateSurfaceId, $createdSurfaceId)) {
            if (-not [string]::IsNullOrWhiteSpace($surface)) {
                try {
                    Invoke-WtcliJson kill-pane --target $surface | Out-Null
                }
                catch {
                    # Preserve the primary failure in the result envelope.
                }
            }
        }

        Set-Content -LiteralPath $OutputPath -Value ($evidence | ConvertTo-Json -Depth 5) -Encoding utf8

        # The result is durable before this fire-and-forget cleanup closes the
        # probe's own surface. Start-Process inherits the scoped protocol claim.
        if (-not [string]::IsNullOrWhiteSpace($baseSurfaceId)) {
            Start-Process -FilePath $wtcliPath `
                -ArgumentList @('--json', 'kill-pane', '--target', $baseSurfaceId) `
                -WindowStyle Hidden | Out-Null
        }
    }

    if ($evidence.success) {
        exit 0
    }
    exit 1
}

$ownsOutputPath = [string]::IsNullOrWhiteSpace($OutputPath)
if ($ownsOutputPath) {
    $OutputPath = Join-Path ([System.IO.Path]::GetTempPath()) (
        'intelligent-terminal-surface-parity-{0}.json' -f [guid]::NewGuid().ToString('N')
    )
}

try {
    Remove-Item -LiteralPath $OutputPath -Force -ErrorAction SilentlyContinue

    $arguments = @(
        'new-tab',
        '--title',
        'Surface parity probe',
        'pwsh.exe',
        '-NoProfile',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        $PSCommandPath,
        '-InstallRoot',
        $InstallRoot,
        '-TimeoutSeconds',
        $TimeoutSeconds,
        '-ComputeTarget',
        $ComputeTarget,
        '-Agent',
        $Agent,
        '-SurfaceProfile',
        $SurfaceProfile,
        '-SurfaceCommand',
        $SurfaceCommand,
        '-DebugDelaySeconds',
        $DebugDelaySeconds,
        '-InnerProbe',
        '-OutputPath',
        $OutputPath
    )
    if ($IncludeManagedAgent) {
        $arguments += '-IncludeManagedAgent'
    }

    & $terminalPath @arguments
    if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {
        throw "Failed to create the in-terminal surface parity probe (exit $LASTEXITCODE)."
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while (-not (Test-Path -LiteralPath $OutputPath -PathType Leaf) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
    }
    if (-not (Test-Path -LiteralPath $OutputPath -PathType Leaf)) {
        throw "Timed out after $TimeoutSeconds seconds waiting for surface parity evidence."
    }

    $result = Get-Content -LiteralPath $OutputPath -Raw | ConvertFrom-Json
    if (-not $result.success) {
        throw "Installed surface parity probe failed: $($result.error)"
    }
    $result
}
finally {
    if ($ownsOutputPath) {
        Remove-Item -LiteralPath $OutputPath -Force -ErrorAction SilentlyContinue
    }
}
