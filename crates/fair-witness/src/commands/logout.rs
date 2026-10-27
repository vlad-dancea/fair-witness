use anyhow::{Context, Result};
use fair_witness_core::auth::Oidc;
use fair_witness_core::damap::Damap;
use fair_witness_core::project::Project;

pub async fn run(project: Option<&Project>) -> Result<()> {
    println!("{}", log_out(project).await?);
    Ok(())
}

/// Ends the DAMAP session saved in `project` and forgets it, and says how that went.
async fn log_out(project: Option<&Project>) -> Result<String> {
    let Some(project) = project else {
        return Ok("Not logged in.".to_string());
    };
    let Some(refresh_token) = project.credentials()?.refresh_token else {
        return Ok("Not logged in.".to_string());
    };
    let ended = end_session(project, &refresh_token).await;
    // Forgotten also when the login server cannot end it, so that logging out never depends on
    // the network. The file is read again, since `setup` may have saved an API key or another
    // login while DAMAP answered.
    let mut credentials = project.credentials()?;
    if credentials.refresh_token.as_ref() == Some(&refresh_token) {
        credentials.refresh_token = None;
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

async fn end_session(project: &Project, refresh_token: &str) -> Result<()> {
    let config = project
        .config()?
        .context("no DAMAP is saved for this folder")?;
    let instance = Damap::new(&config.damap.frontend_url)?
        .instance_config()
        .await?;
    Oidc::discover(&instance.issuer, &instance.client_id, &instance.scope)
        .await?
        .revoke(refresh_token)
        .await
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use fair_witness_core::project::{Config, Credentials, DamapConfig};
    use fair_witness_core::testing::{Reply, Request, answer, fake_model};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;

    /// A folder set up with the DAMAP at `damap`, logged in and with an API key.
    fn folder(damap: String) -> (TempDir, Project) {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project
            .save_config(&Config {
                damap: DamapConfig {
                    frontend_url: damap,
                    dmp_id: Some(3),
                },
                ai: None,
            })
            .unwrap();
        project
            .save_credentials(&Credentials {
                refresh_token: Some("secret".to_string()),
                api_key: Some("sk-key".to_string()),
            })
            .unwrap();
        (root, project)
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
        let (damap, _) = fake_model(vec![json!({
            "issuer": issuer,
            "clientID": "damap",
            "scope": "openid",
        })]);
        let (_root, project) = folder(damap);

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
            assert_eq!(credentials.refresh_token, None);
            assert_eq!(credentials.api_key.as_deref(), Some("sk-key"));
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
    async fn keeps_an_api_key_saved_while_it_logs_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (_root, project) = folder(format!("http://{}", listener.local_addr().unwrap()));
        let saved = Credentials {
            refresh_token: Some("secret".to_string()),
            api_key: Some("sk-new".to_string()),
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
        let credentials = project.credentials().unwrap();
        assert_eq!(
            (credentials.refresh_token, credentials.api_key.as_deref()),
            (None, Some("sk-new"))
        );
    }
}
