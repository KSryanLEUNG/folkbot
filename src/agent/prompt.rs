//! System prompt composition — token-budgeted assembly from soul card, facts,
//! summaries, room context, and user info. Separated from the turn orchestrator
//! so the prompt builder can evolve without touching the run_turn loop.

use anyhow::Result;
use sqlx::SqlitePool;

use crate::storage::facts::{self, Fact, Importance};
use crate::storage::soul::SoulCard;
use crate::storage::summaries::{self, Period, Summary};
use crate::storage::users::{self, User, UserRole};
use crate::tokens;
use crate::tool::ToolRegistry;
use crate::util::{fmt_date, fmt_ts};

/// Fallback base system prompt when the user hasn't set `[agent]
/// system_prompt` in folkbot.toml.
pub const DEFAULT_BASE_PROMPT: &str =
    "You are a family-oriented AI assistant. Keep replies short and natural.";

/// Token budget for the composed system prompt. Sections with priority < 95
/// are dropped when the budget would be exceeded.
pub(super) const SYSTEM_PROMPT_BUDGET: usize = 2000;

/// How many recent daily summaries to load per user. Kept small so the
/// system prompt stays within budget even for long-lived users.
const RECENT_DAILY_LIMIT: usize = 5;

// ─── Turn state snapshot ──────────────────────────────────────────────

/// Per-turn snapshot of all the slow-changing inputs to `build_system_prompt`.
/// Loaded once at turn start and reloaded between tool iterations only when
/// a state-mutating tool ran. Saves ≥4 DB queries per iter when the agent
/// uses non-state-changing tools (MCP, send_message, cross_user_transcript).
pub(super) struct TurnState {
    pub soul: SoulCard,
    pub all_users: Vec<User>,
    pub user_facts: Vec<Fact>,
    pub user_summaries: Vec<Summary>,
    pub room_ctx: Option<RoomCtx>,
    pub broadcast_targets: Vec<String>,
    /// Cached so the registry doesn't rebuild + re-clone Vec<ToolSchema>
    /// on every iter. Rebuilt only when state reloads — schemas are
    /// effectively static within a turn.
    pub tool_schemas: Vec<crate::llm::ToolSchema>,
}

impl TurnState {
    pub(super) async fn load(
        pool: &SqlitePool,
        registry: &ToolRegistry,
        current_user: Option<&User>,
        conv_id: Option<i64>,
    ) -> Result<Self> {
        let soul = SoulCard::load(pool).await?;
        let all_users = users::list_all(pool).await?;
        let user_facts: Vec<Fact> = match current_user {
            Some(u) => facts::list_for_user(pool, u.id, true).await?,
            None => facts::list_for_user(pool, 0, false).await?,
        };
        let user_summaries: Vec<Summary> = match current_user {
            Some(u) => summaries::list_recent(pool, u.id, Period::Day, RECENT_DAILY_LIMIT).await?,
            None => vec![],
        };
        let room_ctx: Option<RoomCtx> = match conv_id {
            Some(cid) => {
                let label = crate::storage::conversations::label(pool, cid).await?;
                let members = crate::storage::conversations::list_members(pool, cid).await?;
                Some(RoomCtx { label, members })
            }
            None => None,
        };
        // Rooms the speaker can broadcast to via `send_message(target_room=...)`.
        // ViceOwner+ only; current room excluded (talk to it directly).
        let broadcast_targets: Vec<String> = match current_user {
            Some(u) if u.role.at_least(UserRole::ViceOwner) => {
                let rooms = crate::storage::conversations::rooms_for_user(pool, u.id).await?;
                rooms
                    .into_iter()
                    .filter(|(rid, _)| Some(*rid) != conv_id)
                    .filter_map(|(_, l)| l)
                    .collect()
            }
            _ => Vec::new(),
        };
        let tool_schemas = registry.schemas();
        Ok(Self {
            soul,
            all_users,
            user_facts,
            user_summaries,
            room_ctx,
            broadcast_targets,
            tool_schemas,
        })
    }
}

