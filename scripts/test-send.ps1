<#
.SYNOPSIS
    Émet une mire vidéo vers orbit-player, pour développer sans le PC fixe.

.DESCRIPTION
    Reproduit ce que fait l'hôte : un flux MPEG-TS H.264 (ou HEVC) basse latence, en UDP ou
    en SRT caller. Le player, lui, écoute toujours.

    L'encodeur est choisi automatiquement : `h264_nvenc` s'il y a une carte NVIDIA, sinon
    `libopenh264` (logiciel, LGPL). Aucun encodeur GPL n'est utilisé (règle 4 de CLAUDE.md),
    ce qui exclut x264 et x265.

.PARAMETER Transport
    udp (défaut) ou srt. En SRT, ce script est l'appelant (`mode=caller`) et le player
    l'auditeur, comme en production.

.PARAMETER Encoder
    auto (défaut), ou un encodeur précis : h264_nvenc, hevc_nvenc, libopenh264, h264_amf,
    h264_qsv, h264_mf...

.PARAMETER Size
    Résolution de la mire. 1920x1080 par défaut ; en encodage logiciel, 1280x720 tient
    mieux le temps réel sur un portable.

.PARAMETER Duration
    Durée en secondes ; 0 (défaut) pour émettre jusqu'à Ctrl+C.

.EXAMPLE
    # Dans un terminal : le player écoute
    cargo run -- --host 127.0.0.1
    # Dans un autre : la mire
    powershell -ExecutionPolicy Bypass -File scripts\test-send.ps1

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\test-send.ps1 -Transport srt -Size 1280x720
#>

[CmdletBinding()]
param(
    [ValidateSet('udp', 'srt')]
    [string] $Transport = 'udp',

    [string] $Target = '127.0.0.1',

    [int] $Port = 5000,

    [string] $Encoder = 'auto',

    [ValidatePattern('^\d+x\d+$')]
    [string] $Size = '1920x1080',

    [int] $Rate = 60,

    [string] $Bitrate = '20M',

    [int] $SrtLatencyMs = 40,

    [int] $Duration = 0,

    # Mire lavfi : testsrc2 par défaut ; « smptebars » ou une horloge sont parfois plus
    # parlants. Pour mesurer la latence à l'œil, voir -Chrono.
    [string] $Pattern = 'testsrc2',

    # Superpose un chronomètre en millisecondes : filmer les deux écrans côte à côte donne
    # la latence bout en bout (critère de la v0.1).
    [switch] $Chrono
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RacineProjet = Split-Path -Parent $PSScriptRoot

# ---------------------------------------------------------------------------
# Quel ffmpeg ?
# ---------------------------------------------------------------------------

$ffmpegLocal = Join-Path $RacineProjet 'third_party\ffmpeg\bin\ffmpeg.exe'
if (Test-Path $ffmpegLocal) {
    $Ffmpeg = $ffmpegLocal
}
else {
    $commande = Get-Command ffmpeg -ErrorAction SilentlyContinue
    if (-not $commande) {
        Write-Host 'ffmpeg introuvable.' -ForegroundColor Red
        Write-Host 'Lancer scripts\setup.ps1 (il installe FFmpeg dans third_party\ffmpeg).'
        exit 1
    }
    $Ffmpeg = $commande.Source
}
Write-Host "ffmpeg : $Ffmpeg" -ForegroundColor DarkGray

# ---------------------------------------------------------------------------
# Choix de l'encodeur
# ---------------------------------------------------------------------------

function Test-Encodeur([string] $Nom) {
    # Encodage réel de quelques images vers null : c'est le seul test fiable
    # (`-encoders` liste des encodeurs dont le matériel peut être absent).
    #
    # Deux pièges de Windows PowerShell 5.1, tous deux rencontrés ici :
    # - sans -nostdin, ffmpeg attend une touche et le script gèle ;
    # - `Start-Process -PassThru` avec redirection laisse ExitCode à $null, donc tout
    #   encodeur paraîtrait en échec. On appelle donc l'exe directement et on lit
    #   $LASTEXITCODE, en neutralisant le temps de l'appel l'arrêt sur erreur : ce que
    #   ffmpeg écrit sur stderr n'est pas une erreur PowerShell.
    $journal = Join-Path $env:TEMP 'orbit-test-encodeur.log'
    $arguments = @(
        '-hide_banner', '-nostdin', '-loglevel', 'error'
        '-f', 'lavfi', '-i', 'testsrc2=size=160x128:rate=10'
        '-c:v', $Nom, '-frames:v', '3', '-f', 'null', '-'
    )

    $precedent = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Ffmpeg @arguments *> $journal
        return ($LASTEXITCODE -eq 0)
    }
    finally {
        $ErrorActionPreference = $precedent
    }
}

