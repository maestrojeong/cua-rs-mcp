//! The bearer token that guards Streamable HTTP mode.
//!
//! # Why loopback was not enough
//!
//! The HTTP transport used to rely on binding to `127.0.0.1` and nothing else.
//! That keeps the network out and does not keep anything else out: every
//! process on the machine can reach loopback, and so can any web page the user
//! has open, since a browser will happily `fetch("http://127.0.0.1:9331/mcp")`
//! on behalf of whatever site is loaded. The endpoint on the other side of that
//! request can read any window and press any button on the desktop. Loopback is
//! a *reachability* boundary; it was being used as an *authorization* boundary,
//! and those are not the same thing.
//!
//! # What this is, and what it is not
//!
//! One shared secret, compared in constant time, required on `/mcp` and not on
//! `/health`. It is not identity, it is not per-client, and it does not survive
//! a restart unless the operator supplies it. It is the smallest thing that
//! makes "can reach the port" stop meaning "may drive the desktop", which is
//! the actual gap. Anything larger — OAuth, per-client keys, a session store —
//! would be a different product decision, and the stdio transport that most
//! clients use needs none of it because the client already owns the process.
//!
//! `/health` stays open deliberately: it reports a name and a version, a
//! supervisor needs it before it has any credential, and refusing it would make
//! "the server is down" and "my token is wrong" the same observation.

use std::fmt::Write as _;

/// Where an operator supplies their own token.
pub const TOKEN_ENV: &str = "CUA_HTTP_TOKEN";

/// How the token was obtained, so startup can say the right thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// Read from [`TOKEN_ENV`].
    Environment,
    /// Generated for this run. Printed once, because nothing else can recover it.
    Generated,
}

/// The secret this server will accept, and where it came from.
#[derive(Debug, Clone)]
pub struct Token {
    value: String,
    pub source: TokenSource,
}

impl Token {
    /// Take the operator's token, or mint one.
    ///
    /// An empty or whitespace-only `CUA_HTTP_TOKEN` is treated as absent rather
    /// than as a token: an unset variable and a variable that expanded to
    /// nothing look identical in a shell script, and the second one silently
    /// accepting `Authorization: Bearer ` would be the worst possible reading.
    pub fn resolve() -> std::io::Result<Self> {
        match std::env::var(TOKEN_ENV) {
            Ok(v) if !v.trim().is_empty() => Ok(Self {
                value: v.trim().to_string(),
                source: TokenSource::Environment,
            }),
            _ => Ok(Self {
                value: generate()?,
                source: TokenSource::Generated,
            }),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether an `Authorization` header value authorizes this server.
    ///
    /// Case-insensitive on the scheme, because the RFC says the scheme is
    /// case-insensitive and clients disagree about it in practice. Exact on the
    /// token, apart from surrounding whitespace.
    pub fn authorizes(&self, header: Option<&str>) -> bool {
        let Some(header) = header else {
            return false;
        };
        let header = header.trim();
        let Some((scheme, presented)) = header.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            return false;
        }
        constant_time_eq(presented.trim().as_bytes(), self.value.as_bytes())
    }
}

/// Compare without leaking where the two differ.
///
/// A naive `==` returns as soon as it finds a differing byte, which over enough
/// requests to a loopback port — where the round trip is microseconds and an
/// attacker can send millions — is a measurable oracle for the prefix. The
/// length is not secret and is checked first.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 32 bytes from the kernel, hex encoded.
///
/// Read straight from `/dev/urandom` rather than pulling in a random-number
/// crate: this is the only place in the workspace that needs randomness, the
/// device is always present on macOS, and a dependency added for sixteen lines
/// is a dependency that has to be audited forever. A short read is an error,
/// never a shorter token — a weak secret that looks like a secret is worse than
/// a server that refuses to start.
fn generate() -> std::io::Result<String> {
    use std::io::Read;

    let mut bytes = [0u8; 32];
    let mut file = std::fs::File::open("/dev/urandom")?;
    file.read_exact(&mut bytes)?;

    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(value: &str) -> Token {
        Token {
            value: value.to_string(),
            source: TokenSource::Generated,
        }
    }

    #[test]
    fn the_right_token_authorizes() {
        let t = token("s3cret");
        assert!(t.authorizes(Some("Bearer s3cret")));
    }

    #[test]
    fn the_scheme_is_case_insensitive_and_the_token_is_not() {
        let t = token("s3cret");
        assert!(t.authorizes(Some("bearer s3cret")));
        assert!(t.authorizes(Some("BEARER s3cret")));
        assert!(!t.authorizes(Some("Bearer S3CRET")));
    }

    #[test]
    fn surrounding_whitespace_is_forgiven() {
        let t = token("s3cret");
        assert!(t.authorizes(Some("  Bearer   s3cret  ")));
    }

    #[test]
    fn everything_else_is_refused() {
        let t = token("s3cret");
        assert!(!t.authorizes(None));
        assert!(!t.authorizes(Some("")));
        assert!(
            !t.authorizes(Some("s3cret")),
            "a bare token is not a scheme"
        );
        assert!(!t.authorizes(Some("Basic s3cret")));
        assert!(!t.authorizes(Some("Bearer")));
        assert!(!t.authorizes(Some("Bearer ")));
        assert!(!t.authorizes(Some("Bearer wrong")));
    }

    #[test]
    fn a_prefix_of_the_token_is_not_the_token() {
        let t = token("s3cret");
        assert!(!t.authorizes(Some("Bearer s3cre")));
        assert!(!t.authorizes(Some("Bearer s3crets")));
    }

    #[test]
    fn constant_time_comparison_still_answers_correctly() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
    }

    #[test]
    fn a_generated_token_is_long_random_hex() {
        let a = generate().expect("/dev/urandom");
        let b = generate().expect("/dev/urandom");
        assert_eq!(a.len(), 64, "32 bytes, hex encoded");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two runs must not share a secret");
    }

    #[test]
    fn an_empty_environment_variable_does_not_become_an_empty_token() {
        // Cannot set the variable here without racing other tests in this
        // binary, so the rule itself is asserted: a token is never empty, and
        // an empty presented value never authorizes.
        let t = token("");
        assert!(!t.authorizes(Some("Bearer ")));
        assert!(!t.authorizes(Some("Bearer")));
    }
}
