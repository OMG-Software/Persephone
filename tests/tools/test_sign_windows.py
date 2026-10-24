"""Refusal paths of tools/sign_windows.ps1.

These cases need no network and no certificate store: each test makes a
throwaway certificate in memory. Signing itself needs a timestamp server, so
the release workflow is the only place where a file is actually signed.
"""

import base64
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "sign_windows.ps1"
POWERSHELL = shutil.which("pwsh") or shutil.which("powershell")
PASSWORD = "test-pfx-password-7c1e"

# Writes a PFX with the given extended key usage and validity (days from now)
# to the path in $env:TEST_PFX.
MAKE_PFX = r"""
$ErrorActionPreference = 'Stop'
$rsa = [Security.Cryptography.RSA]::Create(2048)
$request = New-Object Security.Cryptography.X509Certificates.CertificateRequest(
    'CN=Persephone test signer', $rsa, [Security.Cryptography.HashAlgorithmName]::SHA256,
    [Security.Cryptography.RSASignaturePadding]::Pkcs1)
$usages = New-Object Security.Cryptography.OidCollection
[void]$usages.Add((New-Object Security.Cryptography.Oid($env:TEST_EKU)))
$request.CertificateExtensions.Add((New-Object Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension($usages, $false)))
$now = [DateTimeOffset]::Now
$certificate = $request.CreateSelfSigned($now.AddDays([int]$env:TEST_FROM), $now.AddDays([int]$env:TEST_TO))
[IO.File]::WriteAllBytes($env:TEST_PFX, $certificate.Export('Pfx', $env:TEST_PASSWORD))
"""

CODE_SIGNING = "1.3.6.1.5.5.7.3.3"
SERVER_AUTH = "1.3.6.1.5.5.7.3.1"


def find_signtool():
    kits = Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Windows Kits" / "10" / "bin"
    return next(iter(sorted(kits.glob("*/x64/signtool.exe"), reverse=True)), None) if kits.is_dir() else None


