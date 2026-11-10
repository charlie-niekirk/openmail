//! Where refresh tokens are kept between runs.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use super::AuthError;

/// Stores one refresh token per account, keyed by the account's email address.
///
/// Calls may block (the OS keychain can show a prompt), so async callers should
/// run them on a blocking thread.
pub trait TokenStore: Send + Sync {
    /// The refresh token stored for `account`, if any.
    ///
    /// # Errors
    ///
    /// [`AuthError::TokenStore`] if the store cannot be read.
    fn load(&self, account: &str) -> Result<Option<String>, AuthError>;

    /// Stores `refresh_token` for `account`, replacing any previous one.
    ///
    /// # Errors
    ///
    /// [`AuthError::TokenStore`] if the store cannot be written.
    fn save(&self, account: &str, refresh_token: &str) -> Result<(), AuthError>;

    /// Removes the refresh token for `account`. Removing a missing one succeeds.
    ///
    /// # Errors
    ///
    /// [`AuthError::TokenStore`] if the store cannot be written.
    fn delete(&self, account: &str) -> Result<(), AuthError>;
}

/// Keeps refresh tokens in the OS credential store: the macOS Keychain, the
/// Windows Credential Manager, or the Secret Service on Linux.
#[derive(Debug, Clone)]
pub struct KeyringTokenStore {
    service: String,
}

impl KeyringTokenStore {
    /// The keychain service name entries are stored under by default.
    pub const DEFAULT_SERVICE: &str = "openmail-google-oauth";

    /// A store using [`DEFAULT_SERVICE`](Self::DEFAULT_SERVICE).
    pub fn new() -> Self {
        Self::with_service(Self::DEFAULT_SERVICE)
    }

    /// A store using another service name, for example to keep a development
    /// build's tokens apart.
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(&self.service, account).map_err(keyring_error)
    }
}

impl Default for KeyringTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenStore for KeyringTokenStore {
    fn load(&self, account: &str) -> Result<Option<String>, AuthError> {
        match self.entry(account)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keyring_error(error)),
        }
    }

    fn save(&self, account: &str, refresh_token: &str) -> Result<(), AuthError> {
        self.entry(account)?
            .set_password(refresh_token)
            .map_err(keyring_error)
    }

    fn delete(&self, account: &str) -> Result<(), AuthError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keyring_error(error)),
        }
    }
}

fn keyring_error(error: keyring::Error) -> AuthError {
    AuthError::TokenStore(Box::new(error))
}

/// Keeps refresh tokens in memory, for tests and for running without a keychain.
#[derive(Default)]
pub struct MemoryTokenStore {
    tokens: Mutex<HashMap<String, String>>,
}

impl MemoryTokenStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl fmt::Debug for MemoryTokenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("MemoryTokenStore")
            .field("accounts", &tokens.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self, account: &str) -> Result<Option<String>, AuthError> {
        let tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(tokens.get(account).cloned())
    }

    fn save(&self, account: &str, refresh_token: &str) -> Result<(), AuthError> {
        let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        tokens.insert(account.to_owned(), refresh_token.to_owned());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), AuthError> {
        let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        tokens.remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips_per_account() {
        let store = MemoryTokenStore::new();
        assert_eq!(store.load("a@example.com").unwrap(), None);

        store.save("a@example.com", "token-a").unwrap();
        store.save("b@example.com", "token-b").unwrap();
        store.save("a@example.com", "token-a2").unwrap();
        assert_eq!(
            store.load("a@example.com").unwrap().as_deref(),
            Some("token-a2")
        );
        assert_eq!(
            store.load("b@example.com").unwrap().as_deref(),
            Some("token-b")
        );

        store.delete("a@example.com").unwrap();
        store.delete("a@example.com").unwrap();
        assert_eq!(store.load("a@example.com").unwrap(), None);
        assert_eq!(
            store.load("b@example.com").unwrap().as_deref(),
            Some("token-b")
        );
    }

    #[test]
    fn memory_store_debug_hides_tokens() {
        let store = MemoryTokenStore::new();
        store.save("a@example.com", "secret-token").unwrap();
        let debug = format!("{store:?}");
        assert!(debug.contains("a@example.com") && !debug.contains("secret-token"));
    }

    #[test]
    fn keyring_store_uses_the_default_service() {
        assert_eq!(
            KeyringTokenStore::default().service,
            KeyringTokenStore::DEFAULT_SERVICE
        );
    }
}
