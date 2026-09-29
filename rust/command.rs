use anyhow::{Result, anyhow};

use crate::domain::{HarnessCommand, JobKind};

pub fn parse_text_command(text: &str) -> Result<HarnessCommand> {
    let trimmed = text.trim();
    let mut parts = trimmed.split_whitespace();
    let first = parts.next().unwrap_or_default();
    let rest = parts.collect::<Vec<_>>();
    match first.to_ascii_lowercase().as_str() {
        "/help" | "/start" => Ok(HarnessCommand::Help),
        "/new" | "/bug" => {
            let repository = rest
                .first()
                .ok_or_else(|| anyhow!("Usage: {first} <repo> <description>"))?;
            let prompt = rest[1..].join(" ");
            if prompt.is_empty() {
                return Err(anyhow!("Usage: {first} <repo> <description>"));
            }
            Ok(HarnessCommand::Start {
                repository: (*repository).to_owned(),
                prompt,
                kind: if first.eq_ignore_ascii_case("/bug") {
                    JobKind::Bug
                } else {
                    JobKind::Task
                },
            })
        }
        "/steer" => non_empty(rest.join(" "), "Usage: /steer <message>")
            .map(|message| HarnessCommand::Steer { message }),
        "/answer" => {
            let request_id = rest
                .first()
                .ok_or_else(|| anyhow!("Usage: /answer <id> <answer>"))?;
            let answer = rest[1..].join(" ");
            if answer.is_empty() {
                return Err(anyhow!("Usage: /answer <id> <answer>"));
            }
            Ok(HarnessCommand::Answer {
                request_id: (*request_id).to_owned(),
                answer,
            })
        }
        "/cancel" => Ok(HarnessCommand::Cancel {
            job_id: rest.first().map(|value| (*value).to_owned()),
        }),
        "/jobs" => Ok(HarnessCommand::ListJobs),
        "/use" => Ok(HarnessCommand::Select {
            job_id: rest
                .first()
                .ok_or_else(|| anyhow!("Usage: /use <job-id>"))?
                .to_string(),
        }),
        "/status" => Ok(HarnessCommand::Status),
        value if value.starts_with('/') => Err(anyhow!("Unknown command. Use /help")),
        _ => non_empty(trimmed.to_owned(), "Message cannot be empty")
            .map(|message| HarnessCommand::Continue { message }),
    }
}

fn non_empty(value: String, message: &str) -> Result<String> {
    if value.trim().is_empty() {
        Err(anyhow!(message.to_owned()))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transport_neutral_commands() {
        assert_eq!(
            parse_text_command("/new app fix parser").unwrap(),
            HarnessCommand::Start {
                repository: "app".into(),
                prompt: "fix parser".into(),
                kind: JobKind::Task,
            }
        );
        assert_eq!(
            parse_text_command("/bug app panic").unwrap(),
            HarnessCommand::Start {
                repository: "app".into(),
                prompt: "panic".into(),
                kind: JobKind::Bug,
            }
        );
        assert_eq!(
            parse_text_command("keep going").unwrap(),
            HarnessCommand::Continue {
                message: "keep going".into()
            }
        );
        assert!(parse_text_command("/new app").is_err());
        assert!(parse_text_command("/unknown").is_err());
    }
}
