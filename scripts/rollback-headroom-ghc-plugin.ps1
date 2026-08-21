[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
if (Test-Path Variable:\PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$UserHome = 'C:\Users\shafan'
$ServicesDir = Join-Path $UserHome '.headroom\services'
$RollbackRoot = Join-Path $ServicesDir 'rollback-headroom-ghc'
$LegacyHostExe = Join-Path $RollbackRoot 'ProxyServiceHost-publish\HeadroomProxyServiceHost.exe'
$Log = Join-Path $UserHome '.headroom\logs\rollback-headroom-ghc-plugin.log'

New-Item -ItemType Directory -Path (Split-Path $Log) -Force | Out-Null

function Write-RollbackLog {
    param([string]$Message)
    "[$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')] $Message" |
        Tee-Object -FilePath $Log -Append
}

function Remove-ServiceIfPresent {
    param([string]$Name)
    $service = Get-Service -Name $Name -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        return
    }
    if ($service.Status -ne 'Stopped') {
        Stop-Service -Name $Name -Force -ErrorAction SilentlyContinue
        $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(20))
    }
    sc.exe delete $Name | Out-Null
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline -and $null -ne (Get-Service -Name $Name -ErrorAction SilentlyContinue)) {
        Start-Sleep -Milliseconds 250
    }
    if ($null -ne (Get-Service -Name $Name -ErrorAction SilentlyContinue)) {
        throw "Timed out deleting Windows service $Name"
    }
}

function Wait-HttpHealthy {
    param(
        [string]$Url,
        [int]$TimeoutSeconds = 60
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        try {
            $response = Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 3
            if ($response.StatusCode -ge 200 -and $response.StatusCode -lt 300) {
                return $true
            }
        } catch {
        }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this rollback from an elevated PowerShell terminal.'
}
if (-not (Test-Path $LegacyHostExe)) {
    throw "Legacy service-host backup is missing: $LegacyHostExe"
}

Write-RollbackLog 'Restoring dual-service Headroom + ghc-proxy architecture'
Remove-ServiceIfPresent 'headroom-default'
Remove-ServiceIfPresent 'ghc-proxy'

$ghcBinary = "`"$LegacyHostExe`" --service ghc-proxy"
$headroomBinary = "`"$LegacyHostExe`" --service headroom"
New-Service -Name ghc-proxy -BinaryPathName $ghcBinary -DisplayName 'GitHub Copilot API Proxy' -StartupType Automatic | Out-Null
New-Service -Name headroom-default -BinaryPathName $headroomBinary -DisplayName 'Headroom Default Proxy' -StartupType Automatic -DependsOn ghc-proxy | Out-Null

sc.exe description ghc-proxy "Runs ghc-proxy on 127.0.0.1:8314 using the pre-migration service host." | Tee-Object -FilePath $Log -Append
sc.exe failure ghc-proxy reset= 60 actions= restart/5000/restart/5000/restart/30000 | Tee-Object -FilePath $Log -Append
sc.exe failureflag ghc-proxy 1 | Tee-Object -FilePath $Log -Append
sc.exe description headroom-default "Runs Headroom on 127.0.0.1:8787 and routes upstream requests through ghc-proxy on 127.0.0.1:8314." | Tee-Object -FilePath $Log -Append
sc.exe failure headroom-default reset= 60 actions= restart/5000/restart/5000/restart/30000 | Tee-Object -FilePath $Log -Append
sc.exe failureflag headroom-default 1 | Tee-Object -FilePath $Log -Append

Start-Service -Name ghc-proxy
if (-not (Wait-HttpHealthy 'http://127.0.0.1:8314/health?strict=true')) {
    throw 'Rollback ghc-proxy did not become healthy on port 8314.'
}
Start-Service -Name headroom-default
if (-not (Wait-HttpHealthy 'http://127.0.0.1:8787/readyz')) {
    throw 'Rollback Headroom did not become ready on port 8787.'
}

Write-RollbackLog 'Rollback completed successfully'
Get-Service -Name ghc-proxy,headroom-default |
    Format-Table Name,Status,StartType |
    Out-String |
    Tee-Object -FilePath $Log -Append
