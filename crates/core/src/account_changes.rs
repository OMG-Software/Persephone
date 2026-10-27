//! Changing this account's password and its email addresses and phone
//! numbers (ADR-0019, ADR-0020).
//!
//! * The password: the current one and the new one (twice) are entered in
//!   the core's own windows (`lib.rs` asks for them), never in the page. The
//!   change sends `logout_devices` as the page chose; the specification
//!   signs out the other devices by default.
//! * Email addresses and phone numbers (3PIDs) are not secret, so the page
//!   sends them. They are checked here before anything is sent: an email
//!   address is printable ASCII with one `@`, a phone number is digits with
//!   a two-letter country code. Adding one takes three steps: the homeserver
//!   sends a link (email) or a code (phone); the user opens the link, or
//!   types the code in the core's own window; then the address is added with
//!   the account password, also in the core's window. Nothing is bound to an
//!   identity server, and no identity server is named.
//! * A phone code goes to the `submit_url` the homeserver gave, only when it
//!   is on the homeserver itself (same scheme, host and port). Without one,
//!   confirming needs an identity server, so the medium is reported as not
//!   supported.
//! * User-interactive authentication follows a flow made of the password
//!   step alone; any other flow is `unsupported_auth`. A refused password is
//!   `invalid_password`. Nothing is retried.
//! * A refusal is reported by its Matrix error code, as a fixed
//!   [`AccountChange`] code. A homeserver without the endpoints
//!   (`M_UNRECOGNIZED`) reports the medium as not supported.
//!
//! The SDK copies passwords into its requests; those copies are outside the
//! zeroing guarantee, as at sign-in.

use crate::credentials::{
    CredentialSource, NewPassword, PasswordPurpose, PromptError, SecretPrompt,
};
use crate::views::project_name;
use matrix_sdk::{
    Client, HttpError,
    ruma::{
        OwnedClientSecret, OwnedSessionId, UInt, UserId,
        api::{
            client::{
                account::change_password,
                discovery::get_capabilities,
                uiaa::{
                    AuthData, AuthFlow, AuthType, MatrixUserIdentifier, Password, UiaaInfo,
                    UserIdentifier,
                },
            },
            error::ErrorKind,
        },
        thirdparty::Medium,
    },
};
use persephone_contracts::{AccountChange, ThreePid, ThreePidList, ThreePidMedium};
use persephone_core_protocol::{
    COUNTRY_CODE_BYTES, MAX_EMAIL_BYTES, MAX_PHONE_NUMBER_BYTES, MAX_THREE_PID_ADDRESS_BYTES,
    MAX_THREE_PIDS,
};
use zeroize::Zeroizing;

/// Longest answer a `submit_url` may give, in bytes.
const MAX_SUBMIT_REPLY_BYTES: usize = 4096;

/// An email address the page sent: printable ASCII without spaces, one `@`
/// with text on both sides, and a dot in the domain, within the limit. So it
/// is also safe to show in a native window.
pub fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    email.len() <= MAX_EMAIL_BYTES
        && !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && email.bytes().all(|byte| byte.is_ascii_graphic())
}

/// A country code: two uppercase ASCII letters.
pub fn valid_country(country: &str) -> bool {
    country.len() == COUNTRY_CODE_BYTES && country.bytes().all(|byte| byte.is_ascii_uppercase())
}