@unittest.skipUnless(os.name == "nt" and POWERSHELL, "needs Windows PowerShell or pwsh")
class SignWindowsRefusalTests(unittest.TestCase):
    def setUp(self):
        self.temp = Path(tempfile.mkdtemp(prefix="persephone-sign-test-"))
        self.addCleanup(shutil.rmtree, self.temp, ignore_errors=True)
        self.runner_temp = self.temp / "runner"
        self.runner_temp.mkdir()
        self.target = self.temp / "program.exe"
        self.target.write_bytes(b"MZ not a real program\n")
        self.state = self.temp / "state.txt"
        self.output = self.temp / "github-output.txt"
        self.output.write_text("", encoding="utf-8")

    def make_pfx(self, eku, valid_from=-1, valid_to=30):
        pfx = self.temp / f"certificate-{eku}-{valid_from}-{valid_to}.pfx"
        environment = dict(os.environ, TEST_PFX=str(pfx), TEST_EKU=eku, TEST_FROM=str(valid_from),
                           TEST_TO=str(valid_to), TEST_PASSWORD=PASSWORD)
        subprocess.run([POWERSHELL, "-NoProfile", "-NonInteractive", "-Command", MAKE_PFX],
                       env=environment, check=True, capture_output=True, timeout=120)
        return base64.b64encode(pfx.read_bytes()).decode("ascii")

    def run_script(self, *arguments, **variables):
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith("WINDOWS_") and key != "GITHUB_OUTPUT"}
        environment.update(RUNNER_TEMP=str(self.runner_temp), WINDOWS_SIGNATURE_STATE=str(self.state),
                           GITHUB_OUTPUT=str(self.output), WINDOWS_TIMESTAMP_URL="http://127.0.0.1:9")
        environment.update(variables)
        result = subprocess.run(
            [POWERSHELL, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(SCRIPT),
             *arguments],
            env=environment, capture_output=True, text=True, timeout=300)
        return result.returncode, result.stdout + result.stderr

    def assert_refused_before_signing(self, code, output, message):
        self.assertNotEqual(code, 0)
        self.assertIn(message, output)
        self.assertNotIn(PASSWORD, output)
        # THEN the file is untouched, nothing is reported as signed, and the
        # decoded certificate is gone.
        self.assertEqual(self.target.read_bytes(), b"MZ not a real program\n")
        self.assertFalse(self.state.exists())
        self.assertEqual(self.output.read_text(encoding="utf-8"), "")
        self.assertEqual(list(self.runner_temp.iterdir()), [])

    def test_certificate_without_code_signing_usage_is_refused(self):
        # GIVEN a valid certificate that only allows server authentication
        encoded = self.make_pfx(SERVER_AUTH)
        # WHEN the script is asked to sign a file with it
        code, output = self.run_script(str(self.target), WINDOWS_CERTIFICATE=encoded,
                                       WINDOWS_CERTIFICATE_PASSWORD=PASSWORD)
        self.assert_refused_before_signing(code, output, "not a code-signing certificate")

    def test_expired_certificate_is_refused(self):
        # GIVEN a code-signing certificate that expired yesterday
        encoded = self.make_pfx(CODE_SIGNING, valid_from=-30, valid_to=-1)
        code, output = self.run_script(str(self.target), WINDOWS_CERTIFICATE=encoded,
                                       WINDOWS_CERTIFICATE_PASSWORD=PASSWORD)
        self.assert_refused_before_signing(code, output, "WINDOWS_CERTIFICATE expired")

    def test_wrong_password_is_refused_without_printing_either_password(self):
        # GIVEN a code-signing certificate and the wrong password for it
        encoded = self.make_pfx(CODE_SIGNING)
        wrong = "wrong-password-5d2a"
        code, output = self.run_script(str(self.target), WINDOWS_CERTIFICATE=encoded,
                                       WINDOWS_CERTIFICATE_PASSWORD=wrong)
        self.assert_refused_before_signing(code, output, "could not be opened")
        self.assertNotIn(wrong, output)

    def test_invalid_base64_is_refused(self):
        code, output = self.run_script(str(self.target), WINDOWS_CERTIFICATE="not base64 !",
                                       WINDOWS_CERTIFICATE_PASSWORD=PASSWORD)
        self.assert_refused_before_signing(code, output, "not valid Base64")

    def test_missing_certificate_is_refused(self):
        # GIVEN no WINDOWS_CERTIFICATE WHEN signing is requested THEN it stops.
        code, output = self.run_script(str(self.target), WINDOWS_CERTIFICATE_PASSWORD=PASSWORD)
        self.assert_refused_before_signing(code, output, "WINDOWS_CERTIFICATE is not set")

    def test_missing_file_is_refused_before_the_certificate_is_decoded(self):
        encoded = self.make_pfx(CODE_SIGNING)
        code, output = self.run_script(str(self.temp / "missing.exe"), WINDOWS_CERTIFICATE=encoded,
                                       WINDOWS_CERTIFICATE_PASSWORD=PASSWORD)
        self.assert_refused_before_signing(code, output, "missing.exe")

    def test_verification_needs_the_expected_thumbprint(self):
        code, output = self.run_script("-VerifyOnly", str(self.target))
        self.assertNotEqual(code, 0)
        self.assertIn("WINDOWS_SIGNING_THUMBPRINT is not set", output)
        self.assertFalse(self.state.exists())

    @unittest.skipUnless(find_signtool(), "needs signtool from the Windows SDK")
    def test_verification_refuses_an_unsigned_file(self):
        # GIVEN a file without an Authenticode signature
        # WHEN it is verified against an expected signer
        code, output = self.run_script("-VerifyOnly", str(self.target), WINDOWS_SIGNING_THUMBPRINT="00" * 32)
        # THEN verification fails and records nothing.
        self.assertNotEqual(code, 0)
        self.assertIn("no Authenticode signature", output)
        self.assertFalse(self.state.exists())


if __name__ == "__main__":
    unittest.main()
