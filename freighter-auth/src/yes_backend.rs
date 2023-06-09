//! A backend that says "yes" to every request for authorization.
//!
//! This is exactly as insecure as it sounds, and is meant primarily for testing purposes.

use crate::{AuthProvider, AuthResult, ListedOwner};
use async_trait::async_trait;
use rand::distributions::{Alphanumeric, DistString};

pub struct YesAuthProvider;

#[async_trait]
impl AuthProvider for YesAuthProvider {
    async fn register(&self, _username: &str, _password: &str) -> AuthResult<String> {
        let token = Alphanumeric.sample_string(&mut rand::thread_rng(), 32);

        Ok(token)
    }

    async fn login(&self, username: &str, password: &str) -> AuthResult<String> {
        self.register(username, password).await
    }

    async fn list_owners(&self, _token: &str, _crate_name: &str) -> AuthResult<Vec<ListedOwner>> {
        Ok(Vec::new())
    }

    async fn add_owners(&self, _token: &str, _users: &[&str], _crate_name: &str) -> AuthResult<()> {
        Ok(())
    }

    async fn remove_owners(
        &self,
        _token: &str,
        _users: &[&str],
        _crate_name: &str,
    ) -> AuthResult<()> {
        Ok(())
    }

    async fn publish(&self, _token: &str, _crate_name: &str) -> AuthResult<()> {
        Ok(())
    }

    async fn auth_yank(&self, _token: &str, _crate_name: &str) -> AuthResult<()> {
        Ok(())
    }

    async fn auth_unyank(&self, _token: &str, _crate_name: &str) -> AuthResult<()> {
        Ok(())
    }
}
