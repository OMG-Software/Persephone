//! Connections to call members on other SFUs (ADR-0054).
//!
//! The own SFU stays as ADR-0043 made it. A member whose first acceptable
//! transport is another SFU can be heard and seen over a subscribe-only
//! connection of the worker, after the user agreed to contact that server.
//! This module holds the pure rules: which SFU a member is on, which SFUs a
//! call connects to, and what the user has been asked. It does no I/O.

use super::membership::ActiveMember;
use std::collections::HashMap;
use url::{Host, Url};

/// Most other SFUs for one call. Members on further SFUs stay "not
/// reachable".
pub const MAX_OTHER_SFUS: usize = 3;

/// Where a member is heard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Home<'a> {
    /// On the own SFU.
    Own,
    /// On this other SFU.
    Other(&'a Url),
    /// Nowhere this client can reach.
    Nowhere,
}

/// Whether the host of `url` may be contacted as another SFU (ADR-0054). It
/// must be a DNS name that is not local by its form: not an IP address
/// literal (IPv4 or IPv6), not `localhost` or `*.localhost` (RFC 6761), not a
/// single-label name, and not `*.local` (mDNS), `*.home.arpa` (RFC 8375) or
/// `*.internal`. Case and one trailing dot do not matter. This is the rule
/// for the service URL and for the `wss://` URL in the answer.
pub fn is_other_host(url: &Url) -> bool {
    let Some(Host::Domain(domain)) = url.host() else {
        return false;
    };
    let name = domain.to_ascii_lowercase();
    let name = name.strip_suffix('.').unwrap_or(&name);
    let local = |suffix: &str| name.ends_with(suffix);
    name.contains('.')
        && !name.is_empty()
        && name != "localhost"
        && !local(".localhost")
        && !local(".local")
        && !local(".home.arpa")
        && !local(".internal")
}

/// The SFU of `member` (ADR-0054).
///
/// A member whose first acceptable transport is the own SFU is on the own
/// SFU only, so it is never taken from another. Otherwise its other SFU is
/// the first acceptable transport that differs from the own one (as the
/// parsed URL string), whose host is a DNS name and that is a JWT service
/// (`https`). An SFU that only a homeserver issues tokens for (`wss`,
/// ADR-0056) is never another SFU: this core cannot get a token for it.
/// A member whose first transport is such an SFU publishes there, so it is
/// nowhere, not on a later JWT service.
pub fn home<'a>(member: &'a ActiveMember, own: &Url) -> Home<'a> {
    let first = member.transports.first();
    if first.is_some_and(|first| first.as_str() == own.as_str()) {
        return Home::Own;
    }
    if first.is_some_and(|first| first.scheme() != "https") {
        return Home::Nowhere;
    }
    member
        .transports
        .iter()
        .find(|transport| {
            transport.as_str() != own.as_str()
                && transport.scheme() == "https"
                && is_other_host(transport)
        })
        .map_or(Home::Nowhere, Home::Other)
}

/// The host name the user is asked about, from a URL with a DNS host.
pub fn host_of(url: &Url) -> Option<String> {
    match url.host() {
        Some(Host::Domain(domain)) => Some(domain.to_ascii_lowercase()),
        _ => None,
    }
}

/// What a connection to one other SFU is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// Its host has not been answered yet.
    NeedsConsent,
    /// The token request or the worker's connect is running.
    Connecting,
    Connected,
    /// Declined, failed or ended. It stays so for the rest of the call.
    Unreachable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Answer {
    Asking,
    Yes,
    No,
}

#[derive(Clone, Debug)]
struct Slot {
    url: Url,
    host: String,
    state: State,
}

/// What to do now.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Plan {
    /// Connections (1 to 3) to start: request a token, then `Subscribe`.
    pub connect: Vec<u8>,
    /// A host to ask the user about. At most one question is open at a time.
    pub ask: Option<String>,
}

