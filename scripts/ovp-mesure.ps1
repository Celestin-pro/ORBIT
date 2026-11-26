<#
.SYNOPSIS
    Série de mesures du protocole OVP, à lancer sur le PC FIXE (émetteur).

.DESCRIPTION
    Lance orbit-sender six fois de suite (FEC 0 / 20 / 40 %, pacing activé puis désactivé),
    chaque essai pendant -Duree secondes, et garde un journal par essai dans -Dossier.

    Côté PC portable, lancer AVANT ce script un seul recv_stats pour toute la série :
        .\target\release\examples\recv_stats.exe --host <IP du fixe> --trace-losses --duration-s 480 |
            Tee-Object recv-serie.log
    Entre deux essais, orbit-sender redémarre : recv_stats se resynchronise tout seul.

    Prérequis sur le fixe : ffmpeg dans le PATH (ou -Ffmpeg), session ORBIT arrêtée (sinon le
    port 5000 du portable est déjà pris), règle de pare-feu UDP 5003 entrante.

.EXAMPLE
    .\ovp-mesure.ps1 -Client 192.168.1.72
#>
param(
    [Parameter(Mandatory = $true)] [string]$Client,
    [string]$Exe,
    [string]$Ffmpeg = 'ffmpeg',
    [int]$Duree = 60,
    [int]$DebitMbps = 20,
    [string]$Dossier
)

$ErrorActionPreference = 'Stop'
# Sous PowerShell 5.1, $PSScriptRoot est vide dans les valeurs par défaut de param() :
# les chemins par défaut sont donc calculés ici.
$ici = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
if (-not $Exe) { $Exe = Join-Path $ici 'orbit-sender.exe' }
if (-not $Dossier) { $Dossier = Join-Path $ici ("mesures-" + (Get-Date -Format 'yyyyMMdd-HHmm')) }
if (-not (Test-Path $Exe)) { throw "orbit-sender introuvable : $Exe (option -Exe)" }
New-Item -ItemType Directory -Force $Dossier | Out-Null

$essais = @(
    @{ Nom = 'fec0-pacing';    Args = @('--fec-percent', '0') },
    @{ Nom = 'fec20-pacing';   Args = @('--fec-percent', '20') },
    @{ Nom = 'fec40-pacing';   Args = @('--fec-percent', '40') },
    @{ Nom = 'fec0-rafale';    Args = @('--fec-percent', '0', '--no-pacing') },
    @{ Nom = 'fec20-rafale';   Args = @('--fec-percent', '20', '--no-pacing') },
    @{ Nom = 'fec40-rafale';   Args = @('--fec-percent', '40', '--no-pacing') }
)

foreach ($e in $essais) {
    $journal = Join-Path $Dossier "$($e.Nom).log"
    Write-Host "=== $($e.Nom) ($Duree s) -> $journal" -ForegroundColor Cyan
    $arguments = @('--client', $Client, '--source', 'desktop', '--bitrate-mbps', $DebitMbps,
                   '--ffmpeg', $Ffmpeg, '--duration-s', $Duree) + $e.Args
    & $Exe @arguments | Tee-Object -FilePath $journal
    if ($LASTEXITCODE -ne 0) { Write-Warning "orbit-sender a renvoyé $LASTEXITCODE pour $($e.Nom)" }
    Start-Sleep -Seconds 5
}
Write-Host "Terminé. Journaux dans $Dossier" -ForegroundColor Green
