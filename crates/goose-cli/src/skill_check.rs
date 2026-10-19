//! Optional, deterministic completion checks for explicitly invoked headless skills.
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckConfig {
    command: Vec<String>,
    timeout_seconds: u64,
}

pub(crate) struct SkillCheck {
    config: CheckConfig,
    working_dir: PathBuf,
    skill: String,
    arguments: String,
    run_id: String,
    started_at: DateTime<Utc>,
    skill_prompt: String,
}

#[derive(Serialize)]
struct CheckInput<'a> {
    skill: &'a str,
    arguments: &'a str,
    run_id: &'a str,
    started_at: DateTime<Utc>,
    response: &'a str,
}

impl SkillCheck {
    pub(crate) fn for_prompt(prompt: &str, run_id: &str) -> Result<Option<Self>> {
        let Some(command) = prompt.trim().strip_prefix('/') else {
            return Ok(None);
        };
        let (name, arguments) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        let working_dir = std::env::current_dir()?;
        let skill = goose::skills::list_installed_skills(Some(&working_dir))
            .into_iter()
            .find(|skill| skill.name.eq_ignore_ascii_case(name));
        let Some(skill) = skill else { return Ok(None) };
        let Some(mut check) = Self::from_skill(
            &skill.name,
            skill.properties.get("headless-check"),
            arguments,
            run_id,
            working_dir,
        )?
        else {
            return Ok(None);
        };
        check.skill_prompt =
            goose::skills::loaded_skill_context_with_args(&skill, Some(arguments))?;
        Ok(Some(check))
    }

    fn from_skill(
        skill: &str,
        value: Option<&serde_json::Value>,
        arguments: &str,
        run_id: &str,
        working_dir: PathBuf,
    ) -> Result<Option<Self>> {
        let Some(value) = value else {
            return Ok(None);
        };
        let config: CheckConfig = serde_json::from_value(value.clone())
            .context("Invalid skill metadata.headless-check")?;
        if config.command.is_empty() || config.command[0].is_empty() || config.timeout_seconds == 0
        {
            bail!("headless-check requires a command and a positive timeout_seconds");
        }
        Ok(Some(Self {
            config,
            working_dir,
            skill: skill.to_string(),
            arguments: arguments.trim().to_string(),
            run_id: run_id.to_string(),
            started_at: Utc::now(),
            skill_prompt: String::new(),
        }))
    }

    pub(crate) fn prompt(&self) -> String {
        format!(
            "Headless skill completion validation is enabled for /{}.\n\
             Original invocation arguments: {}\n\
             Execute the requested arguments above, not an example or default in the skill.\n\
             Run ID: {}\nStarted at: {}\n\
             Follow the skill's CI result contract. Its validator will receive only your final \
             response, the original arguments, and this run identity after you finish. \
             Missing or failing results fail the CLI. Print progress during execution; \
             reserve the final response for the required result. Do not repair or rerun a \
             failed test to change its verdict.\n\n{}",
            self.skill,
            self.arguments,
            self.run_id,
            self.started_at.to_rfc3339(),
            self.skill_prompt
        )
    }

    pub(crate) async fn validate(&self, response: &str, output_format: &str) -> Result<()> {
        let input = serde_json::to_vec(&CheckInput {
            skill: &self.skill,
            arguments: &self.arguments,
            run_id: &self.run_id,
            started_at: self.started_at,
            response,
        })?;
        let mut command = Command::new(&self.config.command[0]);
        command
            .args(&self.config.command[1..])
            .current_dir(&self.working_dir)
            .stdin(Stdio::piped())
            .stdout(if output_format == "text" {
                Stdio::inherit()
            } else {
                Stdio::from(std::io::stderr())
            })
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("Could not start skill completion check")?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let result =
            tokio::time::timeout(Duration::from_secs(self.config.timeout_seconds), async {
                // Drain/wait concurrently: a validator may reject before consuming its input.
                let (write, exit) = tokio::join!(
                    async {
                        let result = stdin.write_all(&input).await;
                        drop(stdin);
                        result
                    },
                    child.wait()
                );
                let exit = exit?;
                if !exit.success() {
                    bail!("Skill /{} completion check failed ({exit})", self.skill);
                }
                write.context("Skill completion check did not consume its input")?;
                Ok(())
            })
            .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                let _ = child.kill().await;
                bail!("Skill /{} completion check timed out", self.skill)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(command: &[&str]) -> SkillCheck {
        SkillCheck {
            config: CheckConfig {
                command: command.iter().map(|s| s.to_string()).collect(),
                timeout_seconds: 1,
            },
            working_dir: std::env::current_dir().unwrap(),
            skill: "fixture".into(),
            arguments: "staging/".into(),
            run_id: "fixture-run".into(),
            started_at: Utc::now(),
            skill_prompt: String::new(),
        }
    }

    #[test]
    fn metadata_is_opt_in_and_invalid_contracts_fail_closed() {
        let root = std::env::current_dir().unwrap();
        assert!(
            SkillCheck::from_skill("fixture", None, "staging/", "run", root.clone())
                .unwrap()
                .is_none()
        );
        for value in [
            serde_json::json!({"command": [], "timeout_seconds": 1}),
            serde_json::json!({"command": ["true"], "timeout_seconds": 0}),
            serde_json::json!({"command": "true", "timeout_seconds": 1}),
        ] {
            assert!(SkillCheck::from_skill(
                "fixture",
                Some(&value),
                "staging/",
                "run",
                root.clone()
            )
            .is_err());
        }
        let value = serde_json::json!({"command": ["true"], "timeout_seconds": 1});
        let mut check =
            SkillCheck::from_skill("fixture", Some(&value), "staging/", "fresh-run", root)
                .unwrap()
                .unwrap();
        check.skill_prompt = "Canonical skill content".into();
        assert!(check.prompt().contains("fresh-run"));
        assert!(check
            .prompt()
            .contains("Original invocation arguments: staging/"));
        assert!(check.prompt().contains("Canonical skill content"));
        let envelope = serde_json::to_value(CheckInput {
            skill: &check.skill,
            arguments: &check.arguments,
            run_id: &check.run_id,
            started_at: check.started_at,
            response: "verdict",
        })
        .unwrap();
        assert_eq!(envelope["arguments"], "staging/");
        assert_eq!(envelope["run_id"], "fresh-run");
        assert_eq!(envelope["response"], "verdict");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn checker_exit_controls_success_even_when_response_claims_pass() {
        assert!(check(&["sh", "-c", "cat >/dev/null; exit 0"])
            .validate("PASS", "text")
            .await
            .is_ok());
        assert!(check(&["sh", "-c", "cat >/dev/null; exit 2"])
            .validate("PASS", "text")
            .await
            .unwrap_err()
            .to_string()
            .contains("failed"));
        assert!(check(&["/nonexistent/skill-validator"])
            .validate("PASS", "text")
            .await
            .is_err());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn checker_cannot_wait_forever() {
        assert!(check(&["sh", "-c", "cat >/dev/null; exec sleep 30"])
            .validate("PASS", "text")
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn original_arguments_are_data_not_shell_code() {
        let mut check = check(&["sh", "-c", "cat >/dev/null"]);
        check.arguments = "staging/ $(exit 42)".into();
        assert!(check.validate("FAIL", "text").await.is_ok());
    }
}