/// The other SFUs of one call. Nothing is kept after the call.
#[derive(Debug, Default)]
pub struct OtherSfus {
    /// Connection `n` is `slots[n - 1]`. A slot is never reused in a call.
    slots: Vec<Slot>,
    /// The answer for each host, asked once in a call.
    hosts: HashMap<String, Answer>,
}

impl OtherSfus {
    /// Adds the other SFUs of `members` that have no connection yet, in the
    /// members' order, up to [`MAX_OTHER_SFUS`] in all. Returns whether one
    /// was added.
    pub fn track(&mut self, members: &[ActiveMember], own: &Url) -> bool {
        let mut added = false;
        for member in members {
            if self.slots.len() >= MAX_OTHER_SFUS {
                break;
            }
            let Home::Other(url) = home(member, own) else {
                continue;
            };
            if self
                .slots
                .iter()
                .any(|slot| slot.url.as_str() == url.as_str())
            {
                continue;
            }
            let Some(host) = host_of(url) else {
                continue;
            };
            self.slots.push(Slot {
                url: url.clone(),
                host,
                state: State::NeedsConsent,
            });
            added = true;
        }
        added
    }

    /// Decides what happens next for the slots that wait for consent. A host
    /// that was answered "yes" connects, one answered "no" is unreachable, and
    /// a new host is asked, one at a time.
    pub fn plan(&mut self) -> Plan {
        let mut plan = Plan::default();
        let open = self.hosts.values().any(|answer| *answer == Answer::Asking);
        for (position, slot) in self.slots.iter_mut().enumerate() {
            if slot.state != State::NeedsConsent {
                continue;
            }
            match self.hosts.get(&slot.host) {
                Some(Answer::Yes) => {
                    slot.state = State::Connecting;
                    plan.connect.push(position as u8 + 1);
                }
                Some(Answer::No) => slot.state = State::Unreachable,
                Some(Answer::Asking) => {}
                None if !open && plan.ask.is_none() => {
                    self.hosts.insert(slot.host.clone(), Answer::Asking);
                    plan.ask = Some(slot.host.clone());
                }
                None => {}
            }
        }
        plan
    }

    /// The user answered the question about `host`. Returns what to do now.
    pub fn answer(&mut self, host: &str, connect: bool) -> Plan {
        if self.hosts.get(host) == Some(&Answer::Asking) {
            self.hosts.insert(
                host.to_owned(),
                if connect { Answer::Yes } else { Answer::No },
            );
        }
        self.plan()
    }

    /// The worker reported connection `index` up or down. Returns whether the
    /// state changed. An unknown or not connecting slot ignores `up`.
    pub fn worker_connection(&mut self, index: u8, up: bool) -> bool {
        let Some(slot) = usize::from(index)
            .checked_sub(1)
            .and_then(|position| self.slots.get_mut(position))
        else {
            return false;
        };
        let next = match (slot.state, up) {
            (State::Connecting, true) => State::Connected,
            (State::Connecting | State::Connected, false) => State::Unreachable,
            _ => return false,
        };
        slot.state = next;
        true
    }

    /// The token request of connection `index` failed. Returns whether the
    /// state changed.
    pub fn token_failed(&mut self, index: u8) -> bool {
        let Some(slot) = usize::from(index)
            .checked_sub(1)
            .and_then(|position| self.slots.get_mut(position))
        else {
            return false;
        };
        if slot.state == State::Connecting {
            slot.state = State::Unreachable;
            true
        } else {
            false
        }
    }

    /// The service URL of connection `index`, while it is connecting.
    pub fn connecting_url(&self, index: u8) -> Option<&Url> {
        let slot = self.slots.get(usize::from(index).checked_sub(1)?)?;
        (slot.state == State::Connecting).then_some(&slot.url)
    }

    /// The connection number (1 to 3) of this SFU URL.
    pub fn index_of(&self, url: &Url) -> Option<u8> {
        self.slots
            .iter()
            .position(|slot| slot.url.as_str() == url.as_str())
            .map(|position| position as u8 + 1)
    }

