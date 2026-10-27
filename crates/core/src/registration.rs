//! Registering a new account (ADR-0018).
//!
//! * The page sends only the homeserver and the desired username, a Matrix
//!   localpart checked against the specification's character set. The core
//!   asks the homeserver whether the name is free before it asks for
//!   anything else, so a taken name is reported without a password prompt.
//! * The password (twice) and, when the homeserver needs one, the
//!   registration token are entered in the core's own windows, as for
//!   sign-in; they never cross to the UI.
//! * Registration uses the user-interactive authentication flows the
//!   homeserver offers. Only flows made of a registration token and the
//!   "dummy" step are followed; a flow needing an email address, a captcha
//!   or accepting terms is `unsupported_registration`, and the user can
//!   register in the homeserver's own web page instead.
//! * A refusal is reported by its Matrix error code, as a fixed code: the
//!   name taken, not valid or reserved, registration closed, the token not
//!   accepted, the password too weak, or too many attempts. Nothing is
//!   retried.
//! * The new session is saved encrypted and started like a sign-in.

use crate::account::{DEVICE_NAME, save_session};
use crate::homeserver::Homeserver;
use crate::store::Stores;
use matrix_sdk::{
    Client,
    ruma::api::{
        client::{
            account::{get_username_availability, register},
            uiaa::{AuthData, AuthFlow, AuthType, Dummy, RegistrationToken, UiaaInfo},
        },
        error::ErrorKind,
    },
};
use persephone_contracts::SignInError;
use persephone_core_protocol::MAX_USERNAME_BYTES;

/// Most requests one registration makes before giving up: the first, one
/// per step, and slack for a homeserver that repeats a step.
const MAX_REQUESTS: usize = 4;

/// A username the page sent: a non-empty localpart made of the characters the
/// Matrix specification allows in new user IDs (lowercase letters, digits and
/// `._=-/+`), within the limit. So it is also safe to show in a native window.
pub fn valid_username(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= MAX_USERNAME_BYTES
        && username
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'=' | b'-' | b'/' | b'+'))
}

/// The registration error for a refusal with this Matrix error code.
fn refusal(kind: &ErrorKind) -> SignInError {
    match kind {
        ErrorKind::UserInUse => SignInError::UsernameTaken,
        ErrorKind::InvalidUsername => SignInError::InvalidUsername,
        ErrorKind::Exclusive => SignInError::UsernameReserved,
        ErrorKind::WeakPassword => SignInError::WeakPassword,
        ErrorKind::Forbidden => SignInError::RegistrationDisabled,
        ErrorKind::LimitExceeded(_) => SignInError::RateLimited,
        _ => SignInError::Failed,
    }
}

fn http_error(error: &matrix_sdk::HttpError) -> SignInError {
    match error.client_api_error_kind() {
        Some(kind) => refusal(kind),
        None if matches!(error, matrix_sdk::HttpError::Reqwest(_)) => SignInError::Unreachable,
        None => SignInError::Failed,
    }
}

/// Whether `username` is free on the homeserver; a refusal says why not.
pub async fn check_username(client: &Client, username: &str) -> Result<(), SignInError> {
    let request = get_username_availability::v3::Request::new(username.to_owned());
    match client.send(request).await {
        Ok(response) if response.available => Ok(()),
        Ok(_) => Err(SignInError::UsernameTaken),
        Err(error) => Err(http_error(&error)),
    }
}

/// The steps this client can take.
fn supported(stage: &AuthType) -> bool {
    matches!(stage, AuthType::Dummy | AuthType::RegistrationToken)
}

/// The flow to follow: the shortest one made only of supported steps, or
/// none.
fn choose_flow(flows: &[AuthFlow]) -> Option<&AuthFlow> {
    flows
        .iter()
        .filter(|flow| flow.stages.iter().all(supported))
        .min_by_key(|flow| flow.stages.len())
}

/// What a registration needs next, from the homeserver's answer.
#[derive(Debug, PartialEq)]
enum Next {
    Token,
    Dummy,
}

/// The next step of the chosen flow. A token just sent and still not
/// completed was not accepted.
fn next_step(info: &UiaaInfo, sent_token: bool) -> Result<Next, SignInError> {
    let flow = choose_flow(&info.flows).ok_or(SignInError::UnsupportedRegistration)?;
    match flow
        .stages
        .iter()
        .find(|stage| !info.completed.contains(stage))
    {
        Some(AuthType::RegistrationToken) if sent_token => Err(SignInError::TokenRejected),
        Some(AuthType::RegistrationToken) => Ok(Next::Token),
        Some(AuthType::Dummy) => Ok(Next::Dummy),
        // Every step done, yet refused; or a step `choose_flow` excludes.
        _ => Err(SignInError::Failed),
    }
}

/// Where a registration attempt ended.
#[derive(Debug)]
pub enum Registered {
    /// The account exists and its session is saved; the user ID.
    Done(String),
    /// The homeserver asks for a registration token; continue with it and
    /// this session.
    NeedsToken(Option<String>),
}

