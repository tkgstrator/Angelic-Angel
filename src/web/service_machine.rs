//! What the machine API's account management needs from a service. Its callers hold the account's
//! mutation lock (`Service::machine_lock`) for the whole request, so nothing here takes it.

use super::*;

/// The saved facts about one account that the machine API checks or reports. Never a credential.
pub(crate) struct MachineProfile {
    pub owner: Option<String>,
    pub x_user_id: Option<String>,
    pub post: bool,
    pub like: bool,
    pub follow: bool,
}

impl<B: Backend> Service<B> {
    pub(crate) fn machine_profile(&self) -> std::result::Result<MachineProfile, ApiError> {
        let account = self.load()?;
        Ok(MachineProfile {
            owner: account.machine_owner,
            x_user_id: account.x_user_id,
            post: account.machine_post_enabled,
            like: account.machine_like_enabled,
            follow: account.machine_follow_enabled,
        })
    }

    /// Edits the stored account under the config lock; `edit` may refuse. The account itself is
    /// never returned, so no credential can leave through here.
    pub(crate) fn machine_edit(
        &self,
        edit: impl FnOnce(&mut Account) -> std::result::Result<(), ApiError>,
    ) -> std::result::Result<(), ApiError> {
        self.ensure_live()?;
        Config::modify_account(&self.config_path, &self.id, |current| {
            let mut account = current.ok_or_else(ApiError::account_not_found)?;
            edit(&mut account)?;
            Ok((Some(account), ()))
        })
    }

    /// Saves new cookies the way the WebUI does: not while registered and not during a bulk job.
    pub(crate) fn machine_set_credentials(
        &self,
        twitter: TwitterConfig,
    ) -> std::result::Result<(), ApiError> {
        self.ensure_no_following_job()?;
        self.machine_edit(|account| {
            if account.registration.is_some() {
                return Err(ApiError::conflict(
                    "credentials_locked",
                    "Unregister before changing the saved credentials.",
                ));
            }
            account.twitter = twitter;
            Ok(())
        })
    }
}

impl<B: Backend> Accounts<B> {
    /// The accounts a machine client created, with the client that owns each one.
    pub(crate) fn machine_owners(&self) -> std::result::Result<HashMap<String, String>, ApiError> {
        let config = Config::load_optional(&self.config_path)?.unwrap_or_default();
        let mut owners = HashMap::new();
        for account in config.accounts {
            if let Some(owner) = account.machine_owner {
                owners.insert(account.id, owner);
            }
        }
        Ok(owners)
    }

    /// [`Accounts::delete`] for a caller that already holds the service's mutation lock as `slot`.
    pub(crate) async fn delete_locked(
        &self,
        service: &Arc<Service<B>>,
        slot: &mut Option<JoinHandle<()>>,
    ) -> std::result::Result<(), ApiError> {
        let outcome = service.delete_locked(slot).await;
        self.forget_removed(&service.id, service);
        outcome
    }
}