    /// Whether the worker is connected to this SFU.
    pub fn is_connected(&self, url: &Url) -> bool {
        self.index_of(url)
            .and_then(|index| self.slots.get(usize::from(index) - 1))
            .is_some_and(|slot| slot.state == State::Connected)
    }

    /// The state of connection `index`.
    #[cfg(test)]
    pub fn state(&self, index: u8) -> Option<State> {
        self.slots
            .get(usize::from(index).checked_sub(1)?)
            .map(|slot| slot.state)
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{OwnedDeviceId, OwnedUserId};

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn own() -> Url {
        url("https://sfu.own.example/jwt")
    }

    fn member(name: &str, transports: &[&str]) -> ActiveMember {
        let transports: Vec<Url> = transports.iter().map(|t| url(t)).collect();
        ActiveMember {
            user_id: OwnedUserId::try_from(format!("@{name}:example.org")).unwrap(),
            device_id: OwnedDeviceId::from("D"),
            identity: format!("@{name}:example.org:D"),
            backend_identities: vec![format!("@{name}:example.org:D")],
            service_url: transports[0].clone(),
            transports,
            joined_ts: 0,
            expires_ts: u64::MAX,
        }
    }

    /// GIVEN a member whose first transport is the own SFU
    /// WHEN its home is asked
    /// THEN it is the own SFU, also when a later transport differs.
    #[test]
    fn given_the_own_sfu_first_then_the_member_is_on_the_own_sfu_only() {
        let both = member(
            "a",
            &["https://sfu.own.example/jwt", "https://sfu.b.example"],
        );
        assert_eq!(home(&both, &own()), Home::Own);
        let only = member("a", &["https://sfu.own.example/jwt"]);
        assert_eq!(home(&only, &own()), Home::Own);
    }

    /// GIVEN a member whose first transport differs from the own one
    /// WHEN its home is asked
    /// THEN it is that transport; a string that differs only in the path is
    /// another SFU, as the ADR compares strings.
    #[test]
    fn given_a_different_service_url_string_then_it_is_another_sfu() {
        let other = member(
            "a",
            &["https://sfu.b.example/jwt", "https://sfu.own.example/jwt"],
        );
        assert_eq!(
            home(&other, &own()),
            Home::Other(&url("https://sfu.b.example/jwt"))
        );
        let path = member("a", &["https://sfu.own.example/other"]);
        assert_eq!(
            home(&path, &own()),
            Home::Other(&url("https://sfu.own.example/other"))
        );
    }

    /// GIVEN a member on an SFU that its homeserver issues tokens for (`wss`)
    /// WHEN its home is asked
    /// THEN it is the own SFU when the own SFU is that one, else it is never
    /// another SFU and is nowhere, also with a later JWT service, because it
    /// publishes on its first transport (ADR-0056).
    #[test]
    fn given_a_homeserver_flow_sfu_then_it_is_never_another_sfu() {
        let own_wss = url("wss://sfu.own.example:7443");
        let same = member("a", &["wss://sfu.own.example:7443"]);
        assert_eq!(home(&same, &own_wss), Home::Own);
        let elsewhere = member("a", &["wss://sfu.b.example"]);
        assert_eq!(home(&elsewhere, &own()), Home::Nowhere);
        assert_eq!(home(&elsewhere, &own_wss), Home::Nowhere);
        let then_service = member("a", &["wss://sfu.b.example", "https://sfu.c.example"]);
        assert_eq!(home(&then_service, &own_wss), Home::Nowhere);
        assert_eq!(home(&then_service, &own()), Home::Nowhere);
    }

    /// GIVEN transports with IPv4 or IPv6 literal hosts
    /// WHEN the home is asked
    /// THEN the literal is skipped for the next DNS name, or the member is
    /// nowhere when there is none.
    #[test]
    fn given_ip_literal_hosts_then_they_are_never_another_sfu() {
        for literal in [
            "https://192.0.2.10/jwt",
            "https://[2001:db8::1]/jwt",
            "https://127.0.0.1:8443",
            "https://0x7f.1/jwt",
            "https://[::1]",
        ] {
            let only = member("a", &[literal]);
            assert_eq!(home(&only, &own()), Home::Nowhere, "{literal}");
            let then_name = member("a", &[literal, "https://sfu.b.example"]);
            assert_eq!(
                home(&then_name, &own()),
                Home::Other(&url("https://sfu.b.example")),
                "{literal}"
            );
        }
    }

    /// GIVEN a service host that is local by its form
    /// WHEN the home is asked
    /// THEN the member is nowhere, in any case and with a trailing dot, while a
    /// normal name is accepted.
    #[test]
    fn given_local_host_names_then_they_are_never_another_sfu() {
        for host in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "sfu.localhost",
            "Sfu.LocalHost.",
            "sfu",
            "sfu.",
            "printer.local",
            "printer.LOCAL.",
            "nas.home.arpa",
            "nas.Home.Arpa.",
            "sfu.internal",
            "sfu.INTERNAL.",
            "a.b.internal",
        ] {
            let url = Url::parse(&format!("https://{host}/jwt")).unwrap();
            assert!(!is_other_host(&url), "{host}");
            let only = member("a", &[url.as_str()]);
            assert_eq!(home(&only, &own()), Home::Nowhere, "{host}");
        }
        for host in [
            "sfu.example.org",
            "SFU.Example.ORG.",
            "local.example.org",
            "internal.example",
        ] {
            let url = Url::parse(&format!("https://{host}/jwt")).unwrap();
            assert!(is_other_host(&url), "{host}");
        }
        for literal in ["https://192.0.2.10", "https://[2001:db8::1]"] {
            assert!(!is_other_host(&Url::parse(literal).unwrap()), "{literal}");
        }
    }