# La liste des encodeurs sort sur stdout : pas de 2>&1, qui perturbe PowerShell 5.1.
$encodeursListes = (& $Ffmpeg -hide_banner -encoders | Out-String)

if ($Encoder -eq 'auto') {
    # Ordre de préférence : matériel NVIDIA (comme en production), puis logiciel LGPL.
    $candidats = @('h264_nvenc', 'h264_qsv', 'h264_amf', 'h264_mf', 'libopenh264')
    $Encoder = $null
    foreach ($candidat in $candidats) {
        if ($encodeursListes -notmatch [regex]::Escape($candidat)) { continue }
        Write-Host "  essai de $candidat..." -ForegroundColor DarkGray
        if (Test-Encodeur $candidat) {
            $Encoder = $candidat
            break
        }
    }
    if (-not $Encoder) {
        Write-Host 'Aucun encodeur H.264 utilisable dans ce build FFmpeg.' -ForegroundColor Red
        Write-Host 'Rappel : x264 est GPL et donc exclu du projet (règle 4 de CLAUDE.md).'
        exit 1
    }
}
Write-Host "encodeur : $Encoder" -ForegroundColor Green

# ---------------------------------------------------------------------------
# Options propres à l'encodeur
# ---------------------------------------------------------------------------

# Dans tous les cas : aucune image B (-bf 0) pour ne pas réordonner, et une image clé
# toutes les 2 secondes.
$optionsEncodeur = @('-b:v', $Bitrate, '-bf', '0', '-g', [string] ($Rate * 2))

switch -Wildcard ($Encoder) {
    '*_nvenc' {
        # p1 = le plus rapide, ull = ultra low latency (ce que fait l'hôte).
        $optionsEncodeur += @('-preset', 'p1', '-tune', 'ull', '-zerolatency', '1', '-rc', 'cbr')
    }
    'libopenh264' {
        # Encodeur logiciel : une tranche par thread, pas de recherche de débit.
        $optionsEncodeur += @('-rc_mode', 'bitrate', '-slices', '0')
    }
    '*_qsv' { $optionsEncodeur += @('-preset', 'veryfast', '-low_delay_brc', '1') }
    '*_amf' { $optionsEncodeur += @('-usage', 'ultralowlatency', '-quality', 'speed') }
    '*_mf' { $optionsEncodeur += @('-rate_control', 'cbr', '-scenario', 'display_remoting') }
    default { }
}

# ---------------------------------------------------------------------------
# Source et destination
# ---------------------------------------------------------------------------

$entree = "${Pattern}=size=${Size}:rate=${Rate}"
$filtres = @()
if ($Chrono) {
    # drawtext avec une horloge en millisecondes, en haut à gauche, très lisible.
    $filtres += "drawtext=text='%{eif\:trunc(t*1000)\:d}':fontcolor=white:fontsize=96:box=1:boxcolor=black:x=40:y=40"
}

$destination = if ($Transport -eq 'udp') {
    "udp://${Target}:${Port}?pkt_size=1316"
}
else {
    "srt://${Target}:${Port}?mode=caller&latency=$($SrtLatencyMs * 1000)&transtype=live"
}

$arguments = @(
    '-hide_banner'
    # -nostdin : ne pas capter le clavier, pour que Ctrl+C reste la seule façon d'arrêter.
    '-nostdin'
    # -re : émettre en temps réel, comme une vraie capture d'écran.
    '-re'
    '-f', 'lavfi', '-i', $entree
)
if ($filtres.Count -gt 0) { $arguments += @('-vf', ($filtres -join ',')) }
$arguments += @('-c:v', $Encoder)
$arguments += $optionsEncodeur
if ($Duration -gt 0) { $arguments += @('-t', [string] $Duration) }
$arguments += @('-f', 'mpegts', $destination)

Write-Host ''
Write-Host "mire        : $entree" -ForegroundColor White
Write-Host "destination : $destination" -ForegroundColor White
if ($Duration -gt 0) { Write-Host "durée       : $Duration s" }
else { Write-Host 'durée       : illimitée (Ctrl+C pour arrêter)' }
Write-Host ''
Write-Host 'Côté player, attendu dans le journal :' -ForegroundColor DarkGray
Write-Host "  « Entrée ouverte : $(if ($Transport -eq 'udp') { 'udp' } else { 'srt' })://0.0.0.0:$Port ... (H.264 $Size) »" -ForegroundColor DarkGray
Write-Host "  puis une ligne « réception : ... » par seconde, à environ $Rate paquets/s." -ForegroundColor DarkGray
Write-Host ''

if ($Transport -eq 'srt') {
    Write-Host 'SRT : le player doit déjà écouter, sinon la connexion est refusée.' -ForegroundColor Yellow
    Write-Host ''
}

& $Ffmpeg @arguments
exit $LASTEXITCODE
