use anyhow::Result;
use fair_witness_core::auth::Oidc;
use fair_witness_core::damap::Damap;
use fair_witness_core::project::{DamapLogin, Project};

pub async fn run(project: Option<&Project>) -> Result<()> {
    println!("{}", log_out(project).await?);
    Ok(())
}

/// Ends the DAMAP session saved in `project` and forgets it, and says how that went.
async fn log_out(project: Option<&Project>) -> Result<String> {
    let Some(project) = project else {
        return Ok("Not logged in.".to_string());
    };
    let Some(login) = project.credentials()?.damap else {
        return Ok("Not logged in.".to_string());
    };
    let ended = end_session(&login).await;
    // Forgotten also when the login server cannot end it, so that logging out never depends on
    // the network. The file is read again, since `setup` may have saved an API key or another
    // login while DAMAP answered.
    let mut credentials = project.credentials()?;
    if credentials.damap.as_ref() == Some(&login) {
        credentials.damap = None;
        project.save_credentials(&credentials)?;
    }
    Ok(match ended {
        Ok(()) => "Logged out, and the DAMAP session has ended.".to_string(),
        Err(e) => format!(
            "Logged out, but the DAMAP session could not be ended, so it stays valid until it \
             expires: {e:#}"
        ),
    })
}

/// Ends `login` at the login server of the DAMAP it was saved for, which `config.toml` may no
/// longer name.
async fn end_session(login: &DamapLogin) -> Result<()> {
    let damap = Damap::new(&login.url)?;
    let instance = damap.instance_config().await?;
    let oidc = Oidc::discover(&instance.issuer, &instance.client_id, &instance.scope).await?;
    oidc.revoke(login.refresh_token_for(damap.url(), oidc.issuer())?)
        .await
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use fair_witness_core::project::{ApiKey, Config, Credentials, DamapConfig};
    use fair_witness_core::testing::{Reply, Request, answer, fake_model};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;

    /// A folder logged in to the DAMAP at `damap` through `login_server`, with an API key, and
    /// a config.toml that names another DAMAP since, which must not decide where the session
    /// ends.
    fn folder(damap: String, login_server: Option<String>) -> (TempDir, Project) {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project
            .save_config(&Config {
                damap: DamapConfig {
                    frontend_url: "https://other.example.org".to_string(),
                    dmp_id: Some(3),
                },
                ai: None,
            })
            .unwrap();
        project
            .save_credentials(&Credentials {
                damap: Some(DamapLogin {
                    url: damap,
                    login_server,
                    refresh_token: "secret".to_string(),
                }),
                ai: Some(api_key("sk-key")),
            })
            .unwrap();
        (root, project)
    }

    fn api_key(key: &str) -> ApiKey {
        ApiKey {
            url: "https://api.example.org/v1".to_string(),
            key: key.to_string(),
        }
    }

    /// A DAMAP whose login server is `login_server`.
    fn damap(login_server: &str) -> String {
        fake_model(vec![json!({
            "issuer": login_server,
            "clientID": "damap",
            "scope": "openid",
        })])
        .0
    }

    /// What logging out said, the credentials it left, the login server's URL and the requests
    /// it got, in a folder set up with a DAMAP whose login server answers `revocation` to ending
    /// the session.
    async fn log_out_with(revocation: Reply) -> (String, Credentials, String, Vec<Request>) {
        let (login, revocations) = fake_model(vec![revocation]);
        let (issuer, _) = fake_model(vec![json!({
            "token_endpoint": format!("{login}/token"),
            "revocation_endpoint": format!("{login}/revoke"),
        })]);
        let (_root, project) = folder(damap(&issuer), Some(issuer));

        let said = log_out(Some(&project)).await.unwrap();

        let credentials = project.credentials().unwrap();
        (said, credentials, login, revocations.join().unwrap())
    }

    #[tokio::test]
    async fn ends_the_session_and_forgets_it_also_when_that_fails() {
        let (ended, ended_credentials, _, requests) =
            log_out_with(answer("200 OK", Value::Null)).await;
        let (refused, refused_credentials, login, _) = log_out_with(answer(
            "401 Unauthorized",
            json!({
                "error": "invalid_client",
                "error_description": "Invalid client or Invalid client credentials",
            }),
        ))
        .await;

        assert_eq!(
            [ended, refused],
            [
                "Logged out, and the DAMAP session has ended.".to_string(),
                format!(
                    "Logged out, but the DAMAP session could not be ended, so it stays valid \
                     until it expires: {login}/revoke answered 401 Unauthorized: Invalid client \
                     or Invalid client credentials (invalid_client)"
                ),
            ]
        );
        for credentials in [ended_credentials, refused_credentials] {
            assert_eq!(
                credentials,
                Credentials {
                    damap: None,
                    ai: Some(api_key("sk-key")),
                }
            );
        }
        assert!(
            requests[0].0.starts_with("POST /v1/revoke "),
            "{}",
            requests[0].0
        );
        assert_eq!(
            requests[0].1,
            json!("client_id=damap&token=secret&token_type_hint=refresh_token")
        );
    }

    #[tokio::test]
    async fn ends_no_session_at_a_login_server_the_login_is_not_from() {
        // Accepts no connection, so that one waits for `accept` if the token was sent.
        let login = TcpListener::bind("127.0.0.1:0").unwrap();
        login.set_nonblocking(true).unwrap();
        let (issuer, _) = fake_model(vec![json!({
            "token_endpoint": "https://login.example.org/token",
            "revocation_endpoint": format!("http://{}/revoke", login.local_addr().unwrap()),
        })]);
        let damap = damap(&issuer);
        let saved_login_server = "https://login.example.org/realms/damap";
        let (_root, project) = folder(damap.clone(), Some(saved_login_server.to_string()));

        let said = log_out(Some(&project)).await.unwrap();

        assert_eq!(
            said,
            format!(
                "Logged out, but the DAMAP session could not be ended, so it stays valid until \
                 it expires: the saved DAMAP login is from the login server at \
                 {saved_login_server}, but {damap} now logs in at {issuer}"
            )
        );
        assert_eq!(project.credentials().unwrap().damap, None);
        assert!(login.accept().is_err());
    }

    #[tokio::test]
    async fn keeps_an_api_key_saved_while_it_logs_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let damap = format!("http://{}", listener.local_addr().unwrap());
        let (_root, project) = folder(damap.clone(), None);
        let saved = Credentials {
            damap: Some(DamapLogin {
                url: damap,
                login_server: None,
                refresh_token: "secret".to_string(),
            }),
            ai: Some(api_key("sk-new")),
        };
        let damap = {
            let project = project.clone();
            thread::spawn(move || {
                // `setup` saves another API key while DAMAP has not answered yet, and DAMAP
                // never does.
                let (connection, _) = listener.accept().unwrap();
                project.save_credentials(&saved).unwrap();
                drop(connection);
            })
        };

        log_out(Some(&project)).await.unwrap();

        damap.join().unwrap();
        assert_eq!(
            project.credentials().unwrap(),
            Credentials {
                damap: None,
                ai: Some(api_key("sk-new")),
            }
        );
    }
}
