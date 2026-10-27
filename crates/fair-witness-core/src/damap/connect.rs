use anyhow::{Context, Result, anyhow};

use super::{Damap, InstanceConfig};
use crate::auth::{Notices, Oidc, Prompts};
use crate::project::{DamapLogin, Project};

/// What `log_in` may ask of the person in front of the screen.
#[derive(Clone, Copy)]
pub enum Login<'a> {
    /// Nothing, so that only a saved session that can be refreshed logs in, since nobody can log
    /// in during a headless run.
    SavedOnly(&'a dyn Notices),
    /// To log in when there is no saved session or it cannot be refreshed.
    Ask(&'a dyn Prompts),
    /// Also whether to keep a saved session that can be refreshed, as `setup` asks.
    AskToStay(&'a dyn Prompts),
}

impl<'a> Login<'a> {
    fn notices(self) -> &'a dyn Notices {
        match self {
            Login::SavedOnly(notices) => notices,
            Login::Ask(prompts) | Login::AskToStay(prompts) => prompts,
        }
    }
}

pub async fn reach(url: &str) -> Result<(Damap, InstanceConfig)> {
    let damap = Damap::new(url)
        .map_err(|e| anyhow!("{e:#}\nTo use a different URL, run `fair-witness setup`."))?;
    let instance = damap.instance_config().await.map_err(|e| {
        anyhow!("{e:#}\nIs DAMAP running? To use a different URL, run `fair-witness setup`.")
    })?;
    Ok((damap, instance))
}

/// Logs `damap` in at the login server `instance` names, with the `saved` session while it can
/// be refreshed, and saves the new session in `project`. It writes nothing else, so that
/// `config.toml` stays as the person left it.
pub async fn log_in(
    project: &Project,
    mut damap: Damap,
    instance: &InstanceConfig,
    saved: Option<&DamapLogin>,
    login: Login<'_>,
) -> Result<Damap> {
    let title = instance.app_title.as_deref().unwrap_or("DAMAP");
    login
        .notices()
        .notice(&format!("Connected to {title} at {}", damap.url()));
    let oidc = Oidc::discover(&instance.issuer, &instance.client_id, &instance.scope).await?;
    let tokens = match login {
        Login::SavedOnly(_) => {
            let saved =
                saved.context("not logged in to DAMAP; log in with `fair-witness setup`")?;
            let refresh_token = saved
                .refresh_token_for(damap.url(), oidc.issuer())
                .map_err(|e| anyhow!("{e}; log in with `fair-witness setup`"))?;
            oidc.refresh(refresh_token)
                .await
                .context("the DAMAP login has expired; log in again with `fair-witness setup`")?
        }
        Login::Ask(prompts) | Login::AskToStay(prompts) => {
            let refreshed = match saved {
                Some(saved) => Some(match saved.refresh_token_for(damap.url(), oidc.issuer()) {
                    Ok(refresh_token) => oidc.refresh(refresh_token).await,
                    Err(e) => Err(e),
                }),
                None => None,
            };
            match refreshed {
                Some(Ok(_))
                    if matches!(login, Login::AskToStay(_)) && !prompts.stay_logged_in()? =>
                {
                    oidc.login(prompts).await?
                }
                Some(Ok(tokens)) => tokens,
                Some(Err(e)) => {
                    prompts.notice(&format!("{e}. Please log in again."));
                    oidc.login(prompts).await?
                }
                None => {
                    prompts.notice("Not logged in.");
                    oidc.login(prompts).await?
                }
            }
        }
    };
    save_login(
        project,
        damap.url(),
        &oidc,
        tokens.refresh_token,
        login.notices(),
    )?;
    damap.set_access_token(tokens.access_token);
    Ok(damap)
}

/// Saves the session `refresh_token` from the login server `oidc` for the DAMAP at `url`.
fn save_login(
    project: &Project,
    url: &str,
    oidc: &Oidc,
    refresh_token: Option<String>,
    notices: &dyn Notices,
) -> Result<()> {
    let mut credentials = project.credentials()?;
    // A login without a refresh token leaves none saved, since the one from before may be for
    // another DAMAP. A refresh always brings one, the old one when the login server issues none.
    credentials.damap = refresh_token.map(|refresh_token| DamapLogin {
        url: url.to_string(),
        login_server: Some(oidc.issuer().to_string()),
        refresh_token,
    });
    project.save_credentials(&credentials)?;
    if credentials.damap.is_none() {
        notices.notice(
            "The login server issued no refresh token; you will have to log in again next time.",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::thread::JoinHandle;

    use serde_json::{Value, json};

    use super::*;
    use crate::project::Credentials;
    use crate::testing::{Reply, Request, answer, fake_model};

    /// Keeps the notices, logs in as `user` and answers `stay` when asked whether to stay logged
    /// in.
    #[derive(Default)]
    struct Person {
        stay: bool,
        notices: RefCell<Vec<String>>,
    }

    impl Notices for Person {
        fn notice(&self, message: &str) {
            self.notices.borrow_mut().push(message.to_string());
        }
    }

    impl Prompts for Person {
        fn confirm_code(&self, _: &str, _: &str) {}

        fn username_and_password(&self) -> Result<(String, String)> {
            Ok(("user".to_string(), "user".to_string()))
        }

        fn stay_logged_in(&self) -> Result<bool> {
            Ok(self.stay)
        }
    }

    fn tokens(refresh_token: &str) -> Reply {
        json!({ "access_token": "access", "refresh_token": refresh_token }).into()
    }

    fn expired() -> Reply {
        answer(
            "400 Bad Request",
            json!({ "error": "invalid_grant", "error_description": "Token is not active" }),
        )
    }

    /// Logs in with `login` and a saved session, at a login server that answers its token
    /// requests with `answers`. Gives back why it failed, if it did, the session saved then, the
    /// token endpoint and its requests, to be read after the rest is checked, since the server
    /// waits for every request it has an answer for.
    async fn log_in_with(
        answers: Vec<Reply>,
        login: Login<'_>,
    ) -> (Option<String>, String, String, JoinHandle<Vec<Request>>) {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        let (token_server, token_requests) = fake_model(answers);
        let token_endpoint = format!("{token_server}/token");
        let (issuer, _) = fake_model(vec![json!({ "token_endpoint": token_endpoint })]);
        let saved = DamapLogin {
            url: "https://damap.example.org".to_string(),
            login_server: Some(issuer.clone()),
            refresh_token: "saved".to_string(),
        };
        project
            .save_credentials(&Credentials {
                damap: Some(saved.clone()),
                ai: None,
            })
            .unwrap();
        let instance = InstanceConfig {
            issuer,
            client_id: "damap".to_string(),
            scope: "openid".to_string(),
            app_title: None,
        };
        let damap = Damap::new(&saved.url).unwrap();

        let failed = log_in(&project, damap, &instance, Some(&saved), login)
            .await
            .err()
            .map(|e| format!("{e:#}"));

        assert!(project.config().unwrap().is_none());
        let session = project.credentials().unwrap().damap.unwrap().refresh_token;
        (failed, session, token_endpoint, token_requests)
    }

    /// The grant of each token request.
    fn grants(requests: JoinHandle<Vec<Request>>) -> Vec<String> {
        requests
            .join()
            .unwrap()
            .iter()
            .map(|(_, body)| {
                let Value::String(form) = body else {
                    panic!("not a form: {body}")
                };
                form.split('&')
                    .find_map(|field| field.strip_prefix("grant_type="))
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn logs_in_again_only_when_it_may_ask() {
        let refused = |token_endpoint: &str| {
            format!(
                "session could not be refreshed: {token_endpoint} answered 400 Bad Request: \
                 Token is not active (invalid_grant)"
            )
        };
        let connected = "Connected to DAMAP at https://damap.example.org".to_string();
        let both = ["refresh_token", "password"];

        let headless = Person::default();
        let (failed, session, token_endpoint, requests) =
            log_in_with(vec![expired()], Login::SavedOnly(&headless)).await;
        assert_eq!(
            (failed, session, headless.notices.into_inner()),
            (
                Some(format!(
                    "the DAMAP login has expired; log in again with `fair-witness setup`: {}",
                    refused(&token_endpoint)
                )),
                "saved".to_string(),
                vec![connected.clone()],
            )
        );
        assert_eq!(grants(requests), ["refresh_token"]);

        let screen = Person::default();
        let (failed, session, token_endpoint, requests) =
            log_in_with(vec![expired(), tokens("new")], Login::Ask(&screen)).await;
        assert_eq!(
            (failed, session, screen.notices.into_inner()),
            (
                None,
                "new".to_string(),
                vec![
                    connected.clone(),
                    format!("{}. Please log in again.", refused(&token_endpoint)),
                ],
            )
        );
        assert_eq!(grants(requests), both);

        let setup = Person::default();
        let (failed, session, _, requests) = log_in_with(
            vec![tokens("refreshed"), tokens("new")],
            Login::AskToStay(&setup),
        )
        .await;
        assert_eq!(
            (failed, session, setup.notices.into_inner()),
            (None, "new".to_string(), vec![connected])
        );
        assert_eq!(grants(requests), both);
    }

    #[tokio::test]
    async fn saves_a_login_with_its_servers_and_drops_it_after_one_without_a_refresh_token() {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        let (issuer, _) = fake_model(vec![json!({
            "token_endpoint": "https://login.example.org/token",
        })]);
        let oidc = Oidc::discover(&issuer, "damap", "openid").await.unwrap();
        let person = Person::default();
        let log_in = |url: &str, refresh_token: Option<&str>| {
            let damap = Damap::new(url).unwrap();
            let refresh_token = refresh_token.map(str::to_string);
            save_login(&project, damap.url(), &oidc, refresh_token, &person).unwrap();
            project.credentials().unwrap().damap
        };

        let with_token = log_in("https://damap.example.org/", Some("refresh"));
        let without = log_in("https://other.example.org", None);

        assert_eq!(
            [with_token, without],
            [
                Some(DamapLogin {
                    url: "https://damap.example.org".to_string(),
                    login_server: Some(issuer),
                    refresh_token: "refresh".to_string(),
                }),
                None,
            ]
        );
        assert_eq!(
            person.notices.into_inner(),
            [
                "The login server issued no refresh token; you will have to log in again next \
                 time."
            ]
        );
    }
}