/// Registers `username` with `password`, following the homeserver's flow
/// until it needs a token this call does not have. `token` continues a
/// registration that asked for one, with its session.
pub async fn register(
    client: &Client,
    homeserver: &Homeserver,
    stores: &Stores,
    username: &str,
    password: &str,
    token: Option<(&str, Option<String>)>,
) -> Result<Registered, SignInError> {
    let mut sent_token = token.is_some();
    let mut auth = token.map(|(token, session)| {
        let mut stage = RegistrationToken::new(token.to_owned());
        stage.session = session;
        AuthData::RegistrationToken(stage)
    });
    for _ in 0..MAX_REQUESTS {
        let mut request = register::v3::Request::new();
        request.username = Some(username.to_owned());
        request.password = Some(password.to_owned());
        request.initial_device_display_name = Some(DEVICE_NAME.to_owned());
        request.auth = auth.take();
        let error = match client.matrix_auth().register(request).await {
            Ok(_) => {
                return save_session(client, homeserver, stores)
                    .await
                    .map(Registered::Done);
            }
            Err(error) => error,
        };
        let Some(info) = error.as_uiaa_response() else {
            return Err(match &error {
                matrix_sdk::Error::Http(http) => http_error(http),
                _ => SignInError::Failed,
            });
        };
        match next_step(info, sent_token)? {
            Next::Token => return Ok(Registered::NeedsToken(info.session.clone())),
            Next::Dummy => {
                sent_token = false;
                let mut stage = Dummy::new();
                stage.session = info.session.clone();
                auth = Some(AuthData::Dummy(stage));
            }
        }
    }
    Err(SignInError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(stages: &[AuthType]) -> AuthFlow {
        AuthFlow::new(stages.to_vec())
    }

    fn info(flows: Vec<AuthFlow>, completed: Vec<AuthType>) -> UiaaInfo {
        let mut info = UiaaInfo::new(flows);
        info.completed = completed;
        info.session = Some("s".into());
        info
    }

    /// GIVEN usernames WHEN checked THEN only lowercase localparts of the
    /// allowed characters pass, within the limit.
    #[test]
    fn given_usernames_then_only_valid_localparts_pass() {
        for valid in ["al", "a.b_c=d-e/f+g", "0", &"a".repeat(MAX_USERNAME_BYTES)] {
            assert!(valid_username(valid), "{valid}");
        }
        for invalid in [
            "",
            "Al",
            "@al:example.org",
            "al:example.org",
            "a l",
            "al\n",
            "caf\u{e9}",
            "a\u{202e}b",
            &"a".repeat(MAX_USERNAME_BYTES + 1),
        ] {
            assert!(!valid_username(invalid), "{invalid:?}");
        }
    }

    /// GIVEN flows a homeserver offers WHEN one is chosen THEN the shortest
    /// supported one wins, and none is chosen when every flow needs a step
    /// this client cannot take.
    #[test]
    fn given_flows_then_the_shortest_supported_one_is_chosen() {
        let flows = [
            flow(&[AuthType::EmailIdentity, AuthType::Dummy]),
            flow(&[AuthType::RegistrationToken, AuthType::Dummy]),
            flow(&[AuthType::Dummy]),
        ];
        let stages = |flows: &[AuthFlow]| choose_flow(flows).map(|flow| flow.stages.clone());
        assert_eq!(stages(&flows), Some(vec![AuthType::Dummy]));
        assert_eq!(
            stages(&flows[..2]),
            Some(vec![AuthType::RegistrationToken, AuthType::Dummy])
        );
        let unsupported = [
            flow(&[AuthType::ReCaptcha]),
            flow(&[AuthType::Terms, AuthType::Dummy]),
            flow(&[AuthType::EmailIdentity]),
        ];
        assert!(choose_flow(&unsupported).is_none());
        assert!(choose_flow(&[]).is_none());
    }

    /// GIVEN the homeserver's answer WHEN the next step is chosen THEN a
    /// token is asked for, a dummy step follows a token, a token sent and
    /// not completed is rejected, and unsupported flows are reported.
    #[test]
    fn given_answers_then_the_next_step_follows_the_flow() {
        let token_flow = vec![flow(&[AuthType::RegistrationToken, AuthType::Dummy])];
        assert_eq!(
            next_step(&info(token_flow.clone(), vec![]), false),
            Ok(Next::Token)
        );
        assert_eq!(
            next_step(&info(token_flow.clone(), vec![]), true),
            Err(SignInError::TokenRejected)
        );
        assert_eq!(
            next_step(
                &info(token_flow.clone(), vec![AuthType::RegistrationToken]),
                true
            ),
            Ok(Next::Dummy)
        );
        assert_eq!(
            next_step(
                &info(
                    token_flow,
                    vec![AuthType::RegistrationToken, AuthType::Dummy]
                ),
                false
            ),
            Err(SignInError::Failed)
        );
        assert_eq!(
            next_step(&info(vec![flow(&[AuthType::Dummy])], vec![]), false),
            Ok(Next::Dummy)
        );
        assert_eq!(
            next_step(&info(vec![flow(&[AuthType::ReCaptcha])], vec![]), false),
            Err(SignInError::UnsupportedRegistration)
        );
    }

    /// GIVEN refusals WHEN reported THEN each Matrix error code has its own
    /// fixed registration code, and any other is `failed`.
    #[test]
    fn given_refusals_then_each_has_a_fixed_code() {
        for (kind, expected) in [
            (ErrorKind::UserInUse, SignInError::UsernameTaken),
            (ErrorKind::InvalidUsername, SignInError::InvalidUsername),
            (ErrorKind::Exclusive, SignInError::UsernameReserved),
            (ErrorKind::WeakPassword, SignInError::WeakPassword),
            (ErrorKind::Forbidden, SignInError::RegistrationDisabled),
            (ErrorKind::Unauthorized, SignInError::Failed),
        ] {
            assert_eq!(refusal(&kind), expected, "{kind:?}");
        }
    }
}
