[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
if (Test-Path Variable:\PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$UserHome = 'C:\Users\shafan'
$Repo = Join-Path $UserHome 'ghc-proxy'
$ServicesDir = Join-Path $UserHome '.headroom\services'
$Python = Join-Path $UserHome 'headroom\Scripts\python.exe'
$HeadroomWheel = Join-Path $Repo 'target\headroom-upgrade-0.36.2\headroom_ai-0.36.2-cp310-abi3-win_amd64.whl'
$PluginWheel = Join-Path $Repo 'target\headroom-upgrade-0.36.2\headroom_ghc_plugin-0.1.1-py3-none-any.whl'
$Installer = Join-Path $ServicesDir 'install-proxy-services.ps1'
$RollbackScript = Join-Path $Repo 'scripts\rollback-headroom-ghc-plugin.ps1'
$CurrentPublish = Join-Path $ServicesDir 'ProxyServiceHost\bin\Release\net10.0\win-x64\publish'
$RollbackRoot = Join-Path $ServicesDir 'rollback-headroom-ghc'
$LegacyPublish = Join-Path $RollbackRoot 'ProxyServiceHost-publish'
$LegacyHostExe = Join-Path $LegacyPublish 'HeadroomProxyServiceHost.exe'
$CopilotAuth = Join-Path $UserHome '.headroom\copilot_auth.json'
$Log = Join-Path $UserHome '.headroom\logs\migrate-ghc-to-headroom-plugin.log'

New-Item -ItemType Directory -Path (Split-Path $Log) -Force | Out-Null

function Write-MigrationLog {
    param([string]$Message)
    "[$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')] $Message" |
        Tee-Object -FilePath $Log -Append
}

function Assert-LastExitCode {
    param([string]$Action)
    if ($LASTEXITCODE -ne 0) {
        throw "$Action failed with exit code $LASTEXITCODE"
    }
}

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this migration from an elevated PowerShell terminal.'
}

foreach ($required in @($Python, $HeadroomWheel, $PluginWheel, $Installer, $RollbackScript, $CopilotAuth)) {
    if (-not (Test-Path $required)) {
        throw "Required migration input is missing: $required"
    }
}

Write-MigrationLog 'Starting ghc-proxy to in-process Headroom plugin migration'
$headroom = Get-Service -Name headroom-default -ErrorAction SilentlyContinue
$headroomWasRunning = $null -ne $headroom -and $headroom.Status -eq 'Running'
$legacyServiceExists = $null -ne (Get-Service -Name ghc-proxy -ErrorAction SilentlyContinue)

try {
    # Preserve the exact currently-running dual-service host before the installer
    # rebuilds its publish directory. The host embeds the old 8787 -> 8314 routing,
    # so this directory is a complete rollback artifact rather than source-code hope.
    if ($legacyServiceExists) {
        if (-not (Test-Path $CurrentPublish)) {
            throw "Current service-host publish directory is missing: $CurrentPublish"
        }
        Write-MigrationLog "Backing up dual-service host to $LegacyPublish"
        Remove-Item $RollbackRoot -Recurse -Force -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Path $LegacyPublish -Force | Out-Null
        Copy-Item (Join-Path $CurrentPublish '*') $LegacyPublish -Recurse -Force
        if (-not (Test-Path $LegacyHostExe)) {
            throw 'The legacy service-host backup is incomplete.'
        }
    } elseif (-not (Test-Path $LegacyHostExe)) {
        throw 'No ghc-proxy service or existing rollback service-host backup was found.'
    }

    if ($headroomWasRunning) {
        Write-MigrationLog 'Stopping headroom-default to release the native extension'
        Stop-Service -Name headroom-default -Force
        (Get-Service -Name headroom-default).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(20))
    }

    Write-MigrationLog 'Installing Headroom 0.36.2 from the verified local wheel'
    & $Python -m pip install --upgrade --no-deps $HeadroomWheel 2>&1 |
        Tee-Object -FilePath $Log -Append
    Assert-LastExitCode 'Headroom installation'

    Write-MigrationLog 'Installing ghc extension 0.1.1 from the local wheel'
    & $Python -m pip install --force-reinstall --no-deps $PluginWheel 2>&1 |
        Tee-Object -FilePath $Log -Append
    Assert-LastExitCode 'Plugin installation'

    Write-MigrationLog 'Checking Python dependency consistency'
    & $Python -m pip check 2>&1 | Tee-Object -FilePath $Log -Append
    Assert-LastExitCode 'pip check'

    & $Python -c "import headroom, importlib.metadata as m; assert headroom.__version__ == '0.36.2'; assert m.version('headroom-ghc-plugin') == '0.1.1'; assert any(e.name == 'ghc' for e in m.entry_points(group='headroom.proxy_extension'))"
    Assert-LastExitCode 'Package verification'

    Write-MigrationLog 'Re-provisioning the single Headroom Windows service'
    & $Installer
    Assert-LastExitCode 'Service installation'

    $health = Invoke-RestMethod -Uri 'http://127.0.0.1:8787/health' -TimeoutSec 20
    $plugin = Invoke-RestMethod -Uri 'http://127.0.0.1:8787/api/ghc/health' -TimeoutSec 20
    $usage = Invoke-RestMethod -Uri 'http://127.0.0.1:8787/api/usage' -TimeoutSec 20
    $models = Invoke-RestMethod -Uri 'http://127.0.0.1:8787/v1/models' -TimeoutSec 30
    if (-not $health.ready -or $health.version -ne '0.36.2' -or -not $plugin.ready) {
        throw 'Post-migration health validation failed.'
    }
    if ($plugin.version -ne '0.1.1' -or $plugin.openai_target -match '8314' -or $plugin.anthropic_target -match '8314') {
        throw 'The in-process plugin version or upstream target is incorrect.'
    }
    if (-not $usage.token_based_billing -or $models.data.Count -lt 1) {
        throw 'Copilot quota or model discovery validation failed.'
    }

    $listener = Get-NetTCPConnection -LocalPort 8314 -State Listen -ErrorAction SilentlyContinue
    if ($null -ne $listener) {
        throw 'Port 8314 is still listening after migration.'
    }
    if ($null -ne (Get-Service -Name ghc-proxy -ErrorAction SilentlyContinue)) {
        throw 'The ghc-proxy Windows service still exists after migration.'
    }

    Write-MigrationLog 'Migration completed: Headroom owns Copilot transport; port 8314 is closed'
    $health | ConvertTo-Json -Depth 5 | Tee-Object -FilePath $Log -Append
    $plugin | ConvertTo-Json -Depth 5 | Tee-Object -FilePath $Log -Append
    Write-MigrationLog "Rollback remains available at $RollbackScript"
} catch {
    Write-MigrationLog "ERROR: $($_.Exception.Message)"
    if (Test-Path $LegacyHostExe) {
        Write-MigrationLog 'Attempting automatic rollback to the saved dual-service host'
        try {
            & $RollbackScript 2>&1 | Tee-Object -FilePath $Log -Append
            Assert-LastExitCode 'Automatic rollback'
            Write-MigrationLog 'Automatic rollback succeeded'
        } catch {
            Write-MigrationLog "AUTOMATIC ROLLBACK FAILED: $($_.Exception.Message)"
        }
    } elseif ($headroomWasRunning) {
        $current = Get-Service -Name headroom-default -ErrorAction SilentlyContinue
        if ($null -ne $current -and $current.Status -ne 'Running') {
            Start-Service -Name headroom-default -ErrorAction SilentlyContinue
        }
    }
    throw
}
