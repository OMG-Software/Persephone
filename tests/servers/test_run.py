import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError

from tools.servers import run


class RegistrationTests(unittest.TestCase):
    def test_each_account_uses_its_own_uia_session(self):
        calls = []

        def fake_request(_base, _path, body):
            calls.append(json.loads(json.dumps(body)))
            if "session" not in body["auth"]:
                session = f"session-{body['username']}"
                # A Matrix v1.19 authentication challenge.
                challenge = {
                    "flows": [{"stages": ["m.login.registration_token"]}],
                    "params": {}, "session": session}
                raise HTTPError(
                    "http://127.0.0.1/register", 401, "UIA", {},
                    io.BytesIO(json.dumps(challenge).encode()))
            return {"user_id": f"@{body['username']}:localhost"}

        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as directory:
            output = Path(directory)
            registration_auth = {
                "type": "m.login.registration_token", "token": "token-secret"}
            with patch.object(run, "request", side_effect=fake_request):
                run.register_account("http://127.0.0.1", "test", "p0_alice",
                                     "alice-secret", registration_auth, output)
                run.register_account("http://127.0.0.1", "test", "p0_bob",
                                     "bob-secret", registration_auth, output)

        self.assertEqual([call["username"] for call in calls], [
            "p0_alice", "p0_alice", "p0_bob", "p0_bob"])
        self.assertEqual([call["auth"].get("session") for call in calls], [
            None, "session-p0_alice", None, "session-p0_bob"])
        self.assertEqual(registration_auth.get("session"), None)
        self.assertEqual([call["password"] for call in calls], [
            "alice-secret", "alice-secret", "bob-secret", "bob-secret"])

    def test_retained_registration_error_redacts_password_and_token(self):
        password = "password-secret"
        token = "registration-token-secret"
        payload = json.dumps({"password": password, "detail": token})
        error = HTTPError("http://127.0.0.1/register", 403, "denied", {},
                          io.BytesIO(payload.encode()))

        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as directory:
            output = Path(directory)
            with patch.object(run, "request", side_effect=error):
                with self.assertRaisesRegex(RuntimeError, "HTTP 403"):
                    run.register_account(
                        "http://127.0.0.1", "test", "p0_alice", password,
                        {"type": "m.login.registration_token", "token": token},
                        output)

            retained = (output / "test-registration-error.txt").read_text()
            self.assertNotIn(password, retained)
            self.assertNotIn(token, retained)
            self.assertEqual(retained.count("[REDACTED]"), 2)


if __name__ == "__main__":
    unittest.main()