/// Per-turn room context, passed into `build_system_prompt` when the turn
/// is happening in a shared group room. `None` everywhere = DM mode.
pub(super) struct RoomCtx {
    pub label: Option<String>,
    pub members: Vec<User>,
}

// ─── Prompt assembly ──────────────────────────────────────────────────

struct PromptSection {
    priority: u16,
    content: String,
}

/// Compose the system prompt for one chat turn, respecting a token budget.
/// Sections accumulated by priority desc; sections with priority < 95 drop
/// out when budget would be exceeded.
pub(super) fn build_system_prompt(
    base: &str,
    soul: &SoulCard,
    current: Option<&User>,
    all_users: &[User],
    facts: &[Fact],
    recent_summaries: &[Summary],
    room: Option<&RoomCtx>,
    broadcast_targets: &[String],
    budget_tokens: usize,
) -> String {
    let mut sections: Vec<PromptSection> = Vec::new();

    sections.push(PromptSection {
        priority: 100,
        content: base.to_string(),
    });
    sections.push(PromptSection {
        priority: 99,
        content: soul.format_for_prompt(),
    });

    // Agent's clock — LLM training data has no concept of "today".
    // Without this, replies about "tomorrow" / "this week" / "last month"
    // anchor against the model's training cutoff, not real time.
    let now_ts = crate::storage::db::now_ts();
    let now_section = format!("\n## Current time\n{} (Asia/Hong_Kong)\n", fmt_ts(now_ts));
    sections.push(PromptSection {
        priority: 97,
        content: now_section,
    });

    let mut who = String::from("\n## Who I'm talking to\n");
    match current {
        Some(u) => {
            who.push_str(&format!(
                "You're talking to {} (user_id={}, role={}).\n",
                u.display_name.as_deref().unwrap_or(&u.name),
                u.id,
                u.role.as_str()
            ));
            // Spell out the agent's permissions for this user. Without this,
            // the LLM tends to refuse cross-user / privileged actions even
            // when they're authorized at the tool level.
            match u.role {
                UserRole::Owner => who.push_str(
                    "Owner permissions: can `soul_patch` to change my personality, can `cross_user_transcript` \
                     to read my raw conversations with other users, can `send_message` to proactively send messages to other users or groups, \
                     can `send_file` to send files from the workspace to other users or groups. \
                     When the owner asks \"what did you talk about with X\", **proactively call `cross_user_transcript`**; \
                     when the owner says \"tell X / remind X / say to X ...\", **call `send_message` directly \
                     to send it for them** (use `target=username`); \
                     when the owner says \"send X's image / file to Y\", **call `send_file`** \
                     (`workspace_path` comes from the path in the message marker, e.g. `[image: inbox/abc.jpg]` \
                     means use `workspace_path=\"inbox/abc.jpg\"`). \
                     When the owner says \"**show / send / forward to me** that image / file\", also use `send_file`, \
                     `target` is the owner's own name — text replies can't carry files, only `send_file` can. \
                     When the owner says \"say in group X / say in the family group ...\", use `target_room=group_name`. \
                     Just confirm the result with the owner after sending. \
                     Note: `send_message`'s `content` should only contain the body (e.g. \"remember to keep warm\"), \
                     don't start it with \"X wanted me to tell you\" — the tool adds attribution automatically. \
                     `send_file`'s `caption` is body-only too.\n",
                ),
                UserRole::ViceOwner => who.push_str(
                    "Vice owner permissions: can see summary-level overviews of other users (not verbatim), \
                     can `send_message` to ask me to proactively send messages to other users or groups (only groups they've joined), \
                     can `send_file` to send files from the workspace to other users or groups. \
                     Can't change my personality, can't read raw transcripts. When asked for details, honestly say \"only the gist\". \
                     When she says \"tell X / remind X ...\", **call `send_message` directly to send it for her** \
                     (use `target=username`); for \"say in the group\" use `target_room=group_name`; \
                     for \"send that image / file to ...\" use `send_file` (get `workspace_path` from the message marker); \
                     for \"show me\" also use `send_file`, `target` is her own name. \
                     Just confirm the result after sending. \
                     Note: `send_message`'s `content` / `send_file`'s `caption` \
                     should only contain the body (e.g. \"mom's working\"), don't start it with \"X wanted me to tell you\" — \
                     the tool adds attribution automatically.\n",
                ),
                UserRole::Regular => who.push_str(
                    "Regular permissions: you can chat with me and teach me about yourself, \
                     but can't see others or change my personality.\n",
                ),
            }
            // Make broadcast targets explicit so the LLM doesn't have to
            // infer room labels from snapshot section titles. Only shown
            // for owner / vice_owner; shown both in DM and room mode (the
            // current room is filtered out upstream in room mode).
            if u.role.at_least(UserRole::ViceOwner) && !broadcast_targets.is_empty() {
                who.push_str(&format!(
                    "Groups you can broadcast to (use `send_message(target_room=...)`): {}\n",
                    broadcast_targets.join(", ")
                ));
            }
        }
        None => who.push_str(
            "I don't know this person yet. Ask their name naturally in conversation. \
             Once they answer, **call the `user_identify(name)` tool** to remember them; \
             you won't need to ask again.\n",
        ),
    }
    sections.push(PromptSection {
        priority: 98,
        content: who,
    });

    // Always tell Folkbot how to read the unified timeline. This applies in
    // both DM and room mode — every prefix in the sliding window now
    // looks like `[time | source] speaker: content`, where source is `DM` or
    // `group "X"`. Folkbot uses the source tag to know where each message
    // happened so it doesn't conflate DM and group context.
    let timeline_note = String::from(
        "\n## How to read the message timeline\n\
         Each message in the sliding window has the format `[time | source] speaker: content`:\n\
         - source = `DM` → DM between that user and you (Folkbot) only\n\
         - source = `group \"X\"` → message in group X, visible to all group members\n\
         Your window is \"the current speaker's personal timeline\": their own DM + all groups they've joined, \
         mixed and ordered by time. That's how you can remember across conversations in real time.\n",
    );
    sections.push(PromptSection {
        priority: 95,
        content: timeline_note,
    });

    // Room context: when the turn is happening in a shared group room, list
    // who's in it and remind Folkbot this is a "public hall" — anything said is
    // visible to all members. DM-private secrets shouldn't be re-aired here.
    if let Some(r) = room {
        let mut s = String::from("\n## This conversation space\n");
        match r.label.as_deref() {
            Some(l) => s.push_str(&format!(
                "This reply will be sent into group \"{}\".\n",
                l
            )),
            None => s.push_str("This reply will be sent into a group.\n"),
        }
        if !r.members.is_empty() {
            s.push_str("Members:\n");
            for m in &r.members {
                s.push_str(&format!(
                    "- {} (role={})\n",
                    m.display_name.as_deref().unwrap_or(&m.name),
                    m.role.as_str()
                ));
            }
        }
        s.push_str(
            "\n**Important privacy boundary**: your timeline shows the current speaker's DM content, \
             but **this** reply will be seen by all group members. **Don't copy DM content \
             verbatim into a group reply**. You can form judgements based on DM content, but don't quote or paraphrase it. \
             Private promises like \"don't tell X\" you made in DM **especially must not be broken \
             in the group**. No need to prefix replies with `Folkbot:`, just talk as Folkbot.\n",
        );
        sections.push(PromptSection {
            priority: 96,
            content: s,
        });
    } else {
        // DM mode reminder: the speaker is talking 1:1 with Folkbot now, but
        // Folkbot may reference (in conversation) the speaker's own group
        // activity from the timeline. Just don't air OTHER people's DM.
        let dm_note = String::from(
            "\n## Where this reply lands\n\
             This reply only goes to the current speaker's DM (private message), nobody else can see it. \
             You can freely discuss their own group activity and their own DM details. \
             But **don't** proactively reveal what other users have told you in their own DMs — that's their private message.\n",
        );
        sections.push(PromptSection {
            priority: 96,
            content: dm_note,
        });
    }

    let h_facts: Vec<&Fact> = facts.iter().filter(|f| f.importance == Importance::H).collect();
    if !h_facts.is_empty() {
        let mut s = String::from("\n## Permanent facts ([H], must not forget)\n");
        for f in &h_facts {
            let scope = if f.user_id == 0 { " (shared)" } else { "" };
            s.push_str(&format!("- {}{}\n", f.content, scope));
        }
        sections.push(PromptSection {
            priority: 90,
            content: s,
        });
    }

    let m_facts: Vec<&Fact> = facts.iter().filter(|f| f.importance == Importance::M).collect();
    if !m_facts.is_empty() {
        let mut s = String::from("\n## Mid-term facts ([M])\n");
        for f in &m_facts {
            let scope = if f.user_id == 0 { " (shared)" } else { "" };
            s.push_str(&format!("- {}{}\n", f.content, scope));
        }
        sections.push(PromptSection {
            priority: 75,
            content: s,
        });
    }

    if !recent_summaries.is_empty() {
        let mut s = String::from("\n## Our recent context\n");
        for sum in recent_summaries {
            s.push_str(&format!(
                "\n### Daily summary ({})\n{}\n",
                fmt_date(sum.start_ts),
                sum.content
            ));
        }
        sections.push(PromptSection {
            priority: 65,
            content: s,
        });
    }

    // Cross-user awareness gated by role: only owner + vice_owner see it.
    // Regular users get nothing about other people in their system prompt.
    //
    // Show ALL other users (not just the ones with summaries) so the agent
    // doesn't wrongly claim a user "doesn't exist" when in fact they're in
    // the directory but just haven't chatted enough to have a summary yet.
    let can_see_others = current
        .map(|u| u.role.at_least(UserRole::ViceOwner))
        .unwrap_or(false);
    if can_see_others {
        let others: Vec<&User> = all_users
            .iter()
            .filter(|u| current.map(|c| c.id != u.id).unwrap_or(true))
            .collect();
        if !others.is_empty() {
            let header = if matches!(current.map(|u| u.role), Some(UserRole::Owner)) {
                "\n## Other people I know\n(you're owner, call `cross_user_transcript` when you need the raw text)\n"
            } else {
                "\n## Other people I know (brief — just the gist, no verbatim quoting)\n"
            };
            let mut s = String::from(header);
            for u in others {
                s.push_str(&format!(
                    "\n### {} (role={})\n",
                    u.name,
                    u.role.as_str()
                ));
                match (&u.last_summary, u.last_summary_ts) {
                    (Some(sum), Some(ts)) => {
                        s.push_str(&format!("(summary time {})\n{}\n", fmt_ts(ts), sum));
                    }
                    (Some(sum), None) => s.push_str(&format!("{}\n", sum)),
                    (None, _) => s.push_str(
                        "(I haven't really chatted with this person yet, no recent summary — but they are one of the people I know)\n",
                    ),
                }
            }
            sections.push(PromptSection {
                priority: 50,
                content: s,
            });
        }
    }

    let l_facts: Vec<&Fact> = facts.iter().filter(|f| f.importance == Importance::L).collect();
    if !l_facts.is_empty() {
        let mut s = String::from("\n## Trivial facts ([L])\n");
        for f in &l_facts {
            let scope = if f.user_id == 0 { " (shared)" } else { "" };
            s.push_str(&format!("- {}{}\n", f.content, scope));
        }
        sections.push(PromptSection {
            priority: 30,
            content: s,
        });
    }

    sections.sort_by(|a, b| b.priority.cmp(&a.priority));
    let mut kept = Vec::new();
    let mut used = 0;
    for s in sections {
        let cost = tokens::count(&s.content);
        if s.priority >= 95 || used + cost <= budget_tokens {
            used += cost;
            kept.push(s);
        }
    }
    kept.sort_by(|a, b| b.priority.cmp(&a.priority));
    kept
        .into_iter()
        .map(|s| s.content)
        .collect::<Vec<_>>()
        .join("\n")
}
