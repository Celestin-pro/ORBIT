<#
.SYNOPSIS
    Gère les worktrees git des agents qui travaillent en parallèle sur orbit-player.

.DESCRIPTION
    Chaque agent travaille dans son propre dossier (worktree git) sur sa propre branche
    `agent/<nom>`, créée depuis `main`. Les agents ne se marchent donc jamais dessus : ni
    fichiers modifiés en même temps, ni `cargo build` qui se bloquent sur le même `target/`.

    Les worktrees sont créés dans `.claude/worktrees/<nom>` (même emplacement que les worktrees
    créés automatiquement par Claude Code), ignoré par git.

.EXAMPLE
    ./scripts/agent.ps1 new audio-jitter       # crée .claude/worktrees/audio-jitter sur agent/audio-jitter
    ./scripts/agent.ps1 list                   # liste les worktrees et leur avance sur main
    ./scripts/agent.ps1 merge audio-jitter     # vérifie puis fusionne agent/audio-jitter dans main
    ./scripts/agent.ps1 remove audio-jitter    # supprime le worktree et la branche (si fusionnée)
#>
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('new', 'list', 'merge', 'remove')]
    [string]$Action,

    [Parameter(Position = 1)]
    [string]$Nom
)

$ErrorActionPreference = 'Stop'
$Racine = Split-Path -Parent $PSScriptRoot
$DossierWorktrees = Join-Path $Racine '.claude\worktrees'

function Invoke-Git {
    # Lance git et s'arrête si la commande échoue (PowerShell 5.1 ne le fait pas tout seul).
    & git -C $Racine @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') a échoué (code $LASTEXITCODE)." }
}

function Assert-Nom {
    if (-not $Nom) { throw "Nom de l'agent manquant. Exemple : ./scripts/agent.ps1 $Action audio-jitter" }
    if ($Nom -notmatch '^[a-z0-9][a-z0-9-]*$') {
        throw "Nom invalide '$Nom' : minuscules, chiffres et tirets uniquement."
    }
}

switch ($Action) {
    'new' {
        Assert-Nom
        $chemin = Join-Path $DossierWorktrees $Nom
        if (Test-Path $chemin) { throw "Le worktree $chemin existe déjà." }

        Invoke-Git worktree add -b "agent/$Nom" $chemin main

        # .cargo/config.toml est ignoré par git : on le copie pour que cargo trouve FFmpeg/SDL3
        # (il pointe vers le third_party du dépôt principal, partagé par tous les worktrees).
        $config = Join-Path $Racine '.cargo\config.toml'
        if (Test-Path $config) {
            New-Item -ItemType Directory -Force (Join-Path $chemin '.cargo') | Out-Null
            Copy-Item $config (Join-Path $chemin '.cargo\config.toml')
        } else {
            Write-Warning '.cargo/config.toml absent : lancez d''abord scripts/setup.ps1 dans le dépôt principal.'
        }

        Write-Host ''
        Write-Host "Worktree prêt : $chemin (branche agent/$Nom)" -ForegroundColor Green
        Write-Host "Lancer l'agent :  cd `"$chemin`"; claude"
    }

    'list' {
        Invoke-Git worktree list
        Write-Host ''
        $branches = & git -C $Racine for-each-ref --format='%(refname:short)' 'refs/heads/agent/'
        foreach ($b in $branches) {
            $avance = & git -C $Racine rev-list --count "main..$b"
            $retard = & git -C $Racine rev-list --count "$b..main"
            Write-Host ("{0,-35} {1,3} commit(s) d'avance, {2,3} de retard sur main" -f $b, $avance, $retard)
        }
    }

    'merge' {
        Assert-Nom
        $chemin = Join-Path $DossierWorktrees $Nom
        $branche = "agent/$Nom"

        # 1. Le worktree doit être propre : tout le travail de l'agent est commité.
        if (Test-Path $chemin) {
            $sale = & git -C $chemin status --porcelain
            if ($sale) { throw "Le worktree $chemin contient des modifications non commitées." }
        }

        # 2. Le dépôt principal doit être sur main et propre.
        $courante = & git -C $Racine branch --show-current
        if ($courante -ne 'main') { throw "Le dépôt principal est sur '$courante', pas sur main." }
        if (& git -C $Racine status --porcelain) { throw 'Le dépôt principal a des modifications non commitées.' }

        # 3. Rebase de la branche sur main (historique linéaire), puis vérification complète.
        if (Test-Path $chemin) {
            & git -C $chemin rebase main
            if ($LASTEXITCODE -ne 0) {
                throw "Conflit pendant le rebase de $branche sur main : résoudre dans $chemin (git rebase --continue), puis relancer."
            }
            Push-Location $chemin
            try {
                cargo fmt --check;               if ($LASTEXITCODE -ne 0) { throw 'cargo fmt --check a échoué.' }
                cargo clippy -- -D warnings;     if ($LASTEXITCODE -ne 0) { throw 'cargo clippy a échoué.' }
                cargo test;                      if ($LASTEXITCODE -ne 0) { throw 'cargo test a échoué.' }
            } finally { Pop-Location }
        }

        # 4. Fusion en avance rapide uniquement : main ne reçoit que du code vérifié ci-dessus.
        Invoke-Git merge --ff-only $branche
        Write-Host "$branche fusionnée dans main." -ForegroundColor Green
    }

    'remove' {
        Assert-Nom
        $chemin = Join-Path $DossierWorktrees $Nom
        if (Test-Path $chemin) { Invoke-Git worktree remove $chemin }
        # -d (et non -D) : git refuse de supprimer une branche non fusionnée.
        & git -C $Racine branch -d "agent/$Nom"
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "Branche agent/$Nom conservée (pas encore fusionnée dans main). Forcer : git branch -D agent/$Nom"
        }
    }
}
