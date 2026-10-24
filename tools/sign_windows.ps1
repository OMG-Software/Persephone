<#
.SYNOPSIS
Signs Windows release files with Authenticode and verifies each signature.

.DESCRIPTION
This script is the only place that signs Windows release files. Its interface
is a list of files. To change the signing provider (for example to a cloud
signing service), replace this script and keep that interface.

The release workflow calls it in two stages:
  1. From Tauri's bundle.windows.signCommand hook while `cargo tauri bundle`
     runs, once for each file Tauri signs: the sidecars persephone.exe and
     persephone-media-worker.exe, the main binary persephone-ui.exe, NSIS
     plugins and uninstaller, WiX extensions, and the NSIS and MSI installers.
     These are the files that end up in the installers.
  2. With -VerifyOnly on the finished installers and on the executables
     extracted from the MSI.

Each file is signed with SHA-256 and an RFC 3161 timestamp, then checked with
`signtool verify /pa /v`. A chain that ends in an untrusted root (a
self-signed certificate) is accepted and recorded as "self-signed" when the
file's signer is the configured certificate; any other failure stops the run.

Environment (set by the workflow; values are never printed):
  WINDOWS_SIGNING_PFX           Path of the decoded PFX file.
  WINDOWS_CERTIFICATE_PASSWORD  PFX password.
  WINDOWS_TIMESTAMP_URL         RFC 3161 timestamp server.
  WINDOWS_SIGNING_THUMBPRINT    SHA-256 thumbprint of the signing certificate.
  WINDOWS_SIGNATURE_STATE       File that receives one line per verified file.
#>
param(
    [switch]$VerifyOnly,
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-Required([string]$Name) {
    $value = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrEmpty($value)) { throw "$Name is not set" }
    return $value
}

function Get-Sha256Thumbprint($Certificate) {
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try { return -join ($sha256.ComputeHash($Certificate.RawData) | ForEach-Object { $_.ToString('X2') }) }
    finally { $sha256.Dispose() }
}

function Find-SignTool {
    # The Windows SDK on GitHub-hosted runners installs signtool per SDK
    # version; use the newest x64 build that exists.
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $found = Get-ChildItem -Path $kits -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^\d+(\.\d+){3}$' } |
        Sort-Object { [version]$_.Name } -Descending |
        ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' } |
        Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
        Select-Object -First 1
    if (-not $found) { throw "signtool.exe was not found under $kits" }
    return $found
}

function Invoke-SignTool([string]$SignTool, [string[]]$Arguments) {
    # Output is returned to the caller, never echoed with the arguments,
    # because the arguments contain the PFX password.
    # signtool writes errors to stderr; collect them as text instead of
    # letting PowerShell turn them into terminating errors.
    $ErrorActionPreference = 'Continue'
    $output = & $SignTool @Arguments 2>&1 | ForEach-Object { "$_" } | Out-String
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = $output }
}

function Sign-File([string]$SignTool, [string]$File) {
    $pfx = Get-Required 'WINDOWS_SIGNING_PFX'
    $password = Get-Required 'WINDOWS_CERTIFICATE_PASSWORD'
    $timestamp = Get-Required 'WINDOWS_TIMESTAMP_URL'
    $arguments = @('sign', '/fd', 'SHA256', '/td', 'SHA256', '/tr', $timestamp, '/f', $pfx, '/p', $password, '/d', 'Persephone', $File)
    # Timestamp servers fail transiently; retry the whole signing a few times.
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        $result = Invoke-SignTool $SignTool $arguments
        if ($result.ExitCode -eq 0) { return }
        Write-Host "signtool sign failed for $File (attempt $attempt of 3):"
        Write-Host ($result.Output.Replace($password, '***'))
        Start-Sleep -Seconds (5 * $attempt)
    }
    throw "signing failed: $File"
}

function Test-File([string]$SignTool, [string]$File) {
    $expected = Get-Required 'WINDOWS_SIGNING_THUMBPRINT'
    $result = Invoke-SignTool $SignTool @('verify', '/pa', '/v', $File)
    $signature = Get-AuthenticodeSignature -LiteralPath $File
    if ($null -eq $signature.SignerCertificate) { throw "no Authenticode signature: $File" }
    $signer = Get-Sha256Thumbprint $signature.SignerCertificate
    if ($signer -ne $expected) { throw "signed by an unexpected certificate ($signer): $File" }
    if ($null -eq $signature.TimeStamperCertificate) { throw "signature has no timestamp: $File" }
    if ($result.ExitCode -eq 0) { return 'trusted' }

    # Accept exactly one failure: a chain that ends in a root this machine
    # does not trust, for a certificate that is its own issuer. A changed file
    # (HashMismatch), a missing signature or any other chain error fails.
    $errors = ([regex]::Matches($result.Output, 'SignTool Error:')).Count
    $untrustedRoot = $errors -eq 1 -and $result.Output -match 'terminated in a root\s+certificate which is not trusted'
    $selfIssued = $signature.SignerCertificate.Subject -eq $signature.SignerCertificate.Issuer
    if ($untrustedRoot -and $selfIssued -and $signature.Status -ne 'HashMismatch' -and $signature.Status -ne 'NotSigned') {
        return 'self-signed'
    }
    Write-Host $result.Output
    throw "signature verification failed ($($signature.Status)): $File"
}

$signTool = Find-SignTool
$state = Get-Required 'WINDOWS_SIGNATURE_STATE'
foreach ($item in $Path) {
    $file = (Resolve-Path -LiteralPath $item).ProviderPath
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "not a file: $item" }
    if (-not $VerifyOnly) { Sign-File $signTool $file }
    $trust = Test-File $signTool $file
    Add-Content -LiteralPath $state -Value "$trust $([IO.Path]::GetFileName($file))" -Encoding utf8
    Write-Host "Authenticode ${trust}: $file"
}
