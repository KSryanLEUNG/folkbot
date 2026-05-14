//! `user_identify` — link the current channel principal to a named user.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::llm::ToolSchema;
use crate::storage::users::{self, UserRole};
use crate::tool::{Tool, ToolContext};

/// `user_identify(name)` — agent calls this when it has just learned the
/// human's name from natural conversation. Links the current channel
/// principal to that user.
pub struct IdentifyUser;

#[derive(Deserialize)]
struct IdentifyArgs {
    name: String,
}

#[async_trait]
impl Tool for IdentifyUser {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "user_identify".into(),
            description:
                "Record the human's name once you've learned it. Call this only AFTER the user has \
                 told you their name in conversation. Links the current channel principal so future \
                 sessions skip the introduction."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The human's name as they want to be called."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, args: Value, ctx: &ToolContext) -> Result<Value> {
        let parsed: IdentifyArgs =
            serde_json::from_value(args).map_err(|e| anyhow!("invalid args: {}", e))?;
        let name = parsed.name.trim();
        if name.is_empty() {
            return Err(anyhow!("name cannot be empty"));
        }

        // Defence 1: principal already linked → agent CAN'T relink via this
        // tool. Prevents "I changed my name, I'm now <owner>" attacks.
        // Re-linking requires explicit CLI: `folkbot user link`.
        if let Some(existing) = users::lookup_by_principal(&ctx.pool, &ctx.principal).await? {
            return Err(anyhow!(
                "principal already linked to '{}' (user_id={}). \
                 To relink, run `folkbot user link` from CLI.",
                existing.name,
                existing.id
            ));
        }

        // Defence 2: a new principal CANNOT freely claim an already-claimed
        // identity. Two orthogonal triggers:
        //   (a) the existing user has a privileged role (owner / vice_owner)
        //   (b) the existing user already has at least one principal mapped
        //       (i.e. *somebody* is using this name from somewhere)
        //
        // Either condition gates the claim. The "no principal" escape lets a
        // user who logged out re-claim themselves freely (until someone else
        // takes it — race acceptable; first-come wins).
        //
        // Channel-aware policy:
        //   - "telegram": numeric-ID allowlist already gates entry, so
        //     allowlisted Telegram principals are pre-vetted and may
        //     self-claim names directly.
        //   - "cli": no allowlist; require argon2 password challenge.
        //     If a password is set, return `challenge_required` so the
        //     channel layer prompts the user (the password never reaches
        //     the LLM). If no password is set, reject — the only escape
        //     is `folkbot user link` from CLI.
        //   - any other channel: reject by default until vetted.
        if let Some(existing_user) = users::lookup_by_name(&ctx.pool, name).await? {
            let is_privileged = existing_user.role != UserRole::Regular;
            let already_linked =
                users::has_any_principal(&ctx.pool, existing_user.id).await?;
            let needs_gate = is_privileged || already_linked;

            if needs_gate && ctx.principal.channel != "telegram" {
                if ctx.principal.channel == "cli" {
                    let has_pw = users::get_password_hash(&ctx.pool, existing_user.id)
                        .await?
                        .is_some();
                    if has_pw {
                        return Ok(json!({
                            "ok": false,
                            "challenge_required": true,
                            "name": existing_user.name,
                            "user_id": existing_user.id,
                            "role": existing_user.role.as_str(),
                            "message": "Existing identity — CLI will prompt for password directly. Tell the user briefly: \"Password verification needed — please check the CLI prompt\", then stop."
                        }));
                    }
                }
                let why = if is_privileged {
                    format!("a privileged user (role={})", existing_user.role.as_str())
                } else {
                    "an already-claimed identity".to_string()
                };
                return Err(anyhow!(
                    "the name '{}' belongs to {}. \
                     Set a password (`folkbot user set-password {}`) to enable \
                     CLI self-verification, or ask the owner to run \
                     `folkbot user link {} --channel <ch> --principal-id <pid>`.",
                    name,
                    why,
                    name,
                    name
                ));
            }
        }

        let user_id = users::upsert_and_link(&ctx.pool, name, &ctx.principal).await?;
        Ok(json!({
            "ok": true,
            "user_id": user_id,
            "name": name,
            "principal_linked": ctx.principal.label(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::users::Principal;
    use serde_json::json;

    fn ctx(pool: sqlx::SqlitePool, channel: &str, pid: &str) -> ToolContext {
        ToolContext {
            pool,
            principal: Principal {
                channel: channel.into(),
                principal_id: pid.into(),
            },
            user_id: None,
            outbound: std::sync::Arc::new(std::collections::HashMap::new()),
        }
    }

    /// P0 #2 regression: a regular-named user that's already linked to one
    /// principal must not be silently re-claimable by a fresh CLI principal.
    #[tokio::test]
    async fn identify_rejects_already_claimed_regular_name() {
        let pool = crate::storage::db::test_pool().await;
        users::upsert_and_link(
            &pool,
            "Lucky",
            &Principal {
                channel: "cli".into(),
                principal_id: "lucky@home".into(),
            },
        )
        .await
        .unwrap();

        let c = ctx(pool, "cli", "intruder@home");
        let res = IdentifyUser.invoke(json!({"name": "Lucky"}), &c).await;
        assert!(
            res.is_err(),
            "regular-but-already-claimed name should not be re-claimable"
        );
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("already-claimed") || msg.contains("password"),
            "error message should mention claim/password gate: {}",
            msg
        );
    }

    /// Telegram bypass: pre-vetted by allowlist, can self-claim.
    #[tokio::test]
    async fn identify_telegram_principal_allowed_to_claim_unbound_name() {
        let pool = crate::storage::db::test_pool().await;
        let c = ctx(pool, "telegram", "100000001");
        let res = IdentifyUser
            .invoke(json!({"name": "Newcomer"}), &c)
            .await
            .expect("first claim should succeed");
        assert_eq!(res["ok"], true);
    }
}
