//! Tool-name ownership — the registration and policy gate for finding 10.
//!
//! The supervisor's name→plugin map used to last-write-win: a later MCP plugin
//! could advertise `read_file`, inherit that name's policy `allow`, and skip
//! the identity/workspace stamps reserved for `cerebro` / `apexos-tools`.
//! Names are now uniquely owned. Virtual tools belong to the supervisor.
//! Canonical plugin tools belong to a fixed plugin id. Everything else is
//! first-come.

use apexos_core::PluginId;
use std::collections::HashMap;

/// Who may advertise `name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameOwner {
    /// Intercepted by the supervisor — no plugin may register it.
    Virtual,
    /// Only this plugin id may register the name.
    Plugin(&'static str),
}

/// Why a claim was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    Virtual,
    Reserved { expected: &'static str },
    Duplicate { owner: String },
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::Virtual => {
                write!(f, "reserved virtual tool (supervisor-owned)")
            }
            ClaimError::Reserved { expected } => {
                write!(f, "reserved to plugin '{expected}'")
            }
            ClaimError::Duplicate { owner } => {
                write!(f, "already registered by '{owner}'")
            }
        }
    }
}

/// Owner of a well-known name, if any.
pub fn name_owner(name: &str) -> Option<NameOwner> {
    if VIRTUAL.contains(&name) {
        return Some(NameOwner::Virtual);
    }
    if CEREBRO.contains(&name) {
        return Some(NameOwner::Plugin("cerebro"));
    }
    if APEXOS_TOOLS.contains(&name) {
        return Some(NameOwner::Plugin("apexos-tools"));
    }
    if OCCIPITAL.contains(&name) {
        return Some(NameOwner::Plugin("occipital"));
    }
    if IMAGINARIUM.contains(&name) {
        return Some(NameOwner::Plugin("imaginarium"));
    }
    if SONUS.contains(&name) {
        return Some(NameOwner::Plugin("sonus"));
    }
    None
}

/// Accept this `(claimant, name)` against the live registry, or say why not.
pub fn claim_tool_name(
    name: &str,
    claimant: &str,
    registry: &HashMap<String, PluginId>,
) -> Result<(), ClaimError> {
    match name_owner(name) {
        Some(NameOwner::Virtual) => return Err(ClaimError::Virtual),
        Some(NameOwner::Plugin(expected)) if claimant != expected => {
            return Err(ClaimError::Reserved { expected });
        }
        Some(NameOwner::Plugin(_)) | None => {}
    }
    if let Some(owner) = registry.get(name) {
        if owner.0 != claimant {
            return Err(ClaimError::Duplicate { owner: owner.0.clone() });
        }
    }
    Ok(())
}

/// Policy / stamp helper: cerebro identity stamp applies to cerebro-owned names
/// and to any call whose live owner is the cerebro plugin.
pub fn stamps_agent_id(plugin_id: &str, tool: &str) -> bool {
    plugin_id == "cerebro" || matches!(name_owner(tool), Some(NameOwner::Plugin("cerebro")))
}

/// Workspace stamp applies to apexos-tools-owned names and to any call whose
/// live owner is that plugin.
pub fn stamps_workspace(plugin_id: &str, tool: &str) -> bool {
    plugin_id == "apexos-tools"
        || matches!(name_owner(tool), Some(NameOwner::Plugin("apexos-tools")))
}

/// True when a reserved name is being invoked by someone other than its owner.
/// Virtual tools have no plugin owner (supervisor handles them). Unreserved
/// names never trip this.
pub fn stolen_allowlist(tool: &str, plugin: Option<&str>) -> bool {
    match name_owner(tool) {
        Some(NameOwner::Virtual) => plugin.is_some(),
        Some(NameOwner::Plugin(expected)) => match plugin {
            Some(p) => p != expected,
            None => false,
        },
        None => false,
    }
}

const VIRTUAL: &[&str] = &[
    "agent_spawn",
    "apply_daemon_update",
    "bootstrap_node",
    "cancel_schedule",
    "cancel_wakeup",
    "convene_council",
    "courier_cancel",
    "courier_queue",
    "courier_status",
    "goal_cancel",
    "goal_create",
    "goal_resume",
    "goal_step",
    "list_goals",
    "list_mesh_peers",
    "list_schedules",
    "list_wakeups",
    "list_workers",
    "mandala_close",
    "mandala_create",
    "mandala_status",
    "mesh_capabilities",
    "mesh_file_send",
    "mesh_memory_send",
    "mesh_procedure_send",
    "mesh_recall",
    "propose_evolution",
    "query_event_log",
    "read_soul_md",
    "rollback_evolution",
    "schedule_task",
    "schedule_wakeup",
    "send_to_agent",
    "soul_rehearse",
    "task_fanout",
    "vast_destroy",
    "vast_launch",
    "vast_list_recipes",
    "vast_status",
    "worker_cancel",
    "worker_report",
];

