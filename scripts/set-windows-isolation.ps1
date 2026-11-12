# Run inside the Windows guest as Administrator/SYSTEM.
# Routing-only isolation; VirtIO serial agent/RPC is unaffected.
# Undo: -Remove, then renew DHCP (ipconfig /renew "Ethernet").
param([string]$InterfaceAlias = 'Ethernet', [switch]$Remove)
$ErrorActionPreference = 'Stop'
$Ignore = if ($Remove) { 'Disabled' } else { 'Enabled' }
foreach ($Store in @('PersistentStore', 'ActiveStore')) {
    Set-NetIPInterface -InterfaceAlias $InterfaceAlias -AddressFamily IPv4,IPv6 `
        -PolicyStore $Store -IgnoreDefaultRoutes $Ignore
}
if (-not $Remove) {
    foreach ($Store in @('PersistentStore', 'ActiveStore')) {
        Get-NetRoute -PolicyStore $Store |
            Where-Object { $_.InterfaceAlias -eq $InterfaceAlias -and $_.DestinationPrefix -in @('0.0.0.0/0', '::/0') } |
            Remove-NetRoute -Confirm:$false
    }
    $Defaults = @(Get-NetRoute -InterfaceAlias $InterfaceAlias |
        Where-Object { $_.DestinationPrefix -in @('0.0.0.0/0', '::/0') })
    if ($Defaults.Count -ne 0) { throw 'Default routes remain; isolation not applied' }
}
# Remove only the earlier Vroom block rule, not any normal Windows firewall rules.
Get-NetFirewallRule -PolicyStore PersistentStore -Name 'Vroom-Lab-Block-Outbound' -ErrorAction SilentlyContinue |
    Remove-NetFirewallRule
foreach ($Store in @('PersistentStore', 'ActiveStore')) {
    Get-NetIPInterface -InterfaceAlias $InterfaceAlias -PolicyStore $Store |
        Select-Object InterfaceAlias,AddressFamily,IgnoreDefaultRoutes,PolicyStore |
        ConvertTo-Json -Compress
}
if ($Remove) { Write-Output 'Default routes permitted again; renew DHCP to restore the gateway.' }
else { Write-Output 'ROUTING_ISOLATION_OK; Vroom firewall block removed.' }
