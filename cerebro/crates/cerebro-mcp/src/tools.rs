use serde_json::{json, Value};

/// Tool schema registry — 63 tools mirroring the Python MCP server.
/// Descriptions are verbatim from the Python mcp_server.py (agent-facing strings).
/// Step 8: remember, recall, associate, get_memory have full schemas.
/// Step 9: remaining tools will be filled in.
pub fn all_tool_schemas() -> Vec<Value> {
    TOOL_NAMES.iter().map(|&name| tool_schema(name)).collect()
}

fn tool_schema(name: &str) -> Value {
    match name {
        "remember" => json!({
            "name": "remember",
            "description": "Save information to long-term memory. Automatically detects duplicates, categorizes the content, and connects it to related memories. Use this to store facts, decisions, or anything worth remembering.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":     { "type": "string", "description": "The memory content to store" },
                    "memory_type": {
                        "type": "string",
                        "enum": ["episodic","semantic","procedural","affective","prospective","schematic"],
                        "description": "Memory type (auto-classified if omitted)"
                    },
                    "tags":     {
                        "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}],
                        "description": "Tags for categorization"
                    },
                    "salience": { "type": "number", "description": "Importance 0-1 (auto-estimated if omitted)" },
                    "agent_id": { "type": "string", "description": "Agent storing this memory" },
                    "visibility": {
                        "type": "string",
                        "enum": ["private","shared","thread"],
                        "description": "Who can see this memory"
                    }
                },
                "required": ["content"]
            }
        }),

        "recall" => json!({
            "name": "recall",
            "description": "Search your memories by meaning, not just keywords. Returns the most relevant memories ranked by relevance, importance, and recency.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":    { "type": "string", "description": "Search query text" },
                    "top_k":   { "type": "integer", "description": "Max results to return (default: 10)" },
                    "agent_id": { "type": "string", "description": "Filter to this agent's memories" },
                    "visibility": { "type": "string", "enum": ["shared"], "description": "Restrict to shared-visibility memories ONLY (the federation scope; narrower than any agent scope)" }
                },
                "required": ["query"]
            }
        }),

        "associate" => json!({
            "name": "associate",
            "description": "Create a typed link between two existing memories. Strengthens the association graph for spreading activation during recall.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source_id": { "type": "string", "description": "ID of the source memory" },
                    "target_id": { "type": "string", "description": "ID of the target memory" },
                    "link_type": {
                        "type": "string",
                        "enum": ["temporal","causal","semantic","affective","contextual","contradicts","supports","derived_from","part_of"],
                        "description": "Relationship type (default: semantic)"
                    },
                    "weight": { "type": "number", "description": "Link strength 0-1 (default: 0.5)" },
                    "agent_id": { "type": "string", "description": "Agent creating this link" }
                },
                "required": ["source_id","target_id"]
            }
        }),

        "get_memory" => json!({
            "name": "get_memory",
            "description": "Retrieve a specific memory by ID.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string", "description": "The memory UUID" },
                    "agent_id":  { "type": "string", "description": "Agent scope for access control" }
                },
                "required": ["memory_id"]
            }
        }),

        "delete_memory" => json!({
            "name": "delete_memory",
            "description": "Soft-delete a memory (recoverable via restore_memory).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string", "description": "Memory UUID to delete" },
                    "agent_id":  { "type": "string" }
                },
                "required": ["memory_id"]
            }
        }),

        "update_memory" => json!({
            "name": "update_memory",
            "description": "Update fields of an existing memory. Only provided fields are changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string", "description": "Memory UUID to update" },
                    "content":   { "type": "string" },
                    "tags":      { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}] },
                    "salience":  { "type": "number" },
                    "agent_id":  { "type": "string" }
                },
                "required": ["memory_id"]
            }
        }),

        "memory_store" => json!({
            "name": "memory_store",
            "description": "Save information to memory (alias for 'remember').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":   { "type": "string" },
                    "agent_id":  { "type": "string" },
                    "tags":      { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}] }
                },
                "required": ["content"]
            }
        }),

        "memory_search" => json!({
            "name": "memory_search",
            "description": "Search memories by meaning (alias for 'recall').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":    { "type": "string" },
                    "top_k":   { "type": "integer" },
                    "agent_id": { "type": "string" }
                },
                "required": ["query"]
            }
        }),

        "memory_neighbors" => json!({
            "name": "memory_neighbors",
            "description": "Return all directly linked memories of a given memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string" },
                    "agent_id":  { "type": "string" }
                },
                "required": ["memory_id"]
            }
        }),

        "find_path" => json!({
            "name": "find_path",
            "description": "Find the shortest directed path between two memories in the association graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source_id": { "type": "string" },
                    "target_id": { "type": "string" },
                    "agent_id":  { "type": "string" }
                },
                "required": ["source_id","target_id"]
            }
        }),

        "common_neighbors" => json!({
            "name": "common_neighbors",
            "description": "Find memories directly linked to both of two given memories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id_a": { "type": "string" },
                    "memory_id_b": { "type": "string" },
                    "agent_id":    { "type": "string" }
                },
                "required": ["memory_id_a","memory_id_b"]
            }
        }),

        "cortex_stats" => json!({
            "name": "cortex_stats",
            "description": "Return aggregate statistics: total memories, deleted, links, counts by type.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),

        "memory_graph_stats" => json!({
            "name": "memory_graph_stats",
            "description": "Return the in-memory association graph node and edge counts.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),

        "session_save" => json!({
            "name": "session_save",
            "description": "Save a session summary to long-term episodic memory with priority and type tags. Used to create searchable session notes for FORGE and other agents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":      { "type": "string", "description": "Session summary content" },
                    "priority":     { "type": "string", "enum": ["LOW","MEDIUM","HIGH","CRITICAL"], "description": "Priority tag (default: MEDIUM; case-insensitive)" },
                    "session_type": { "type": "string", "description": "Session type tag e.g. technical, planning (default: general)" },
                    "salience":     { "type": "number" },
                    "agent_id":     { "type": "string" }
                },
                "required": ["content"]
            }
        }),

        "session_recall" => json!({
            "name": "session_recall",
            "description": "Recall previously saved session notes. Filters to memories tagged session_note, optionally by priority or session_type.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":        { "type": "string" },
                    "top_k":        { "type": "integer" },
                    "priority":     { "type": "string" },
                    "session_type": { "type": "string" },
                    "agent_id":     { "type": "string" }
                },
                "required": ["query"]
            }
        }),

        "list_deleted" => json!({
            "name": "list_deleted",
            "description": "List soft-deleted memories that can be restored.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit":    { "type": "integer", "description": "Max results (default: 50)" },
                    "agent_id": { "type": "string" }
                },
                "required": []
            }
        }),

        "restore_memory" => json!({
            "name": "restore_memory",
            "description": "Restore a soft-deleted memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string" }
                },
                "required": ["memory_id"]
            }
        }),

        "purge_memory" => json!({
            "name": "purge_memory",
            "description": "Permanently delete a memory (irreversible). Prefer delete_memory for recoverable soft-delete.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string" }
                },
                "required": ["memory_id"]
            }
        }),

        "purge_all_deleted" => json!({
            "name": "purge_all_deleted",
            "description": "Permanently delete all soft-deleted memories (irreversible bulk purge).",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),

        "bulk_delete" => json!({
            "name": "bulk_delete",
            "description": "Soft-delete multiple memories at once.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_ids": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["memory_ids"]
            }
        }),

        "export_memories" => json!({
            "name": "export_memories",
            "description": "Export memories as JSON. Optionally filter by type.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_type": { "type": "string", "enum": ["episodic","semantic","procedural","affective","prospective","schematic"] },
                    "limit":       { "type": "integer", "description": "Max results (default: 1000)" },
                    "agent_id":    { "type": "string" }
                },
                "required": []
            }
        }),

        "register_agent" => json!({
            "name": "register_agent",
            "description": "Register an agent in the agent registry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name":        { "type": "string" },
                    "agent_id":    { "type": "string", "description": "Agent ID (auto-generated if omitted)" },
                    "description": { "type": "string" },
                    "metadata":    { "type": "object" }
                },
                "required": ["name"]
            }
        }),

        "list_agents" => json!({
            "name": "list_agents",
            "description": "List all registered agents.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),

        "share_memory" => json!({
            "name": "share_memory",
            "description": "Make a memory shared (globally visible) or transfer it to a specific agent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id":       { "type": "string" },
                    "target_agent_id": { "type": "string", "description": "If omitted, makes globally shared" }
                },
                "required": ["memory_id"]
            }
        }),

        "send_message" => json!({
            "name": "send_message",
            "description": "Send a message to another agent by storing a memory tagged with to:{agent} and from:{agent}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":      { "type": "string" },
                    "to_agent_id":   { "type": "string" },
                    "from_agent_id": { "type": "string" },
                    "thread_id":     { "type": "string" },
                    "agent_id":      { "type": "string" }
                },
                "required": ["content","to_agent_id"]
            }
        }),

        "check_inbox" => json!({
            "name": "check_inbox",
            "description": "Check messages sent to a specific agent (memories tagged to:{agent_id}).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "limit":    { "type": "integer", "description": "Max results (default: 20)" }
                },
                "required": ["agent_id"]
            }
        }),

        "list_threads" => json!({
            "name": "list_threads",
            "description": "List distinct conversation thread IDs that have memories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" }
                },
                "required": []
            }
        }),

        "get_thread_memories" => json!({
            "name": "get_thread_memories",
            "description": "Get all memories belonging to a specific conversation thread.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" },
                    "agent_id":  { "type": "string" }
                },
                "required": ["thread_id"]
            }
        }),

        "prune_thread" => json!({
            "name": "prune_thread",
            "description": "Soft-delete all memories in a conversation thread.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" }
                },
                "required": ["thread_id"]
            }
        }),

        "list_tags" => json!({
            "name": "list_tags",
            "description": "List all tags used across memories with their counts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" }
                },
                "required": []
            }
        }),

        "find_by_tags" => json!({
            "name": "find_by_tags",
            "description": "Find memories carrying EVERY given tag (exact match, AND). Precise where recall is fuzzy — use it for provenance queries (e.g. tags [\"from:apex1\"] lists everything a peer sent; add \"origin:<id>\" to find one specific federated import).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tags":     { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "Tag(s) that must ALL be present" },
                    "limit":    { "type": "integer", "description": "Max results (default: 20)" },
                    "agent_id": { "type": "string" }
                },
                "required": ["tags"]
            }
        }),

        "delete_tag" => json!({
            "name": "delete_tag",
            "description": "Remove a tag from all memories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tag": { "type": "string" }
                },
                "required": ["tag"]
            }
        }),

        "rename_tag" => json!({
            "name": "rename_tag",
            "description": "Rename a tag across all memories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "old_tag": { "type": "string" },
                    "new_tag": { "type": "string" }
                },
                "required": ["old_tag","new_tag"]
            }
        }),

        "merge_tags" => json!({
            "name": "merge_tags",
            "description": "Merge source_tag into target_tag across all memories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source_tag": { "type": "string" },
                    "target_tag": { "type": "string" }
                },
                "required": ["source_tag","target_tag"]
            }
        }),

        "emotional_summary" => json!({
            "name": "emotional_summary",
            "description": "Summarise emotional valence distribution across memories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" }
                },
                "required": []
            }
        }),

        "activation_at_risk" => json!({
            "name": "activation_at_risk",
            "description": "Return memories whose FSRS retrievability has dropped below a threshold.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "threshold": { "type": "number", "description": "Retrievability threshold 0-1 (default: 0.7)" },
                    "limit":     { "type": "integer", "description": "Max results (default: 20)" },
                    "agent_id":  { "type": "string" }
                },
                "required": []
            }
        }),

        "memory_health" => json!({
            "name": "memory_health",
            "description": "Return overall memory health metrics: total, deleted, avg salience, avg stability, by-type breakdown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" }
                },
                "required": []
            }
        }),

        "activation_curve" => json!({
            "name": "activation_curve",
            "description": "Return the access history and FSRS state for a specific memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string" },
                    "agent_id":  { "type": "string" }
                },
                "required": ["memory_id"]
            }
        }),

        "activation_heatmap" => json!({
            "name": "activation_heatmap",
            "description": "Return memory creation counts grouped by type and month.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" }
                },
                "required": []
            }
        }),

        "check_near_duplicates" => json!({
            "name": "check_near_duplicates",
            "description": "Find pairs of memories with cosine similarity above a threshold.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "threshold": { "type": "number", "description": "Similarity threshold 0-1 (default: 0.9)" },
                    "limit":     { "type": "integer", "description": "Number of recent memories to scan (default: 50)" },
                    "agent_id":  { "type": "string" }
                },
                "required": []
            }
        }),

        "episode_start" => json!({
            "name": "episode_start",
            "description": "Begin a new episode (a named sequence of steps).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title":     { "type": "string" },
                    "agent_id":  { "type": "string" },
                    "thread_id": { "type": "string" }
                },
                "required": []
            }
        }),

        "episode_add_step" => json!({
            "name": "episode_add_step",
            "description": "Append a step to an episode, optionally linking a memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "episode_id":  { "type": "string" },
                    "step_index":  { "type": "integer" },
                    "description": { "type": "string" },
                    "memory_id":   { "type": "string" }
                },
                "required": ["episode_id","description"]
            }
        }),

        "episode_end" => json!({
            "name": "episode_end",
            "description": "Mark an episode as complete with an optional summary.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "episode_id": { "type": "string" },
                    "summary":    { "type": "string" }
                },
                "required": ["episode_id"]
            }
        }),

        "get_episode" => json!({
            "name": "get_episode",
            "description": "Retrieve a full episode including its steps.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "episode_id": { "type": "string" }
                },
                "required": ["episode_id"]
            }
        }),

        "list_episodes" => json!({
            "name": "list_episodes",
            "description": "List episodes, optionally filtered by agent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "limit":    { "type": "integer", "description": "Max results (default: 20)" }
                },
                "required": []
            }
        }),

        "get_episode_memories" => json!({
            "name": "get_episode_memories",
            "description": "Get all memories referenced by an episode.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "episode_id": { "type": "string" },
                    "agent_id":   { "type": "string" }
                },
                "required": ["episode_id"]
            }
        }),

        "audit_summary" => json!({
            "name": "audit_summary",
            "description": "Summarise audit log events by action type.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),

        "query_audit" => json!({
            "name": "query_audit",
            "description": "Query audit log entries, optionally filtered by agent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit":    { "type": "integer", "description": "Max results (default: 50)" },
                    "agent_id": { "type": "string" }
                },
                "required": []
            }
        }),

        "store_intention" => json!({
            "name": "store_intention",
            "description": "Save a TODO or reminder for future action. The system will surface it when relevant.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":  { "type": "string", "description": "The TODO or reminder content" },
                    "tags":     { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "Tags for categorization" },
                    "agent_id": { "type": "string", "description": "Agent storing this intention" },
                    "salience": { "type": "number", "description": "Importance 0-1 (default: 0.7)" }
                },
                "required": ["content"]
            }
        }),

        "list_intentions" => json!({
            "name": "list_intentions",
            "description": "List pending TODOs and reminders that have not been resolved.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id":    { "type": "string", "description": "Filter by agent" },
                    "min_salience":{ "type": "number", "description": "Minimum importance threshold (default: 0.3)" },
                    "limit":       { "type": "integer", "description": "Max results (default: 50)" }
                },
                "required": []
            }
        }),

        "resolve_intention" => json!({
            "name": "resolve_intention",
            "description": "Mark a TODO or reminder as done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string", "description": "Memory ID of the intention to resolve" },
                    "agent_id":  { "type": "string", "description": "Agent ID for access check" }
                },
                "required": ["memory_id"]
            }
        }),

        "store_procedure" => json!({
            "name": "store_procedure",
            "description": "Store a workflow, strategy, or how-to guide. These are recalled when you need instructions for a task.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":      { "type": "string", "description": "The workflow or how-to content" },
                    "tags":         { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "Tags for categorization" },
                    "derived_from": { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "IDs of memories this procedure is derived from" },
                    "agent_id":     { "type": "string", "description": "Agent storing this procedure" }
                },
                "required": ["content"]
            }
        }),

        "list_procedures" => json!({
            "name": "list_procedures",
            "description": "List all stored workflows, strategies, and how-to guides.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id":    { "type": "string", "description": "Filter by agent" },
                    "min_salience":{ "type": "number", "description": "Minimum importance threshold (default: 0.0)" },
                    "limit":       { "type": "integer", "description": "Max results (default: 50)" }
                },
                "required": []
            }
        }),

        "find_relevant_procedures" => json!({
            "name": "find_relevant_procedures",
            "description": "Find workflows and how-to guides matching given tags or concepts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tags":     { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "Tags to match" },
                    "concepts": { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "Concepts to match" },
                    "limit":    { "type": "integer", "description": "Max results (default: 5)" },
                    "agent_id": { "type": "string", "description": "Agent scope" }
                },
                "required": []
            }
        }),

        "record_procedure_outcome" => json!({
            "name": "record_procedure_outcome",
            "description": "Record whether a procedure worked or failed. This improves future procedure recommendations.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "procedure_id": { "type": "string", "description": "Procedure memory ID" },
                    "success":      { "type": "boolean", "description": "Whether the procedure succeeded" },
                    "agent_id":     { "type": "string", "description": "Agent ID for access check" }
                },
                "required": ["procedure_id", "success"]
            }
        }),

        "create_schema" => json!({
            "name": "create_schema",
            "description": "Create a general pattern or principle derived from multiple memories. Useful for capturing recurring themes or lessons learned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content":    { "type": "string", "description": "The pattern or principle to record" },
                    "source_ids": { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "IDs of the memories this pattern is derived from" },
                    "tags":       { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "Tags for categorization" },
                    "agent_id":   { "type": "string", "description": "Agent creating this schema" },
                    "salience":   { "type": "number", "description": "Importance 0-1 (default: 0.7)" }
                },
                "required": ["content", "source_ids"]
            }
        }),

        "list_schemas" => json!({
            "name": "list_schemas",
            "description": "List all stored patterns and principles.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "Filter by agent" },
                    "limit":    { "type": "integer", "description": "Max results (default: 50)" }
                },
                "required": []
            }
        }),

        "find_matching_schemas" => json!({
            "name": "find_matching_schemas",
            "description": "Find patterns and principles matching given tags or concepts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tags":     { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "Tags to match" },
                    "concepts": { "anyOf": [{"type":"array","items":{"type":"string"}},{"type":"string"}], "description": "Concepts to match" },
                    "limit":    { "type": "integer", "description": "Max results (default: 5)" },
                    "agent_id": { "type": "string", "description": "Agent scope" }
                },
                "required": []
            }
        }),

        "get_schema_sources" => json!({
            "name": "get_schema_sources",
            "description": "Get the original memories that a pattern or principle was derived from.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "schema_id": { "type": "string", "description": "Schema memory ID" },
                    "agent_id":  { "type": "string", "description": "Agent ID for access check" }
                },
                "required": ["schema_id"]
            }
        }),

        "get_memory_versions" => json!({
            "name": "get_memory_versions",
            "description": "Get version history for a memory. Each content change creates a snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string", "description": "Memory ID" },
                    "limit":     { "type": "integer", "description": "Max versions to return (default: 10)" }
                },
                "required": ["memory_id"]
            }
        }),

        "restore_version" => json!({
            "name": "restore_version",
            "description": "Restore a memory to a previous version snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "version_id": { "type": "integer", "description": "Version row ID from get_memory_versions" },
                    "agent_id":   { "type": "string", "description": "Agent ID for access check" }
                },
                "required": ["version_id"]
            }
        }),

        "dream_run" => json!({
            "name": "dream_run",
            "description": "Run a full 6-phase memory consolidation cycle: SWS replay (Hebbian link strengthening), pattern extraction (procedural memory formation), schema formation (abstract principle generation), emotional reprocessing, pruning of stale memories, and REM recombination (unexpected semantic connections). LLM-assisted phases skip gracefully when no API key is configured.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id":      { "type": "string", "description": "Scope consolidation to a specific agent's memories" },
                    "max_llm_calls": { "type": "integer", "description": "Cap on LLM API calls (default 20, max 20)", "default": 20 }
                },
                "required": []
            }
        }),

        "dream_status" => json!({
            "name": "dream_status",
            "description": "Return the most recent dream consolidation report, or {\"status\": \"no_cycles_run\"} if no cycle has run yet.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),

        "cognitive_bootstrap" => json!({
            "name": "cognitive_bootstrap",
            "description": "Assemble a token-budgeted priming block from live memory state — open intentions, recent session summaries, and query-relevant procedures and memories. Step-0 of session boot: one call replaces the multi-tool orient (session_recall + list_intentions + find_relevant_procedures + recall).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":      { "type": "string", "description": "Current task or last-known context — drives which procedures/memories surface" },
                    "mode":       { "type": "string", "description": "Token budget: minimal (1000) | standard (2000) | full (4500). Default: standard" },
                    "max_tokens": { "type": "integer", "description": "Hard cap on the priming block (only tightens the mode budget; default 2000)" },
                    "agent_id":   { "type": "string", "description": "Scope the priming to this agent's memories" }
                },
                "required": ["query"]
            }
        }),

        "describe_image" => json!({
            "name": "describe_image",
            "description": "Caption an image with a vision model and (optionally) store the caption as a memory — closing the vision→memory loop. Backend is tiered: a local/LAN Ollama VLM, falling back to an external API. Pass a workspace `path` OR inline `b64` (+ `media_type`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":        { "type": "string", "description": "Path to an image file (e.g. a screenshot_mirror / camera_capture / sketch_snapshot output)" },
                    "b64":         { "type": "string", "description": "Base64-encoded image data (alternative to path; for images not on disk)" },
                    "media_type":  { "type": "string", "description": "MIME type for b64 input (image/png|jpeg|gif|webp); sniffed from the data when omitted" },
                    "prompt":      { "type": "string", "description": "What to focus on; defaults to a general detailed caption for search" },
                    "remember":    { "type": "boolean", "description": "If true, store the caption as a memory (tagged `vision`) and return its memory_id. Default false" },
                    "memory_type": { "type": "string", "description": "Memory type when remember=true (episodic|semantic|…); default episodic" },
                    "tags":        { "type": "array", "items": { "type": "string" }, "description": "Extra tags for the stored memory (when remember=true)" },
                    "agent_id":    { "type": "string", "description": "Scope the stored memory to this agent" }
                },
                "required": []
            }
        }),

        "search_vision" => json!({
            "name": "search_vision",
            "description": "Visually recall stored images — the read half of the vision loop (describe_image with remember:true is the write half). Rank your remembered images by a `query` (a text description; CLIP text→image) OR by an example image (`path`/`b64`; image→image similarity). Returns the matching caption memories with the source image_path (when available) so you can re-view them. On a node with image embeddings disabled (Nano tier) it falls back to keyword/semantic recall over the captions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":      { "type": "string", "description": "Text describing what to find (e.g. 'a red bicycle by a door')" },
                    "path":       { "type": "string", "description": "Instead of text: a workspace image path to find visually-similar images" },
                    "b64":        { "type": "string", "description": "Instead of text: an inline base64 image to match against" },
                    "media_type": { "type": "string", "description": "Media type hint for `b64` (e.g. image/jpeg)" },
                    "k":          { "type": "integer", "description": "Max results (default 5, max 50)" },
                    "agent_id":   { "type": "string", "description": "Scope the search to this agent's memories" }
                },
                "required": []
            }
        }),

        // Deferred Tier-7 tools (ingest_file). Advertised for surface parity with
        // Python, but calling them returns an honest "not implemented" error (see
        // dispatch, C-RS-007).
        _ => json!({
            "name": name,
            "description": format!("(not yet implemented) {name}"),
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }),
    }
}

