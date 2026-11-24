<#
.SYNOPSIS
    Prépare l'environnement de compilation d'orbit-player (récepteur vidéo ORBIT).

.DESCRIPTION
    1. Vérifie les prérequis : Rust stable MSVC, Visual Studio Build Tools (C++), LLVM/Clang.
    2. Télécharge SDL3 précompilé (release officielle VC) dans third_party/sdl3.
    3. Télécharge FFmpeg LGPL *shared* (builds BtbN) dans third_party/ffmpeg.
    4. Écrit .cargo/config.toml avec FFMPEG_DIR et SDL3_DIR, pour que `cargo build`
       fonctionne sans toucher aux variables d'environnement de la machine.

    Le script est idempotent : relancé, il ne retélécharge pas ce qui est déjà en place
    (sauf -Force).

.PARAMETER FfmpegBranch
    Branche FFmpeg à récupérer chez BtbN : 8.1, 9.0 ou master.
    IMPORTANT : la branche 7.1 (demandée à l'origine dans CLAUDE.md) n'est plus publiée
    dans la release « latest » de BtbN/FFmpeg-Builds. Utiliser 8.1 (branche stable la plus
    ancienne encore disponible) et aligner ffmpeg-sys-next sur la même version majeure.

.PARAMETER SkipFfmpeg
    Ne pas télécharger FFmpeg (utile à l'étape 1, qui n'en a pas besoin).

.PARAMETER Force
    Retélécharger et réextraire même si les dossiers existent déjà.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\setup.ps1
#>

[CmdletBinding()]
param(
    [ValidateSet('8.1', '9.0', 'master')]
    [string] $FfmpegBranch = '8.1',

    [string] $SdlVersion = '3.4.20',

    [switch] $SkipFfmpeg,

    [switch] $Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# ---------------------------------------------------------------------------
# Utilitaires d'affichage
# ---------------------------------------------------------------------------

$script:Problemes = New-Object System.Collections.Generic.List[string]
$script:Avertissements = New-Object System.Collections.Generic.List[string]

function Write-Titre([string] $Texte) {
    Write-Host ''
    Write-Host "=== $Texte ===" -ForegroundColor Cyan
}

function Write-Ok([string] $Texte) {
    Write-Host "  [OK]   $Texte" -ForegroundColor Green
}

function Write-Avert([string] $Texte, [string] $Remede) {
    Write-Host "  [!]    $Texte" -ForegroundColor Yellow
    if ($Remede) { Write-Host "         -> $Remede" -ForegroundColor Yellow }
    $script:Avertissements.Add($Texte)
}

function Write-Echec([string] $Texte, [string] $Remede) {
    Write-Host "  [NON]  $Texte" -ForegroundColor Red
    if ($Remede) { Write-Host "         -> $Remede" -ForegroundColor Red }
    $script:Problemes.Add($Texte)
}

# ---------------------------------------------------------------------------
# Chemins du projet
# ---------------------------------------------------------------------------

$RacineProjet = Split-Path -Parent $PSScriptRoot
$DossierTiers = Join-Path $RacineProjet 'third_party'
$DossierSdl = Join-Path $DossierTiers 'sdl3'
$DossierFfmpeg = Join-Path $DossierTiers 'ffmpeg'
$DossierCache = Join-Path $DossierTiers '.cache'

Write-Host 'Préparation de l''environnement orbit-player' -ForegroundColor White
Write-Host "Projet : $RacineProjet"

# ---------------------------------------------------------------------------
# 1. Prérequis
# ---------------------------------------------------------------------------

Write-Titre '1. Prérequis'

# --- Rust / cargo, et surtout la cible MSVC ---
$rustc = Get-Command rustc -ErrorAction SilentlyContinue
if (-not $rustc) {
    Write-Echec 'rustc introuvable.' 'Installer Rust : https://rustup.rs (choisir l''hôte x86_64-pc-windows-msvc)'
}
else {
    $infoRust = & rustc -vV
    $ligneHote = $infoRust | Where-Object { $_ -like 'host:*' }
    $versionRust = ($infoRust | Select-Object -First 1)
    if ($ligneHote -and $ligneHote -match 'x86_64-pc-windows-msvc') {
        Write-Ok "$versionRust (hôte MSVC 64 bits)"
    }
    else {
        Write-Echec "Toolchain Rust non MSVC 64 bits : $ligneHote" `
            'rustup toolchain install stable-x86_64-pc-windows-msvc ; rustup default stable-x86_64-pc-windows-msvc'
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Echec 'cargo introuvable.' 'Réinstaller Rust via rustup.'
}
else {
    Write-Ok ((& cargo -V) -join '')
}

# --- Visual Studio Build Tools : on cherche le linker C++ via vswhere ---
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$msvcTrouve = $false
if (Test-Path $vswhere) {
    $instance = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath 2>$null
    if ($instance) {
        Write-Ok "Visual Studio Build Tools (C++) : $instance"
        $msvcTrouve = $true
    }
}
if (-not $msvcTrouve) {
    # Repli : link.exe déjà dans le PATH (invite « x64 Native Tools »).
    if (Get-Command link.exe -ErrorAction SilentlyContinue) {
        Write-Ok 'link.exe présent dans le PATH (invite développeur MSVC).'
    }
    else {
        Write-Echec 'Composant C++ de Visual Studio Build Tools introuvable.' `
            'winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"'
    }
}

# --- LLVM / Clang : requis par bindgen (ffmpeg-sys-next), donc à partir de l'étape 2 ---
$clang = Get-Command clang.exe -ErrorAction SilentlyContinue
if (-not $clang) {
    $cheminsLlvm = @(
        (Join-Path $env:ProgramFiles 'LLVM\bin\clang.exe'),
        (Join-Path ${env:ProgramFiles(x86)} 'LLVM\bin\clang.exe')
    )
    $clang = $cheminsLlvm | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if ($clang) {
    $cheminClang = if ($clang -is [string]) { $clang } else { $clang.Source }
    Write-Ok "LLVM/Clang : $cheminClang"
    # bindgen a besoin de libclang.dll ; on note son dossier pour .cargo/config.toml.
    $script:DossierLibclang = Split-Path -Parent $cheminClang
}
else {
    Write-Avert 'LLVM/Clang introuvable (requis par bindgen pour ffmpeg-sys-next, étape 2 et suivantes).' `
        'winget install LLVM.LLVM   puis relancer ce script'
    $script:DossierLibclang = $null
}

# --- ffmpeg.exe : seulement utile pour scripts/test-send.ps1 ---
if (Get-Command ffmpeg -ErrorAction SilentlyContinue) {
    Write-Ok 'ffmpeg.exe dans le PATH (utile pour scripts/test-send.ps1).'
}
else {
    Write-Avert 'ffmpeg.exe absent du PATH.' `
        'Pas nécessaire pour compiler ; test-send.ps1 utilisera celui de third_party/ffmpeg/bin.'
}

if ($script:Problemes.Count -gt 0) {
    Write-Host ''
    Write-Host 'Prérequis manquants : corrigez les points [NON] ci-dessus, puis relancez.' -ForegroundColor Red
    exit 1
}

# ---------------------------------------------------------------------------
# Téléchargement + extraction (fonction commune)
# ---------------------------------------------------------------------------

New-Item -ItemType Directory -Force -Path $DossierTiers, $DossierCache | Out-Null

function Get-Archive {
    param(
        [Parameter(Mandatory)] [string] $Url,
        [Parameter(Mandatory)] [string] $NomFichier
    )

    $archive = Join-Path $DossierCache $NomFichier
    if ((Test-Path $archive) -and -not $Force) {
        Write-Host "  Archive déjà en cache : $NomFichier"
        return $archive
    }

    Write-Host "  Téléchargement : $Url"
    $progressionPrecedente = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'   # beaucoup plus rapide
    try {
        Invoke-WebRequest -Uri $Url -OutFile $archive -UseBasicParsing
    }
    finally {
        $ProgressPreference = $progressionPrecedente
    }

    $taille = [math]::Round((Get-Item $archive).Length / 1MB, 1)
    Write-Host "  Reçu : $NomFichier ($taille Mo)"
    return $archive
}

function Expand-Vers {
    param(
        [Parameter(Mandatory)] [string] $Archive,
        [Parameter(Mandatory)] [string] $Destination
    )

    # On extrait dans un dossier temporaire : les archives SDL3 et FFmpeg contiennent
    # un unique dossier racine horodaté dont on veut se débarrasser.
    $temporaire = Join-Path $DossierCache ('extract-' + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $temporaire | Out-Null
    try {
        Expand-Archive -LiteralPath $Archive -DestinationPath $temporaire -Force

        # @() : sous Set-StrictMode, un résultat unique n'expose pas .Count sans cela.
        $entrees = @(Get-ChildItem -LiteralPath $temporaire)
        $source = if ($entrees.Count -eq 1 -and $entrees[0].PSIsContainer) {
            $entrees[0].FullName
        }
        else {
            $temporaire
        }

        if (Test-Path $Destination) { Remove-Item -LiteralPath $Destination -Recurse -Force }
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Destination) | Out-Null
        Move-Item -LiteralPath $source -Destination $Destination
    }
    finally {
        if (Test-Path $temporaire) { Remove-Item -LiteralPath $temporaire -Recurse -Force }
    }
}

# ---------------------------------------------------------------------------
# 2. SDL3 précompilé
# ---------------------------------------------------------------------------

Write-Titre "2. SDL3 $SdlVersion (release officielle VC)"

$sdlLib = Join-Path $DossierSdl 'lib\x64\SDL3.lib'
if ((Test-Path $sdlLib) -and -not $Force) {
    Write-Ok "Déjà présent : $DossierSdl"
}
else {
    $nomSdl = "SDL3-devel-$SdlVersion-VC.zip"
    $urlSdl = "https://github.com/libsdl-org/SDL/releases/download/release-$SdlVersion/$nomSdl"
    $archiveSdl = Get-Archive -Url $urlSdl -NomFichier $nomSdl
    Expand-Vers -Archive $archiveSdl -Destination $DossierSdl

    if (-not (Test-Path $sdlLib)) {
        Write-Echec "SDL3.lib introuvable après extraction (attendu : $sdlLib)" `
            'Vérifier le contenu de third_party/sdl3 et la version demandée.'
        exit 1
    }
    Write-Ok "SDL3 installé : $DossierSdl"
}

$sdlDll = Join-Path $DossierSdl 'lib\x64\SDL3.dll'
if (Test-Path $sdlDll) { Write-Ok 'SDL3.dll présent (sera copiée à côté de l''exe par build.rs).' }
else { Write-Avert 'SDL3.dll introuvable dans lib\x64.' 'L''exe ne démarrera pas sans elle.' }

# ---------------------------------------------------------------------------
# 3. FFmpeg LGPL shared
# ---------------------------------------------------------------------------

if ($SkipFfmpeg) {
    Write-Titre '3. FFmpeg LGPL shared — ignoré (-SkipFfmpeg)'
}
else {
    Write-Titre "3. FFmpeg n$FfmpegBranch LGPL shared (BtbN)"

    $entete = Join-Path $DossierFfmpeg 'include\libavcodec\avcodec.h'
    if ((Test-Path $entete) -and -not $Force) {
        Write-Ok "Déjà présent : $DossierFfmpeg"
    }
    else {
        $nomFfmpeg = if ($FfmpegBranch -eq 'master') {
            'ffmpeg-master-latest-win64-lgpl-shared.zip'
        }
        else {
            "ffmpeg-n$FfmpegBranch-latest-win64-lgpl-shared-$FfmpegBranch.zip"
        }
        $urlFfmpeg = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$nomFfmpeg"

        try {
            $archiveFfmpeg = Get-Archive -Url $urlFfmpeg -NomFichier $nomFfmpeg
        }
        catch {
            Write-Echec "Téléchargement impossible : $nomFfmpeg" @"
Les builds LGPL shared disponibles dans la release « latest » de BtbN/FFmpeg-Builds sont
8.1, 9.0 et master. Relancer avec -FfmpegBranch 9.0 par exemple.
Message : $($_.Exception.Message)
"@
            exit 1
        }

        Expand-Vers -Archive $archiveFfmpeg -Destination $DossierFfmpeg

        if (-not (Test-Path $entete)) {
            Write-Echec "En-têtes FFmpeg introuvables après extraction (attendu : $entete)" `
                'Vérifier le contenu de third_party/ffmpeg.'
            exit 1
        }
        Write-Ok "FFmpeg installé : $DossierFfmpeg"
    }

    # Contrôle de licence et de contenu : on refuse tout build GPL,
    # et on vérifie que SRT est présent (nécessaire pour --transport srt).
    $ffmpegExe = Join-Path $DossierFfmpeg 'bin\ffmpeg.exe'
    if (Test-Path $ffmpegExe) {
        $banniere = & $ffmpegExe -hide_banner -version 2>&1 | Out-String
        if ($banniere -match '--enable-gpl') {
            Write-Echec 'Ce build FFmpeg est compilé avec --enable-gpl : interdit (règle 4 de CLAUDE.md).' `
                'Reprendre une archive « lgpl-shared ».'
            exit 1
        }
        Write-Ok 'Build FFmpeg sans --enable-gpl (conforme à la règle 4).'

        $versionLib = [regex]::Match($banniere, 'ffmpeg version (\S+)').Groups[1].Value
        Write-Ok "Version : $versionLib"

        # SRT : on ne se contente pas de la bannière de compilation, on demande à FFmpeg la
        # liste réelle des protocoles. C'est elle qui décide si --transport srt fonctionnera.
        # Format de `ffmpeg -protocols` : une colonne « Input: », une colonne « Output: ».
        $protocoles = & $ffmpegExe -hide_banner -protocols 2>&1 | Out-String
        $srtDisponible = $protocoles -match '(?m)^\s*srt\s*$'
        $libsrtCompile = $banniere -match '--enable-libsrt'

        if ($srtDisponible) {
            Write-Ok 'Protocole srt présent dans ffmpeg -protocols : --transport srt utilisable.'
        }
        else {
            Write-Avert @"
Protocole SRT ABSENT de ce build FFmpeg (ffmpeg -protocols ne le liste pas$(
    if ($libsrtCompile) { ' alors que la bannière annonce --enable-libsrt' })).
"@ @"
--transport srt ne fonctionnera pas ; utiliser --transport udp.
NE PAS basculer sur un build GPL pour obtenir SRT (règle 4 de CLAUDE.md).
Essayer une autre branche LGPL shared : scripts\setup.ps1 -FfmpegBranch 9.0 -Force
"@
        }
    }
    else {
        Write-Avert "ffmpeg.exe absent de $DossierFfmpeg\bin" 'Contrôle de licence impossible.'
    }

    # Les DLL de run-time, copiées plus tard à côté de l'exe par build.rs.
    $nbDll = @(Get-ChildItem -Path (Join-Path $DossierFfmpeg 'bin') -Filter '*.dll' -ErrorAction SilentlyContinue).Count
    Write-Ok "$nbDll DLL FFmpeg dans third_party/ffmpeg/bin."
}

# ---------------------------------------------------------------------------
# 4. .cargo/config.toml : variables vues par cargo et par les build scripts
# ---------------------------------------------------------------------------

Write-Titre '4. Configuration de cargo'

$dossierCargo = Join-Path $RacineProjet '.cargo'
New-Item -ItemType Directory -Force -Path $dossierCargo | Out-Null
$fichierConfig = Join-Path $dossierCargo 'config.toml'

$lignes = New-Object System.Collections.Generic.List[string]
$lignes.Add('# Généré par scripts/setup.ps1 — ne pas modifier à la main, relancer le script.')
$lignes.Add('# Ces variables sont vues par cargo et par les build scripts (ffmpeg-sys-next, build.rs).')
$lignes.Add('')
$lignes.Add('[env]')
$lignes.Add("SDL3_DIR = { value = `"$($DossierSdl -replace '\\','\\')`", force = true }")
if (-not $SkipFfmpeg) {
    $lignes.Add("FFMPEG_DIR = { value = `"$($DossierFfmpeg -replace '\\','\\')`", force = true }")
}
if ($script:DossierLibclang) {
    $lignes.Add("LIBCLANG_PATH = { value = `"$($script:DossierLibclang -replace '\\','\\')`", force = true }")
}

# UTF-8 *sans* BOM : cargo lit le TOML tel quel, un BOM le ferait échouer.
$sansBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllLines($fichierConfig, [string[]] $lignes, $sansBom)
Write-Ok "Écrit : $fichierConfig"
$lignes | Where-Object { $_ -and $_ -notlike '#*' } | ForEach-Object { Write-Host "         $_" }

# ---------------------------------------------------------------------------
# Bilan
# ---------------------------------------------------------------------------

Write-Titre 'Bilan'

if ($script:Avertissements.Count -gt 0) {
    Write-Host "  $($script:Avertissements.Count) avertissement(s) :" -ForegroundColor Yellow
    $script:Avertissements | ForEach-Object { Write-Host "    - $_" -ForegroundColor Yellow }
}
else {
    Write-Ok 'Aucun avertissement.'
}

Write-Host ''
Write-Host 'Environnement prêt. Étapes suivantes :' -ForegroundColor White
Write-Host '  cargo fmt --check'
Write-Host '  cargo clippy -- -D warnings'
Write-Host '  cargo build'
Write-Host '  cargo run -- --host 127.0.0.1'
Write-Host ''