/// A phone number as dialled in its country: digits, at least four, within
/// the limit. Spaces and signs are for the page to remove.
pub fn valid_phone_number(number: &str) -> bool {
    (4..=MAX_PHONE_NUMBER_BYTES).contains(&number.len())
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// An address the page names for removal: within the limit, printable ASCII
/// (both media are), and for a phone number digits only.
pub fn valid_address(medium: ThreePidMedium, address: &str) -> bool {
    !address.is_empty()
        && address.len() <= MAX_THREE_PID_ADDRESS_BYTES
        && match medium {
            ThreePidMedium::Email => valid_email(address),
            ThreePidMedium::Msisdn => address.bytes().all(|byte| byte.is_ascii_digit()),
        }
}

fn medium(medium: ThreePidMedium) -> Medium {
    match medium {
        ThreePidMedium::Email => Medium::Email,
        ThreePidMedium::Msisdn => Medium::Msisdn,
    }
}

/// The fixed code for a refusal with this Matrix error code.
pub fn refusal(kind: &ErrorKind) -> AccountChange {
    match kind {
        ErrorKind::WeakPassword => AccountChange::WeakPassword,
        ErrorKind::ThreepidAuthFailed => AccountChange::NotConfirmed,
        ErrorKind::ThreepidInUse => AccountChange::InUse,
        ErrorKind::ThreepidDenied => AccountChange::Denied,
        ErrorKind::ThreepidNotFound => AccountChange::NothingPending,
        ErrorKind::ThreepidMediumNotSupported
        | ErrorKind::ServerNotTrusted
        | ErrorKind::Unrecognized => AccountChange::MediumNotSupported,
        ErrorKind::Forbidden => AccountChange::ChangesDisabled,
        ErrorKind::LimitExceeded(_) => AccountChange::RateLimited,
        _ => AccountChange::Failed,
    }
}

fn http_error(error: &HttpError) -> AccountChange {
    match error.client_api_error_kind() {
        Some(kind) => refusal(kind),
        None if matches!(error, HttpError::Reqwest(_)) => AccountChange::Unreachable,
        None => AccountChange::Failed,
    }
}

fn sdk_error(error: &matrix_sdk::Error) -> AccountChange {
    match error {
        matrix_sdk::Error::Http(http) => http_error(http),
        _ => AccountChange::Failed,
    }
}

/// Whether the homeserver offers a flow made of the password step alone.
fn password_flow(flows: &[AuthFlow]) -> bool {
    flows
        .iter()
        .any(|flow| flow.stages.as_slice() == [AuthType::Password])
}

/// What to do with a user-interactive authentication answer: send the
/// password once, or stop. A second answer after the password was sent
/// means it was refused.
fn next_auth(info: &UiaaInfo, sent_password: bool) -> Result<(), AccountChange> {
    if sent_password {
        return Err(AccountChange::InvalidPassword);
    }
    if password_flow(&info.flows) {
        Ok(())
    } else {
        Err(AccountChange::UnsupportedAuth)
    }
}

fn password_auth(user_id: &UserId, password: &str, session: Option<String>) -> AuthData {
    let mut auth = Password::new(
        UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.to_string())),
        password.to_owned(),
    );
    auth.session = session;
    AuthData::Password(auth)
}

/// Runs `send` without authentication, then once more with the password if
/// the homeserver asks for it.
async fn with_password<T>(
    user_id: &UserId,
    password: &Zeroizing<String>,
    send: impl AsyncFn(Option<AuthData>) -> Result<T, matrix_sdk::Error>,
) -> Result<T, AccountChange> {
    let mut auth = None;
    let mut sent_password = false;
    loop {
        let error = match send(auth.take()).await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(info) = error.as_uiaa_response() else {
            return Err(sdk_error(&error));
        };
        next_auth(info, sent_password)?;
        auth = Some(password_auth(user_id, password, info.session.clone()));
        sent_password = true;
    }
}

/// Changes the password. `sign_out_others` signs out every other device.
pub async fn change_password(
    client: &Client,
    user_id: &UserId,
    current: &Zeroizing<String>,
    new: &Zeroizing<String>,
    sign_out_others: bool,
) -> AccountChange {
    let result = with_password(user_id, current, async |auth| {
        let mut request = change_password::v3::Request::new(new.to_string());
        request.logout_devices = sign_out_others;
        request.auth = auth;
        client.send(request).await.map_err(matrix_sdk::Error::from)
    })
    .await;
    match result {
        Ok(_) => AccountChange::Done,
        Err(code) => code,
    }
}