    /// GIVEN more members on other SFUs than the limit
    /// WHEN the SFUs are tracked
    /// THEN the first three in the members' order get a connection, and a
    /// later call with the same members adds nothing.
    #[test]
    fn given_four_other_sfus_then_the_first_three_in_member_order_are_used() {
        let members = vec![
            member("a", &["https://sfu.a.example"]),
            member("o", &["https://sfu.own.example/jwt"]),
            member("b", &["https://sfu.b.example"]),
            member("a2", &["https://sfu.a.example"]),
            member("c", &["https://sfu.c.example"]),
            member("d", &["https://sfu.d.example"]),
        ];
        let mut sfus = OtherSfus::default();
        assert!(sfus.track(&members, &own()));
        assert_eq!(sfus.count(), 3);
        assert_eq!(sfus.index_of(&url("https://sfu.a.example")), Some(1));
        assert_eq!(sfus.index_of(&url("https://sfu.b.example")), Some(2));
        assert_eq!(sfus.index_of(&url("https://sfu.c.example")), Some(3));
        assert_eq!(sfus.index_of(&url("https://sfu.d.example")), None);
        assert!(!sfus.track(&members, &own()));
        assert_eq!(sfus.index_of(&url("https://sfu.own.example/jwt")), None);
    }

    /// GIVEN a member change that brings a new SFU after the first plan
    /// WHEN it is tracked THEN it gets the next free index, and the older
    /// ones keep theirs.
    #[test]
    fn given_a_new_sfu_later_then_it_gets_the_next_index() {
        let mut sfus = OtherSfus::default();
        sfus.track(&[member("a", &["https://sfu.a.example"])], &own());
        assert!(sfus.track(
            &[
                member("b", &["https://sfu.b.example"]),
                member("a", &["https://sfu.a.example"])
            ],
            &own()
        ));
        assert_eq!(sfus.index_of(&url("https://sfu.a.example")), Some(1));
        assert_eq!(sfus.index_of(&url("https://sfu.b.example")), Some(2));
    }

