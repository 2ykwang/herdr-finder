use herdr_finder::{
    client::Client,
    ui,
    visits::{VisitHistory, visit_time},
};
use std::{env, error::Error, process::ExitCode};

const OPEN_ACTION: &str = "herdr-finder.open";

fn shortcut() -> String {
    format!(
        "[[keys.command]]\n\
         key = \"prefix+shift+f\"\n\
         type = \"plugin_action\"\n\
         command = \"{OPEN_ACTION}\"\n\
         description = \"Find a running session\""
    )
}

fn print_help() {
    println!(
        "{} {}\n\
         Usage: {} [run|open|shortcut|record-visit|--version|--help]\n\n\
         run opens the finder UI in the current Herdr popup.\n\
         open invokes the installed plugin pane through Herdr.\n\
         shortcut prints a plugin-action key binding.\n\
         record-visit stores workspace focus events from Herdr.\n\n\
         Install and update this plugin with Herdr's plugin manager.",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_NAME"),
    );
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() > 1 {
        return Err("Expected one command. Use --help for usage.".into());
    }
    let action = args
        .first()
        .map(|arg| arg.to_str().ok_or("The command is not valid UTF-8"))
        .transpose()?
        .unwrap_or("run");

    match action {
        "version" | "--version" | "-V" => println!("{}", env!("CARGO_PKG_VERSION")),
        "help" | "--help" | "-h" => print_help(),
        "shortcut" => println!("{}", shortcut()),
        "open" => Client::from_env()?.open_popup()?,
        "run" => ui::run(Client::from_env()?.excluding_caller())?,
        "record-visit" => {
            let at = visit_time();
            let client = Client::from_env()?;
            let history = VisitHistory::from_env(client.socket_path())
                .ok_or("HERDR_PLUGIN_STATE_DIR is required")?;
            let event: serde_json::Value =
                serde_json::from_str(&env::var("HERDR_PLUGIN_EVENT_JSON")?)?;
            history.event(&event, at)?;
        }
        other => return Err(format!("Unknown command: {other}").into()),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", env!("CARGO_PKG_NAME"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_invokes_the_manifest_open_action_without_a_launcher_path() {
        let config: toml::Value = toml::from_str(&shortcut()).unwrap();
        let binding = &config["keys"]["command"][0];
        let manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
        let action = manifest["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["id"].as_str() == Some("open"))
            .unwrap();
        let qualified = format!("{}.open", manifest["id"].as_str().unwrap());

        assert_eq!(binding["key"].as_str(), Some("prefix+shift+f"));
        assert_eq!(binding["type"].as_str(), Some("plugin_action"));
        assert_eq!(binding["command"].as_str(), Some(qualified.as_str()));
        assert!(binding.get("width").is_none() && binding.get("height").is_none());
        assert_eq!(action["command"][1].as_str(), Some("open"));
    }
}