/// Whether the homeserver lets this account change its 3PIDs. On unless its
/// capabilities say otherwise; capabilities it cannot give count as on, and
/// the change itself then answers.
async fn can_change(client: &Client) -> bool {
    match client.send(get_capabilities::v3::Request::new()).await {
        Ok(response) => response.capabilities.thirdparty_id_changes.enabled,
        Err(_) => true,
    }
}

/// The account's email addresses and phone numbers, at most
/// [`MAX_THREE_PIDS`]. A homeserver without the endpoint lists none.
pub async fn list(
    client: &Client,
    pending: Option<ThreePid>,
) -> Result<ThreePidList, AccountChange> {
    let listed = match client.account().get_3pids().await {
        Ok(response) => response.threepids,
        Err(error) if sdk_error(&error) == AccountChange::MediumNotSupported => Vec::new(),
        Err(error) => return Err(sdk_error(&error)),
    };
    let truncated = listed.len() > MAX_THREE_PIDS;
    let three_pids = listed
        .iter()
        .filter_map(|three_pid| {
            let medium = match three_pid.medium {
                Medium::Email => ThreePidMedium::Email,
                Medium::Msisdn => ThreePidMedium::Msisdn,
                _ => return None,
            };
            Some(ThreePid {
                medium,
                address: project_name(&three_pid.address),
            })
        })
        .take(MAX_THREE_PIDS)
        .collect();
    Ok(ThreePidList {
        three_pids,
        truncated,
        can_change: can_change(client).await,
        pending,
    })
}

/// An address waiting to be added: what the homeserver needs to add it, and
/// where a phone's code goes.
#[derive(Clone)]
pub struct Pending {
    pub shown: ThreePid,
    pub client_secret: OwnedClientSecret,
    pub sid: OwnedSessionId,
    /// The checked `submit_url` for a phone's code.
    pub submit_url: Option<url::Url>,
    /// The phone's code was accepted, so adding asks only for the password.
    pub code_confirmed: bool,
}

/// The code for a prompt that ended without an answer.
pub fn prompt_change(error: PromptError) -> AccountChange {
    match error {
        PromptError::Cancelled => AccountChange::Cancelled,
        PromptError::Unsupported => AccountChange::Unsupported,
        PromptError::Invalid | PromptError::Failed => AccountChange::Failed,
    }
}

/// Asks for the current password in the core's own window.
pub fn ask_current_password(
    source: &dyn CredentialSource,
    user_id: &str,
    host: &str,
    purpose: PasswordPurpose,
) -> Result<Zeroizing<String>, AccountChange> {
    source
        .ask_secret(SecretPrompt::CurrentPassword {
            user_id,
            host,
            purpose,
        })
        .map_err(prompt_change)
}

/// Asks for a new password twice in the core's own windows, until both
/// entries match.
pub fn ask_new_password(
    source: &dyn CredentialSource,
    user_id: &str,
    host: &str,
) -> Result<Zeroizing<String>, AccountChange> {
    let ask = |step| {
        source
            .ask_secret(SecretPrompt::ChangedPassword {
                user_id,
                host,
                step,
            })
            .map_err(prompt_change)
    };
    let mut step = NewPassword::Choose;
    loop {
        let first = ask(step)?;
        let second = ask(NewPassword::Confirm)?;
        if first == second {
            return Ok(first);
        }
        step = NewPassword::Mismatch;
    }
}

/// Asks for a phone's confirmation code in the core's own window.
pub fn ask_phone_code(
    source: &dyn CredentialSource,
    host: &str,
) -> Result<Zeroizing<String>, AccountChange> {
    source
        .ask_secret(SecretPrompt::PhoneCode { host })
        .map_err(prompt_change)
}

/// A new client secret: 128 random bits as hex, within the specification's
/// character set.
fn client_secret() -> Result<OwnedClientSecret, AccountChange> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| AccountChange::Failed)?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    OwnedClientSecret::try_from(hex).map_err(|_| AccountChange::Failed)
}

