//! Signing in and out, and building a Gmail client for the chosen account.

use std::sync::Arc;

use anyhow::Context as _;
use openmail_gmail::GmailClient;
use openmail_gmail::api::GmailClientBuilder;
use openmail_gmail::auth::{
    AuthError, CLIENT_ID_ENV, CLIENT_SECRET_ENV, KeyringTokenStore, OAuthClient, OAuthConfig,
    RefreshingTokenProvider, TokenStore,
};

use crate::accounts::{self, AccountList};

/// `login`: signs in through the browser, keeps the refresh token in the
/// keychain and remembers the account.
pub async fn login() -> anyhow::Result<()> {
    let client = OAuthClient::new(&oauth_config()?)?;
    let list = AccountList::open_default()?;

    eprintln!("Opening Google sign-in in your browser. Press Ctrl-C to cancel.");
    let authorization = client
        .sign_in_with(|url| {
            eprintln!("If it does not open, visit:\n\n  {url}\n");
            if let Err(err) = open::that_detached(url.as_str()) {
                tracing::warn!(%err, "could not open the browser");
            }
            Ok(())
        })
        .await
        .context("sign-in failed")?;

    let provider =
        RefreshingTokenProvider::from_authorization(client, keychain(), authorization).await?;
    list.add(provider.account())?;
    tracing::debug!(path = %list.path().display(), "saved the account list");

    println!("Signed in as {}", provider.account());
    Ok(())
}

/// `logout`: deletes the account's refresh token from the keychain and forgets
/// the account. An explicit `--account` is signed out even if it is not listed,
/// to clean up a token left behind.
pub async fn logout(requested: Option<&str>) -> anyhow::Result<()> {
    let list = AccountList::open_default()?;
    let account = match requested {
        Some(requested) => list
            .load()?
            .into_iter()
            .find(|a| a.eq_ignore_ascii_case(requested))
            .unwrap_or_else(|| requested.to_owned()),
        None => accounts::select(&list.load()?, None)?,
    };

    let store = keychain();
    let key = account.clone();
    tokio::task::spawn_blocking(move || store.delete(&key))
        .await
        .context("the keychain task failed")??;
    list.remove(&account)?;

    println!("Signed out {account}");
    Ok(())
}

/// A Gmail client for the account picked by `--account` (or the only one).
pub fn gmail_client(requested: Option<&str>) -> anyhow::Result<GmailClient> {
    account_client(requested).map(|(_, client)| client)
}

/// The account picked by `--account` (or the only one), and a Gmail client for it.
pub fn account_client(requested: Option<&str>) -> anyhow::Result<(String, GmailClient)> {
    account_client_with(requested, |builder| builder)
}

/// Like [`account_client`], with the client's settings adjusted by `configure`.
pub fn account_client_with(
    requested: Option<&str>,
    configure: impl FnOnce(GmailClientBuilder) -> GmailClientBuilder,
) -> anyhow::Result<(String, GmailClient)> {
    let config = oauth_config()?;
    let account = accounts::select(&AccountList::open_default()?.load()?, requested)?;
    tracing::debug!(%account, "using account");
    let provider =
        RefreshingTokenProvider::new(OAuthClient::new(&config)?, keychain(), account.clone());
    let client = configure(GmailClient::builder(Arc::new(provider))).build()?;
    Ok((account, client))
}

/// Whether `err` means the account has to sign in again.
pub fn needs_sign_in(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|e| e.downcast_ref::<AuthError>())
        .any(AuthError::requires_sign_in)
}

fn keychain() -> Arc<dyn TokenStore> {
    Arc::new(KeyringTokenStore::new())
}

/// The OAuth client from the environment, or an explanation of how to create one.
fn oauth_config() -> anyhow::Result<OAuthConfig> {
    match OAuthConfig::from_env() {
        Ok(config) => Ok(config),
        Err(AuthError::MissingClientId) => Err(anyhow::anyhow!(missing_client_id_help())),
        Err(err) => Err(err.into()),
    }
}

fn missing_client_id_help() -> String {
    format!(
        "\
{CLIENT_ID_ENV} is not set.

Openmail is not verified by Google yet, so you sign in with your own OAuth client:

  1. Create a project at https://console.cloud.google.com/ and enable the Gmail API
     (APIs & Services > Library > Gmail API).
  2. Configure the OAuth consent screen: user type External, publishing status
     Testing, and add your Gmail address as a test user.
  3. Create credentials: APIs & Services > Credentials > Create credentials >
     OAuth client ID, application type \"Desktop app\".
  4. Export the client ID and secret it shows you:

       export {CLIENT_ID_ENV}=<client ID>
       export {CLIENT_SECRET_ENV}=<client secret>

See crates/openmail-cli/README.md for details."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_client_id_help_names_the_variables_and_steps() {
        let help = missing_client_id_help();
        assert!(help.starts_with("OPENMAIL_GOOGLE_CLIENT_ID is not set."));
        assert!(help.contains("export OPENMAIL_GOOGLE_CLIENT_ID=<client ID>"));
        assert!(help.contains("export OPENMAIL_GOOGLE_CLIENT_SECRET=<client secret>"));
        assert!(help.contains("Gmail API"));
        assert!(help.contains("test user"));
        assert!(help.contains("Desktop app"));
    }

    #[test]
    fn sign_in_errors_are_detected_through_context() {
        let err = anyhow::Error::new(AuthError::NotSignedIn("ada@example.com".to_owned()))
            .context("could not list labels");
        assert!(needs_sign_in(&err));

        let err = anyhow::Error::new(AuthError::StateMismatch);
        assert!(!needs_sign_in(&err));
        assert!(!needs_sign_in(&anyhow::anyhow!("something else")));
    }

    #[test]
    fn sign_in_errors_are_detected_inside_gmail_errors() {
        let token_error: openmail_gmail::api::TokenError =
            Box::new(AuthError::InvalidGrant("revoked".to_owned()));
        let err = anyhow::Error::new(openmail_gmail::api::Error::Token(token_error));
        assert!(needs_sign_in(&err));
    }
}
