# Run inside a Windows guest, from an elevated PowerShell session.
# Read-only: reports configuration, never enables encryption or exports keys.
#   .\guest-readiness.ps1 -ExpectedArchitecture arm64
# Exit 0: required observations pass; 1: unmet; 2: unable to verify.
# A pass does not attest firmware variable protection, boot enforcement or a cold reboot.
[CmdletBinding()]
param(
    [ValidateSet('any', 'x64', 'arm64')]
    [string]$ExpectedArchitecture = 'any'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$errors = [System.Collections.Generic.List[object]]::new()
$architecture = $null
$os = $null
$secureBoot = $null
$tpm = $null
$bitlocker = $null
$checks = [ordered]@{
    windows_client_build = $null
    architecture_supported = $null
    architecture_matches = $null
    secure_boot_enabled = $null
    tpm2_ready = $null
    bitlocker_protected = $null
    bitlocker_tpm_protector = $null
}

try {
    $info = Get-CimInstance -ClassName Win32_OperatingSystem
    $os = [ordered]@{
        caption = $info.Caption
        version = $info.Version
        build = $info.BuildNumber
        product_type = [int]$info.ProductType
    }
    # Windows 11 starts at build 22000. Exclude Windows Server.
    $checks.windows_client_build = ($info.ProductType -eq 1 -and [int]$info.BuildNumber -ge 22000)
} catch {
    $errors.Add(@{ check = 'operating_system'; message = $_.Exception.Message })
}

try {
    # Native processor architecture also works from an emulated x64 process on ARM64.
    $ids = @(Get-CimInstance -ClassName Win32_Processor | Select-Object -ExpandProperty Architecture -Unique)
    if ($ids.Count -ne 1) { throw 'Native processor architecture is ambiguous or unavailable.' }
    $architecture = switch ([int]$ids[0]) { 9 { 'x64' } 12 { 'arm64' } default { 'unsupported' } }
    $checks.architecture_supported = ($architecture -in @('x64', 'arm64'))
    $checks.architecture_matches = ($ExpectedArchitecture -eq 'any' -or $architecture -eq $ExpectedArchitecture)
} catch {
    $errors.Add(@{ check = 'architecture'; message = $_.Exception.Message })
}

try {
    $secureBoot = [bool](Confirm-SecureBootUEFI)
    $checks.secure_boot_enabled = $secureBoot
} catch {
    $errors.Add(@{ check = 'secure_boot'; message = $_.Exception.Message })
}

try {
    $state = Get-Tpm
    $device = Get-CimInstance -Namespace 'root/CIMV2/Security/MicrosoftTpm' -ClassName Win32_Tpm
    $specVersion = if ($null -ne $device) { [string]$device.SpecVersion } else { $null }
    $isTpm2 = ($null -ne $specVersion -and ($specVersion -split ',')[0].Trim() -eq '2.0')
    $tpm = [ordered]@{
        present = [bool]$state.TpmPresent
        ready = [bool]$state.TpmReady
        enabled = [bool]$state.TpmEnabled
        activated = [bool]$state.TpmActivated
        spec_version = $specVersion
    }
    $checks.tpm2_ready = ($tpm.present -and $tpm.ready -and $tpm.enabled -and $tpm.activated -and $isTpm2)
} catch {
    $errors.Add(@{ check = 'tpm'; message = $_.Exception.Message })
}

try {
    $volume = Get-BitLockerVolume -MountPoint $env:SystemDrive
    # Whitelist these fields: never serialize the volume or KeyProtector objects.
    $protectorTypes = @($volume.KeyProtector | Where-Object { $null -ne $_ } | ForEach-Object { [string]$_.KeyProtectorType } | Sort-Object -Unique)
    $bitlocker = [ordered]@{
        mount_point = $volume.MountPoint
        volume_status = [string]$volume.VolumeStatus
        protection_status = [string]$volume.ProtectionStatus
        encryption_method = [string]$volume.EncryptionMethod
        encryption_percentage = [int]$volume.EncryptionPercentage
        protector_types = $protectorTypes
    }
    $checks.bitlocker_protected = (
        $bitlocker.volume_status -eq 'FullyEncrypted' -and
        $bitlocker.protection_status -eq 'On' -and
        $bitlocker.encryption_percentage -eq 100 -and
        $bitlocker.encryption_method -in @('Aes128', 'Aes256', 'XtsAes128', 'XtsAes256', 'HardwareEncryption')
    )
    $checks.bitlocker_tpm_protector = (@($protectorTypes | Where-Object { $_ -match '^Tpm' }).Count -gt 0)
} catch {
    $errors.Add(@{ check = 'bitlocker'; message = $_.Exception.Message })
}

$unknown = $false
$failed = $false
foreach ($value in $checks.Values) {
    if ($null -eq $value) { $unknown = $true }
    elseif (-not $value) { $failed = $true }
}
$status = if ($unknown) { 'unknown' } elseif ($failed) { 'fail' } else { 'pass' }
[ordered]@{
    schema_version = 1
    platform = 'windows'
    evidence_scope = 'Guest-reported configuration; firmware variable protection, boot enforcement and cold reboot require separate validation.'
    expected_architecture = $ExpectedArchitecture
    architecture = $architecture
    operating_system = $os
    secure_boot_enabled = $secureBoot
    tpm = $tpm
    bitlocker = $bitlocker
    checks = $checks
    errors = @($errors.ToArray())
    status = $status
} | ConvertTo-Json -Depth 8

if ($unknown) { exit 2 }
if ($failed) { exit 1 }
exit 0