/// Asks the homeserver to send a confirmation link to `email`.
pub async fn request_email(client: &Client, email: &str) -> Result<Pending, AccountChange> {
    if !can_change(client).await {
        return Err(AccountChange::ChangesDisabled);
    }
    let secret = client_secret()?;
    let response = client
        .account()
        .request_3pid_email_token(&secret, email, UInt::from(1u32))
        .await
        .map_err(|error| sdk_error(&error))?;
    Ok(Pending {
        shown: ThreePid {
            medium: ThreePidMedium::Email,
            address: email.to_owned(),
        },
        client_secret: secret,
        sid: response.sid,
        submit_url: None,
        code_confirmed: false,
    })
}

/// `submit_url` when it is on the homeserver itself; otherwise `None`.
pub fn same_origin(submit_url: &str, homeserver: &url::Url) -> Option<url::Url> {
    let url = url::Url::parse(submit_url).ok()?;
    (url.scheme() == homeserver.scheme()
        && url.host() == homeserver.host()
        && url.port_or_known_default() == homeserver.port_or_known_default()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// Asks the homeserver to send a code to this phone number. Without a
/// `submit_url` on the homeserver, the code could be checked only by an
/// identity server: `medium_not_supported`.
pub async fn request_phone(
    client: &Client,
    country: &str,
    number: &str,
) -> Result<Pending, AccountChange> {
    if !can_change(client).await {
        return Err(AccountChange::ChangesDisabled);
    }
    let secret = client_secret()?;
    let response = client
        .account()
        .request_3pid_msisdn_token(&secret, country, number, UInt::from(1u32))
        .await
        .map_err(|error| sdk_error(&error))?;
    let submit_url = response
        .submit_url
        .as_deref()
        .and_then(|url| same_origin(url, &client.homeserver()))
        .ok_or(AccountChange::MediumNotSupported)?;
    Ok(Pending {
        shown: ThreePid {
            medium: ThreePidMedium::Msisdn,
            address: format!("{country} {number}"),
        },
        client_secret: secret,
        sid: response.sid,
        submit_url: Some(submit_url),
        code_confirmed: false,
    })
}

/// Sends a phone's code to the homeserver's `submit_url`. Only
/// `{"success": true}` confirms it.
pub async fn submit_code(
    client: &Client,
    pending: &Pending,
    code: &Zeroizing<String>,
) -> Result<(), AccountChange> {
    let Some(url) = &pending.submit_url else {
        return Err(AccountChange::MediumNotSupported);
    };
    let body = Zeroizing::new(
        serde_json::to_vec(&serde_json::json!({
            "sid": pending.sid.as_str(),
            "client_secret": pending.client_secret.as_str(),
            "token": code.as_str(),
        }))
        .map_err(|_| AccountChange::Failed)?,
    );
    let mut response = client
        .http_client()
        .post(url.clone())
        .header("content-type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .map_err(|_| AccountChange::Unreachable)?;
    let status = response.status();
    let mut reply = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AccountChange::Unreachable)?
    {
        if reply.len() + chunk.len() > MAX_SUBMIT_REPLY_BYTES {
            return Err(AccountChange::Failed);
        }
        reply.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(AccountChange::NotConfirmed);
    }
    let confirmed = serde_json::from_slice::<serde_json::Value>(&reply)
        .ok()
        .and_then(|value| value.get("success").and_then(serde_json::Value::as_bool))
        == Some(true);
    if confirmed {
        Ok(())
    } else {
        Err(AccountChange::NotConfirmed)
    }
}

/// Adds a confirmed address with the account password.
pub async fn add(
    client: &Client,
    user_id: &UserId,
    pending: &Pending,
    password: &Zeroizing<String>,
) -> AccountChange {
    let result = with_password(user_id, password, async |auth| {
        client
            .account()
            .add_3pid(&pending.client_secret, &pending.sid, auth)
            .await
    })
    .await;
    match result {
        Ok(_) => AccountChange::Done,
        Err(code) => code,
    }
}

/// Removes an address. No identity server is named: the homeserver unbinds
/// it only from one it was bound through, and Persephone binds none.
pub async fn remove(client: &Client, kind: ThreePidMedium, address: &str) -> AccountChange {
    match client
        .account()
        .delete_3pid(address, medium(kind), None)
        .await
    {
        Ok(_) => AccountChange::Done,
        Err(error) => sdk_error(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN email addresses WHEN checked THEN only printable ASCII with one
    /// `@` and a dotted domain passes, within the limit.
    #[test]
    fn given_email_addresses_then_only_plain_ones_pass() {
        for valid in ["a@example.org", "a.b+c@mail.example.co.uk"] {
            assert!(valid_email(valid), "{valid}");
        }
        let long = format!("{}@example.org", "a".repeat(MAX_EMAIL_BYTES));
        for invalid in [
            "",
            "a",
            "@example.org",
            "a@",
            "a@b",
            "a@.org",
            "a@example.",
            "a@b@example.org",
            "a b@example.org",
            "a@example.org\n",
            "caf\u{e9}@example.org",
            "a\u{202e}@example.org",
            long.as_str(),
        ] {
            assert!(!valid_email(invalid), "{invalid:?}");
        }
    }

    /// GIVEN phone numbers and country codes WHEN checked THEN only digits
    /// and two uppercase letters pass.
    #[test]
    fn given_phone_numbers_then_only_digits_and_a_country_pass() {
        assert!(valid_country("GB") && valid_country("US"));
        for invalid in ["", "G", "gb", "GBR", "4", "G\u{e9}"] {
            assert!(!valid_country(invalid), "{invalid:?}");
        }
        assert!(valid_phone_number("07700900123"));
        for invalid in [
            "",
            "123",
            "+447700900123",
            "07700 900123",
            "0770090012x",
            &"1".repeat(MAX_PHONE_NUMBER_BYTES + 1),
        ] {
            assert!(!valid_phone_number(invalid), "{invalid:?}");
        }
        assert!(valid_address(ThreePidMedium::Msisdn, "447700900123"));
        assert!(!valid_address(ThreePidMedium::Msisdn, "+44"));
        assert!(valid_address(ThreePidMedium::Email, "a@example.org"));
        assert!(!valid_address(ThreePidMedium::Email, ""));
    }

    /// GIVEN refusals WHEN reported THEN each Matrix error code has its own
    /// fixed code, an unknown endpoint is an unsupported medium, and any
    /// other is `failed`.
    #[test]
    fn given_refusals_then_each_has_a_fixed_code() {
        for (kind, expected) in [
            (ErrorKind::WeakPassword, AccountChange::WeakPassword),
            (ErrorKind::ThreepidAuthFailed, AccountChange::NotConfirmed),
            (ErrorKind::ThreepidInUse, AccountChange::InUse),
            (ErrorKind::ThreepidDenied, AccountChange::Denied),
            (
                ErrorKind::ThreepidMediumNotSupported,
                AccountChange::MediumNotSupported,
            ),
            (ErrorKind::Unrecognized, AccountChange::MediumNotSupported),
            (ErrorKind::Forbidden, AccountChange::ChangesDisabled),
            (ErrorKind::Unauthorized, AccountChange::Failed),
        ] {
            assert_eq!(refusal(&kind), expected, "{kind:?}");
        }
    }

    /// GIVEN authentication flows WHEN the next step is chosen THEN only a
    /// flow of the password step alone is followed, once; a second answer
    /// means the password was refused.
    #[test]
    fn given_auth_flows_then_only_the_password_flow_is_followed_once() {
        let info = |stages: &[AuthType]| UiaaInfo::new(vec![AuthFlow::new(stages.to_vec())]);
        assert_eq!(next_auth(&info(&[AuthType::Password]), false), Ok(()));
        assert_eq!(
            next_auth(&info(&[AuthType::Password]), true),
            Err(AccountChange::InvalidPassword)
        );
        for unsupported in [
            &[AuthType::EmailIdentity][..],
            &[AuthType::Password, AuthType::EmailIdentity],
            &[AuthType::Sso],
            &[],
        ] {
            assert_eq!(
                next_auth(&info(unsupported), false),
                Err(AccountChange::UnsupportedAuth),
                "{unsupported:?}"
            );
        }
    }

    /// GIVEN a `submit_url` WHEN checked THEN only one on the homeserver's
    /// own scheme, host and port, without credentials, is kept.
    #[test]
    fn given_submit_urls_then_only_the_homeservers_own_is_kept() {
        let homeserver = url::Url::parse("https://matrix.example.org").unwrap();
        assert!(
            same_origin(
                "https://matrix.example.org/_matrix/client/unstable/add_threepid/msisdn/submit_token",
                &homeserver
            )
            .is_some()
        );
        assert!(same_origin("https://matrix.example.org:443/x", &homeserver).is_some());
        for other in [
            "http://matrix.example.org/x",
            "https://matrix.example.org:8448/x",
            "https://identity.example.org/x",
            "https://matrix.example.org.evil.example/x",
            "https://user:pass@matrix.example.org/x",
            "not a url",
            "",
        ] {
            assert!(same_origin(other, &homeserver).is_none(), "{other}");
        }
    }

    /// A prompt source that answers from a list, recording each prompt.
    struct Answers {
        answers: std::sync::Mutex<Vec<Result<&'static str, PromptError>>>,
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl Answers {
        fn new(answers: Vec<Result<&'static str, PromptError>>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers),
                asked: std::sync::Mutex::default(),
            }
        }
    }

    impl CredentialSource for Answers {
        fn ask(&self, _: &str) -> Result<crate::credentials::Credentials, PromptError> {
            Err(PromptError::Unsupported)
        }

        fn ask_secret(&self, prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
            self.asked.lock().unwrap().push(format!("{prompt:?}"));
            self.answers
                .lock()
                .unwrap()
                .remove(0)
                .map(|answer| Zeroizing::new(answer.to_owned()))
        }
    }

    /// GIVEN two different new passwords and then two equal ones WHEN the
    /// new password is asked for THEN the core asks again after the
    /// mismatch and returns the matching one.
    #[test]
    fn given_a_mismatch_then_the_new_password_is_asked_again() {
        let source = Answers::new(vec![Ok("one"), Ok("two"), Ok("three"), Ok("three")]);
        let password = ask_new_password(&source, "@a:s", "s").unwrap();
        assert_eq!(password.as_str(), "three");
        let asked = source.asked.lock().unwrap();
        assert_eq!(asked.len(), 4);
        assert!(asked[2].contains("Mismatch"), "{}", asked[2]);
    }

    /// GIVEN a prompt that is cancelled, unsupported or fails WHEN asked
    /// THEN each ends with its fixed code and nothing more is asked.
    #[test]
    fn given_a_prompt_without_an_answer_then_the_change_stops_with_its_code() {
        for (error, expected) in [
            (PromptError::Cancelled, AccountChange::Cancelled),
            (PromptError::Unsupported, AccountChange::Unsupported),
            (PromptError::Invalid, AccountChange::Failed),
            (PromptError::Failed, AccountChange::Failed),
        ] {
            let source = Answers::new(vec![Err(error)]);
            assert_eq!(ask_new_password(&source, "@a:s", "s").err(), Some(expected));
            let source = Answers::new(vec![Err(error)]);
            assert_eq!(
                ask_current_password(&source, "@a:s", "s", PasswordPurpose::ChangePassword).err(),
                Some(expected)
            );
            let source = Answers::new(vec![Err(error)]);
            assert_eq!(ask_phone_code(&source, "s").err(), Some(expected));
        }
    }

    /// GIVEN client secrets WHEN made THEN each is new and within the
    /// specification's character set.
    #[test]
    fn given_new_client_secrets_then_each_is_distinct_hex() {
        let (a, b) = (client_secret().unwrap(), client_secret().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 32);
        assert!(a.as_str().bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
