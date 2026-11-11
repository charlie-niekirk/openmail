//! Signing an account in from the app: Google's consent page in the system
//! browser, a loopback redirect (PKCE), the refresh token saved to the
//! keychain, then the account's mail service started.

use std::sync::Arc;

use anyhow::Context as _;
use openmail_gmail::auth::{
    AuthError, KeyringTokenStore, OAuthClient, OAuthConfig, RefreshingTokenProvider, TokenStore,
};
use openmail_store::DataDir;
use openmail_ui::{Connection, SignInError};
use tokio_util::sync::CancellationToken;

use crate::connect;

/// Signs an account in and starts its mail service. Blocks until the user
/// finishes in the browser or `cancel` is cancelled.
pub fn sign_in(cancel: &CancellationToken) -> Result<Connection, SignInError> {
    let Some(config) = connect::oauth_config().map_err(failed)? else {
        return Err(SignInError::Unconfigured {
            help: connect::unconfigured_help(),
        });
    };
    let data_dir = DataDir::from_env_or_platform()
        .context("could not find the data directory")
        .map_err(failed)?;
    let tokens: Arc<dyn TokenStore> = Arc::new(KeyringTokenStore::new());

    // The service gets an OAuth client of its own: the sign-in client's
    // connections belong to the sign-in runtime, which has ended by then.
    let account = authorize(&config, tokens.clone(), cancel)?;
    tracing::info!(%account, "signed in");
    Ok(
        connect::start_service(&data_dir, &config, tokens, account).unwrap_or_else(|err| {
            Connection::Failed {
                error: format!("{err:#}"),
            }
        }),
    )
}

/// Runs the OAuth flow on a runtime of its own and saves the refresh token,
/// returning the account's address.
fn authorize(
    config: &OAuthConfig,
    tokens: Arc<dyn TokenStore>,
    cancel: &CancellationToken,
) -> Result<String, SignInError> {
    let client = OAuthClient::new(config).map_err(failed)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the sign-in runtime")
        .map_err(failed)?;
    runtime.block_on(async {
        let authorization = tokio::select! {
            () = cancel.cancelled() => return Err(SignInError::Cancelled),
            result = client.sign_in() => result.map_err(sign_in_error)?,
        };
        let provider = RefreshingTokenProvider::from_authorization(client, tokens, authorization)
            .await
            .map_err(failed)?;
        Ok(provider.account().to_owned())
    })
}

fn sign_in_error(err: AuthError) -> SignInError {
    match err {
        AuthError::AuthorizationDenied(_) => SignInError::Denied,
        err => failed(err),
    }
}

fn failed(err: impl Into<anyhow::Error>) -> SignInError {
    SignInError::Failed {
        error: format!("{:#}", err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declining_in_the_browser_is_not_a_failure() {
        let denied = sign_in_error(AuthError::AuthorizationDenied("access_denied".to_owned()));
        assert_eq!(denied, SignInError::Denied);

        let failed = sign_in_error(AuthError::StateMismatch);
        assert!(matches!(failed, SignInError::Failed { error } if error.contains("state")));
    }
}