    /// GIVEN two SFUs on one host and one on another
    /// WHEN the plan runs THEN the first host is asked once; WHEN it is
    /// answered "yes" THEN both of its SFUs connect and the next host is
    /// asked; WHEN that is "no" THEN it is unreachable and nothing connects.
    #[test]
    fn given_hosts_then_each_is_asked_once_and_the_answer_decides() {
        let mut sfus = OtherSfus::default();
        sfus.track(
            &[
                member("a", &["https://sfu.a.example/one"]),
                member("b", &["https://sfu.b.example"]),
                member("c", &["https://sfu.a.example/two"]),
            ],
            &own(),
        );
        let first = sfus.plan();
        assert_eq!(first.ask.as_deref(), Some("sfu.a.example"));
        assert!(first.connect.is_empty());
        // A repeat while the window is open asks nothing more.
        assert_eq!(sfus.plan(), Plan::default());
        let yes = sfus.answer("sfu.a.example", true);
        assert_eq!(yes.connect, vec![1, 3]);
        assert_eq!(yes.ask.as_deref(), Some("sfu.b.example"));
        assert_eq!(sfus.state(1), Some(State::Connecting));
        assert_eq!(sfus.state(3), Some(State::Connecting));
        let no = sfus.answer("sfu.b.example", false);
        assert_eq!(no, Plan::default());
        assert_eq!(sfus.state(2), Some(State::Unreachable));
        // The answer is kept for the call: a later member on a declined host
        // is not asked again.
        sfus.track(&[member("d", &["https://sfu.b.example/more"])], &own());
        assert_eq!(sfus.count(), 3);
    }

    /// GIVEN a host the user declined
    /// WHEN another SFU on the same host appears
    /// THEN nobody is asked again and it is unreachable.
    #[test]
    fn given_a_declined_host_then_a_second_sfu_on_it_is_not_asked() {
        let mut sfus = OtherSfus::default();
        sfus.track(&[member("a", &["https://sfu.a.example/one"])], &own());
        assert_eq!(sfus.plan().ask.as_deref(), Some("sfu.a.example"));
        assert_eq!(sfus.answer("sfu.a.example", false), Plan::default());
        sfus.track(&[member("b", &["https://sfu.a.example/two"])], &own());
        assert_eq!(sfus.plan(), Plan::default());
        assert_eq!(sfus.state(2), Some(State::Unreachable));
        assert!(sfus.connecting_url(2).is_none());
    }

    /// GIVEN an answer for a host nobody asked about
    /// WHEN it arrives THEN it changes nothing.
    #[test]
    fn given_an_unasked_answer_then_nothing_connects() {
        let mut sfus = OtherSfus::default();
        sfus.track(&[member("a", &["https://sfu.a.example"])], &own());
        assert_eq!(
            sfus.answer("sfu.a.example", true),
            Plan {
                connect: Vec::new(),
                ask: Some("sfu.a.example".to_owned())
            }
        );
        assert_eq!(sfus.state(1), Some(State::NeedsConsent));
    }

    /// GIVEN a connecting SFU
    /// WHEN the worker says up, then down THEN it is reachable, then not, and
    /// stays not reachable; WHEN a token fails THEN it is not reachable.
    #[test]
    fn given_worker_reports_then_the_state_follows_and_never_reopens() {
        let mut sfus = OtherSfus::default();
        sfus.track(
            &[
                member("a", &["https://sfu.a.example"]),
                member("b", &["https://sfu.a.example/two"]),
            ],
            &own(),
        );
        sfus.plan();
        let plan = sfus.answer("sfu.a.example", true);
        assert_eq!(plan.connect, vec![1, 2]);
        assert!(!sfus.is_connected(&url("https://sfu.a.example")));
        assert!(sfus.worker_connection(1, true));
        assert!(sfus.is_connected(&url("https://sfu.a.example")));
        assert!(!sfus.worker_connection(1, true));
        assert!(sfus.worker_connection(1, false));
        assert!(!sfus.is_connected(&url("https://sfu.a.example")));
        assert!(!sfus.worker_connection(1, true), "never reopened");
        assert!(sfus.token_failed(2));
        assert_eq!(sfus.state(2), Some(State::Unreachable));
        assert!(!sfus.token_failed(2));
        assert!(!sfus.worker_connection(0, true));
        assert!(!sfus.worker_connection(9, true));
    }
}
