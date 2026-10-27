use anyhow::Result;
use dialoguer::{Confirm, Input, Password};
use fair_witness_core::auth::{Notices, Prompts};

/// Asks for what the login needs on the terminal.
pub struct TerminalPrompts;

impl Notices for TerminalPrompts {
    fn notice(&self, message: &str) {
        println!("{message}");
    }
}

impl Prompts for TerminalPrompts {
    fn confirm_code(&self, link: &str, code: &str) {
        println!("To log in, open {link}");
        println!("and confirm the code {code}");
        let _ = open::that(link);
    }

    fn username_and_password(&self) -> Result<(String, String)> {
        let username = Input::new().with_prompt("Username").interact_text()?;
        let password = Password::new().with_prompt("Password").interact()?;
        Ok((username, password))
    }

    fn stay_logged_in(&self) -> Result<bool> {
        Ok(Confirm::new()
            .with_prompt("Stay logged in?")
            .default(true)
            .interact()?)
    }
}
