# client_setup.ps1 - préparation du PC PORTABLE (une seule fois)
# Dans un PowerShell ADMINISTRATEUR, depuis le dossier qui contient client.py :
#   powershell -ExecutionPolicy Bypass -File .\client_setup.ps1
#Requires -RunAsAdministrator
$ErrorActionPreference = "Stop"

Write-Host "`n[1/3] Bibliothèques Python" -ForegroundColor Cyan
& python -m pip install --upgrade zeroconf spake2 pygame pynput sounddevice
if ($LASTEXITCODE -ne 0) { throw "échec de pip install" }

Write-Host "`n[2/3] FFmpeg (ffplay)" -ForegroundColor Cyan
if (Get-Command ffplay -ErrorAction SilentlyContinue) {
    Write-Host "  déjà installé"
} else {
    winget install -e --id Gyan.FFmpeg --silent --accept-package-agreements --accept-source-agreements
    Write-Host "  installé : ouvre un NOUVEAU terminal pour qu'il soit dans le PATH"
}

Write-Host "`n[3/3] Pare-feu (réception vidéo, audio, découverte)" -ForegroundColor Cyan
Get-NetFirewallRule -DisplayName "CloudGaming*" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName "CloudGaming - video (UDP 5000)" -Direction Inbound -Protocol UDP -LocalPort 5000 -Action Allow -Profile Private | Out-Null
New-NetFirewallRule -DisplayName "CloudGaming - audio (UDP 5002)" -Direction Inbound -Protocol UDP -LocalPort 5002 -Action Allow -Profile Private | Out-Null
New-NetFirewallRule -DisplayName "CloudGaming - mDNS (UDP 5353)" -Direction Inbound -Protocol UDP -LocalPort 5353 -Action Allow -Profile Private | Out-Null
Get-NetFirewallRule -Action Block -ErrorAction SilentlyContinue | Where-Object DisplayName -match "python|ffplay" | Remove-NetFirewallRule
$public = Get-NetConnectionProfile | Where-Object NetworkCategory -eq "Public"
if ($public) { Write-Host "  ATTENTION : réseau '$($public.Name)' en profil Public -> passe-le en Privé" -ForegroundColor Yellow }

Write-Host "`nTerminé. Ensuite : python client.py discover" -ForegroundColor Green