/// All 66 tool names (63 functional + 3 deferred Tier-7 stubs) — derived from
/// Python mcp_server.py tool registry.
pub const TOOL_NAMES: &[&str] = &[
    "remember",
    "recall",
    "get_memory",
    "update_memory",
    "delete_memory",
    "associate",
    "memory_search",
    "memory_store",
    "memory_neighbors",
    "common_neighbors",
    "find_path",
    "check_near_duplicates",
    "session_save",
    "session_recall",
    "get_thread_memories",
    "prune_thread",
    "episode_start",
    "episode_add_step",
    "episode_end",
    "get_episode",
    "get_episode_memories",
    "list_episodes",
    "dream_run",
    "dream_status",
    "store_intention",
    "list_intentions",
    "resolve_intention",
    "store_procedure",
    "list_procedures",
    "find_relevant_procedures",
    "record_procedure_outcome",
    "emotional_summary",
    "activation_curve",
    "activation_heatmap",
    "activation_at_risk",
    "memory_health",
    "cortex_stats",
    "memory_graph_stats",
    "audit_summary",
    "query_audit",
    "list_tags",
    "find_by_tags",
    "delete_tag",
    "rename_tag",
    "merge_tags",
    "create_schema",
    "list_schemas",
    "find_matching_schemas",
    "get_schema_sources",
    "register_agent",
    "list_agents",
    "share_memory",
    "send_message",
    "check_inbox",
    "list_threads",
    "cognitive_bootstrap",
    "ingest_file",
    "describe_image",
    "search_vision",
    "export_memories",
    "list_deleted",
    "restore_memory",
    "purge_memory",
    "bulk_delete",
    "purge_all_deleted",
    "get_memory_versions",
    "restore_version",
];
