<#
.SYNOPSIS
Signs Windows release files with Authenticode and verifies each signature.

.DESCRIPTION
This script is the only place that signs Windows release files. Its interface
is a list of files. To change the signing provider (for example to a cloud
signing service), replace this script and keep that interface.

The release workflow calls it in three jobs:
  1. windows-sign-binaries signs persephone.exe, persephone-ui.exe and
     persephone-media-worker.exe before the bundler packs them.
  2. windows-sign-installers signs the NSIS and MSI installers.
  3. windows-verify calls it with -VerifyOnly on the installers and on the
     executables extracted from them. That job has no secrets.
The two signing jobs build no code. They run only this repository's scripts,
the pinned actions and signtool.

In signing mode the script decodes WINDOWS_CERTIFICATE into a new directory
that only the current account can read. It checks that the certificate has a
private key, allows code signing and is valid now. It signs each file with
SHA-256 and an RFC 3161 timestamp and then verifies it. It deletes the decoded
file before it exits, also after a failure. When GITHUB_OUTPUT is set, it
appends the certificate's `subject=` and SHA-256 `thumbprint=`.

Each file is checked with `signtool verify /pa /v`. A chain that ends in an
untrusted root (a self-signed certificate) is accepted and recorded as
"self-signed" when the file's signer is the expected certificate; any other
failure stops the run.

Environment (values are never printed):
  WINDOWS_CERTIFICATE           Signing: Base64 of the PFX file.
  WINDOWS_CERTIFICATE_PASSWORD  Signing: PFX password.
  WINDOWS_TIMESTAMP_URL         Signing: RFC 3161 timestamp server.
  WINDOWS_SIGNING_THUMBPRINT    -VerifyOnly: SHA-256 thumbprint of the signer.
  WINDOWS_SIGNATURE_STATE       File that receives one line per verified file.
#>
param(
    [switch]$VerifyOnly,
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$CodeSigningOid = '1.3.6.1.5.5.7.3.3'

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

function New-PrivateDirectory {
    # Only the current account may read the directory and what it holds.
    $base = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
    $directory = Join-Path $base ('persephone-signing-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $directory | Out-Null
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)
    $me = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($me, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
    Set-Acl -Path $directory -AclObject $acl
    return $directory
}

function Open-Certificate([string]$Directory) {
    $encoded = Get-Required 'WINDOWS_CERTIFICATE'
    $password = Get-Required 'WINDOWS_CERTIFICATE_PASSWORD'
    try { $bytes = [Convert]::FromBase64String($encoded) }
    catch { throw 'WINDOWS_CERTIFICATE is not valid Base64' }
    $pfx = Join-Path $Directory 'certificate.pfx'
    [IO.File]::WriteAllBytes($pfx, $bytes)
    $flags = [Security.Cryptography.X509Certificates.X509KeyStorageFlags]::EphemeralKeySet
    # The exception text is not passed on; it is not needed and is not
    # guaranteed to be free of secret material.
    try { $certificate = New-Object Security.Cryptography.X509Certificates.X509Certificate2($pfx, $password, $flags) }
    catch { throw 'WINDOWS_CERTIFICATE could not be opened with WINDOWS_CERTIFICATE_PASSWORD' }
    try {
        if (-not $certificate.HasPrivateKey) { throw 'WINDOWS_CERTIFICATE has no private key' }
        $eku = @($certificate.Extensions | Where-Object { $_ -is [Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension] })
        $codeSigning = @($eku | ForEach-Object { $_.EnhancedKeyUsages } | Where-Object { $_.Value -eq $CodeSigningOid })
        if ($codeSigning.Count -eq 0) { throw 'WINDOWS_CERTIFICATE is not a code-signing certificate' }
        $now = Get-Date
        if ($certificate.NotAfter -lt $now) { throw "WINDOWS_CERTIFICATE expired on $($certificate.NotAfter)" }
        if ($certificate.NotBefore -gt $now) { throw "WINDOWS_CERTIFICATE is not valid before $($certificate.NotBefore)" }
        # The subject goes into GITHUB_OUTPUT, one value per line.
        if ($certificate.Subject -match '[\r\n]') { throw 'WINDOWS_CERTIFICATE has a subject with a line break' }
        return [pscustomobject]@{
            Pfx        = $pfx
            Password   = $password
            Subject    = $certificate.Subject
            Thumbprint = Get-Sha256Thumbprint $certificate
            NotAfter   = $certificate.NotAfter
        }
    }
    finally { $certificate.Dispose() }
}

function Sign-File([string]$SignTool, $Certificate, [string]$File) {
    $timestamp = Get-Required 'WINDOWS_TIMESTAMP_URL'
    $arguments = @('sign', '/fd', 'SHA256', '/td', 'SHA256', '/tr', $timestamp, '/f', $Certificate.Pfx, '/p', $Certificate.Password, '/d', 'Persephone', $File)
    # Timestamp servers fail transiently; retry the whole signing a few times.
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        $result = Invoke-SignTool $SignTool $arguments
        if ($result.ExitCode -eq 0) { return }
        Write-Host "signtool sign failed for $File (attempt $attempt of 3):"
        Write-Host ($result.Output.Replace($Certificate.Password, '***'))
        Start-Sleep -Seconds (5 * $attempt)
    }
    throw "signing failed: $File"
}

function Test-File([string]$SignTool, [string]$Expected, [string]$File) {
    $result = Invoke-SignTool $SignTool @('verify', '/pa', '/v', $File)
    $signature = Get-AuthenticodeSignature -LiteralPath $File
    if ($null -eq $signature.SignerCertificate) { throw "no Authenticode signature: $File" }
    $signer = Get-Sha256Thumbprint $signature.SignerCertificate
    if ($signer -ne $Expected) { throw "signed by an unexpected certificate ($signer): $File" }
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

function Write-State([string]$State, [string]$Trust, [string]$File) {
    Add-Content -LiteralPath $State -Value "$Trust $([IO.Path]::GetFileName($File))" -Encoding utf8
    Write-Host "Authenticode ${Trust}: $File"
}

$state = Get-Required 'WINDOWS_SIGNATURE_STATE'
$files = foreach ($item in $Path) {
    $file = (Resolve-Path -LiteralPath $item).ProviderPath
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "not a file: $item" }
    $file
}

if ($VerifyOnly) {
    $expected = Get-Required 'WINDOWS_SIGNING_THUMBPRINT'
    $signTool = Find-SignTool
    foreach ($file in $files) { Write-State $state (Test-File $signTool $expected $file) $file }
    exit 0
}

$directory = New-PrivateDirectory
try {
    $certificate = Open-Certificate $directory
    Write-Host "Signing certificate: $($certificate.Subject), SHA-256 $($certificate.Thumbprint), valid until $($certificate.NotAfter)"
    $signTool = Find-SignTool
    foreach ($file in $files) {
        Sign-File $signTool $certificate $file
        Write-State $state (Test-File $signTool $certificate.Thumbprint $file) $file
    }
    if ($env:GITHUB_OUTPUT) {
        Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value "subject=$($certificate.Subject)" -Encoding utf8
        Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value "thumbprint=$($certificate.Thumbprint)" -Encoding utf8
    }
}
finally {
    Remove-Item -LiteralPath $directory -Recurse -Force -ErrorAction SilentlyContinue
}
