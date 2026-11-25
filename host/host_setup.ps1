# host_setup.ps1 - installation de l'agent sur le PC FIXE (une seule fois)
# A lancer depuis le dossier qui contient agent.py, dans un PowerShell ADMINISTRATEUR :
#   powershell -ExecutionPolicy Bypass -File .\host_setup.ps1
#Requires -RunAsAdministrator
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$task = "CloudGamingAgent"

Write-Host "`n[1/5] Python" -ForegroundColor Cyan
$py = (& py -c "import sys; print(sys.executable)").Trim()
$ver = (& py -c "import sys; print('%d.%d' % sys.version_info[:2])").Trim()
if ([version]$ver -lt [version]"3.11") { throw "Python 3.11 minimum requis (trouvé : $ver)" }
$pyw = Join-Path (Split-Path $py) "pythonw.exe"
Write-Host "  $py ($ver)"

Write-Host "`n[2/5] Bibliothèques de l'agent" -ForegroundColor Cyan
& py -m pip install --upgrade fastapi uvicorn zeroconf cryptography spake2 psutil
if ($LASTEXITCODE -ne 0) { throw "échec de pip install" }

Write-Host "`n[3/5] Pare-feu" -ForegroundColor Cyan
Get-NetFirewallRule -DisplayName "CloudGaming*" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName "CloudGaming - agent (TCP 47800)" -Direction Inbound -Protocol TCP -LocalPort 47800 -Action Allow -Profile Private | Out-Null
New-NetFirewallRule -DisplayName "CloudGaming - entrees (UDP 5001)" -Direction Inbound -Protocol UDP -LocalPort 5001 -Action Allow -Profile Private | Out-Null
New-NetFirewallRule -DisplayName "CloudGaming - mDNS (UDP 5353)" -Direction Inbound -Protocol UDP -LocalPort 5353 -Action Allow -Profile Private | Out-Null
# Une ancienne règle "bloquer Python" (fenêtre du pare-feu refusée) passerait avant les nôtres
Get-NetFirewallRule -Action Block -ErrorAction SilentlyContinue | Where-Object DisplayName -match "python" | Remove-NetFirewallRule
Write-Host "  règles créées (profil Privé)"

$public = Get-NetConnectionProfile | Where-Object NetworkCategory -eq "Public"
if ($public) {
    Write-Host "  ATTENTION : le réseau '$($public.Name)' est en profil Public : la détection et les connexions seront bloquées." -ForegroundColor Yellow
    $r = Read-Host "  Le passer en Privé (réseau de confiance, ex. ta box) ? [o/N]"
    if ($r -eq "o") { $public | Set-NetConnectionProfile -NetworkCategory Private; Write-Host "  passé en Privé" }
}

Write-Host "`n[4/5] Démarrage automatique (tâche planifiée '$task')" -ForegroundColor Cyan
$action    = New-ScheduledTaskAction -Execute $pyw -Argument "`"$here\agent.py`"" -WorkingDirectory $here
$trigger   = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
# Session interactive (obligatoire pour capturer l'écran) + droits élevés (installations, jeux admin)
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest
$settings  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
             -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName $task -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
Write-Host "  l'agent démarrera à chaque ouverture de ta session Windows"

Write-Host "`n[5/5] Démarrage de l'agent" -ForegroundColor Cyan
Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
Start-ScheduledTask -TaskName $task
Start-Sleep -Seconds 4
if ((Test-NetConnection -ComputerName localhost -Port 47800 -WarningAction SilentlyContinue).TcpTestSucceeded) {
    Write-Host "  agent en ligne sur le port 47800" -ForegroundColor Green
} else {
    Write-Host "  l'agent ne répond pas : regarde $env:APPDATA\CloudGamingAgent\logs\agent.log" -ForegroundColor Red
}

Write-Host "`nTerminé. Sur le PC portable : python client.py discover, puis python client.py pair" -ForegroundColor Green
Write-Host "Le reste (FFmpeg, ViGEmBus, bibliothèques audio/manette) s'installe depuis le portable :"
Write-Host "  python client.py install all"