const CEREBRO: &[&str] = &[
    "activation_at_risk",
    "activation_curve",
    "activation_heatmap",
    "associate",
    "audit_summary",
    "bulk_delete",
    "check_inbox",
    "check_near_duplicates",
    "cognitive_bootstrap",
    "common_neighbors",
    "cortex_stats",
    "create_schema",
    "delete_memory",
    "delete_tag",
    "describe_image",
    "dream_run",
    "dream_status",
    "emotional_summary",
    "episode_add_step",
    "episode_end",
    "episode_start",
    "export_memories",
    "find_by_tags",
    "find_matching_schemas",
    "find_path",
    "find_relevant_procedures",
    "get_episode",
    "get_episode_memories",
    "get_memory",
    "get_memory_versions",
    "get_schema_sources",
    "get_thread_memories",
    "ingest_file",
    "list_agents",
    "list_deleted",
    "list_episodes",
    "list_intentions",
    "list_procedures",
    "list_schemas",
    "list_tags",
    "list_threads",
    "memory_graph_stats",
    "memory_health",
    "memory_neighbors",
    "memory_search",
    "memory_store",
    "merge_tags",
    "prune_thread",
    "purge_all_deleted",
    "purge_memory",
    "query_audit",
    "recall",
    "record_procedure_outcome",
    "register_agent",
    "remember",
    "rename_tag",
    "resolve_intention",
    "restore_memory",
    "restore_version",
    "search_vision",
    "send_message",
    "session_recall",
    "session_save",
    "share_memory",
    "store_intention",
    "store_procedure",
    "update_memory",
];

const APEXOS_TOOLS: &[&str] = &[
    "audio_analyze",
    "audio_clean",
    "audio_normalize",
    "audio_peak_limit",
    "audio_trim",
    "audio_trim_silence",
    "camera_capture",
    "cpu_temp",
    "create_dir",
    "delete_path",
    "disk_usage",
    "display_face",
    "eject_media",
    "git_branch",
    "git_checkout",
    "git_commit",
    "git_diff",
    "git_init",
    "git_log",
    "git_merge",
    "git_push",
    "git_reset",
    "git_status",
    "git_worktree",
    "gpio_info",
    "gpio_pulse",
    "gpio_pwm",
    "gpio_read",
    "gpio_servo",
    "gpio_write",
    "http_fetch",
    "list_dir",
    "memory_info",
    "notes_append",
    "notes_list",
    "notes_read",
    "notify",
    "read_file",
    "run_command",
    "screenshot_mirror",
    "sketch_draw",
    "sketch_snapshot",
    "ui_arrange",
    "ui_close",
    "ui_focus",
    "ui_open",
    "ui_query",
    "ui_reflex",
    "ui_theme",
    "uptime",
    "write_file",
];

const OCCIPITAL: &[&str] = &[
    "web_click",
    "web_distill",
    "web_dom",
    "web_fetch",
    "web_forget",
    "web_recall",
    "web_related",
    "web_save",
    "web_search",
    "web_submit",
];

const IMAGINARIUM: &[&str] = &[
    "imaginarium_craft_video",
    "imaginarium_estimate",
    "imaginarium_image_edit",
    "imaginarium_image_generate",
    "imaginarium_job_status",
    "imaginarium_job_wait",
    "imaginarium_jobs_list",
    "imaginarium_models",
    "imaginarium_video_edit",
    "imaginarium_video_extend",
    "imaginarium_video_generate",
];

const SONUS: &[&str] = &[
    "check_status",
    "download_track",
    "generate_song",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> HashMap<String, PluginId> {
        HashMap::new()
    }

    #[test]
    fn virtual_names_cannot_be_claimed() {
        assert_eq!(
            claim_tool_name("propose_evolution", "evil", &empty()),
            Err(ClaimError::Virtual)
        );
        assert_eq!(
            claim_tool_name("goal_create", "apexos-tools", &empty()),
            Err(ClaimError::Virtual)
        );
    }

    #[test]
    fn canonical_names_only_match_their_plugin() {
        assert!(claim_tool_name("read_file", "apexos-tools", &empty()).is_ok());
        assert!(claim_tool_name("remember", "cerebro", &empty()).is_ok());
        assert_eq!(
            claim_tool_name("read_file", "evil", &empty()),
            Err(ClaimError::Reserved {
                expected: "apexos-tools"
            })
        );
        assert_eq!(
            claim_tool_name("remember", "evil", &empty()),
            Err(ClaimError::Reserved {
                expected: "cerebro"
            })
        );
    }

    #[test]
    fn first_unreserved_name_wins() {
        let mut reg = empty();
        reg.insert("custom_tool".into(), PluginId("alice".into()));
        assert!(claim_tool_name("custom_tool", "alice", &reg).is_ok());
        assert_eq!(
            claim_tool_name("custom_tool", "bob", &reg),
            Err(ClaimError::Duplicate {
                owner: "alice".into()
            })
        );
    }

    #[test]
    fn stolen_allowlist_blocks_wrong_owner() {
        assert!(stolen_allowlist("read_file", Some("evil")));
        assert!(!stolen_allowlist("read_file", Some("apexos-tools")));
        assert!(!stolen_allowlist("read_file", None));
        assert!(!stolen_allowlist("propose_evolution", None));
        assert!(stolen_allowlist("propose_evolution", Some("evil")));
        assert!(!stolen_allowlist("custom_tool", Some("anyone")));
    }

    #[test]
    fn stamps_follow_the_name_not_just_the_id_string() {
        assert!(stamps_workspace("apexos-tools", "read_file"));
        assert!(stamps_workspace("evil", "read_file"));
        assert!(!stamps_workspace("evil", "custom_tool"));
        assert!(stamps_agent_id("cerebro", "remember"));
        assert!(stamps_agent_id("evil", "remember"));
        assert!(!stamps_agent_id("evil", "custom_tool"));
    }
}
